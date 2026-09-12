//! F1: session + tool-call tracking for `tcr status --json`'s `sessions` array.
//!
//! Design and measurements: `docs/design/panel-tabs.md`; the bridge that specced this is
//! `data/plans/sessions-wire-bridge.md`.
//!
//! Two halves, deliberately separate:
//!
//! - Pure parsing (this module): given a request body already read into memory, pull out the
//!   `metadata.user_id`-embedded `session_id` and, from the LAST TWO entries of `messages`
//!   only, any `tool_use` (assistant) and `tool_result` (user) blocks. Never deserializes the
//!   whole conversation history into owned [`serde_json::Value`]s — `messages` is read
//!   borrowed as [`RawValue`] and only its tail two elements are turned into owned values.
//! - [`WireSessionTracker`]: the bounded, in-memory table one request's parsed events get
//!   folded into. No I/O, no locking — [`crate::manager::Manager`] wraps one in a `Mutex`.
//!
//! `command_head` (a Bash tool's `input.command`, or an `Agent`/`Task` tool's
//! `input.subagent_type: input.description`, capped to 120 chars) is held ONLY in this
//! in-memory table — never written to `~/.cache/teamclaude/logs` or any other file. That is
//! the same body-content-never-hits-disk rule `src/proxy.rs` states for the request log.

use serde_json::value::RawValue;
use serde_json::Value;

/// A `tool_use` block found in an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseEvent {
    pub id: String,
    pub name: Option<String>,
    /// For a `Bash` tool call, the first 120 characters of `input.command`. For an `Agent` or
    /// `Task` tool call (a running subagent), `input.description` (Claude Code's 3-5 word
    /// summary), prefixed with `input.subagent_type` when present (`"henry:coder: F4 subagents
    /// on the wire"`), same 120-char cap. `None` for any other tool. Held in memory only — see
    /// the module doc's no-body-content-on-disk rule.
    pub command_head: Option<String>,
}

/// A `tool_result` block found in a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultEvent {
    pub id: String,
    pub is_error: bool,
    /// `is_error` and the result's own content text contains "timed out" — the same string a
    /// Bash-tool timeout error carries.
    pub timed_out: bool,
}

/// Max characters kept from a Bash tool's `input.command` — see [`ToolUseEvent::command_head`].
const COMMAND_HEAD_MAX: usize = 120;

/// Parse `metadata.user_id`'s stringified JSON blob for its `session_id` — see
/// `src/proxy.rs`'s `stable_session_key` doc-comment for the blob's shape and lineage
/// guarantees. `None` on absence or a parse failure; never a panic on a hand-shaped or
/// truncated blob.
pub fn extract_session_id(user_id_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct SessionIdPeek {
        #[serde(default)]
        session_id: Option<String>,
    }
    serde_json::from_str::<SessionIdPeek>(user_id_json)
        .ok()
        .and_then(|p| p.session_id)
}

/// The minimal shape read from `messages`: only `role` and `content`, and only for the last
/// two elements of the array — see the module doc for why the rest of the history is never
/// materialized.
#[derive(serde::Deserialize)]
struct MessagePeek {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Option<Value>,
}

/// The minimal shape read from one content block.
#[derive(serde::Deserialize)]
struct BlockPeek {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    content: Option<Value>,
}

fn content_mentions_timeout(content: &Option<Value>) -> bool {
    match content {
        Some(Value::String(s)) => s.to_lowercase().contains("timed out"),
        Some(Value::Array(items)) => items.iter().any(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_lowercase().contains("timed out"))
        }),
        _ => false,
    }
}

/// Extract `tool_use` / `tool_result` blocks from the LAST TWO elements of a request's
/// `messages` array, given as a borrowed [`RawValue`] (see [`crate::proxy::SessionKeyPeek`]-
/// style peeks for the same borrowing technique). A body with no `messages`, a non-array
/// `messages`, or malformed elements yields two empty vectors rather than an error — this is
/// a best-effort observability peek, never a request-rejection path.
pub fn extract_tool_events(messages: &RawValue) -> (Vec<ToolUseEvent>, Vec<ToolResultEvent>) {
    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();

    let Ok(all) = serde_json::from_str::<Vec<&RawValue>>(messages.get()) else {
        return (tool_uses, tool_results);
    };
    let tail_start = all.len().saturating_sub(2);
    for raw in &all[tail_start..] {
        let Ok(msg) = serde_json::from_str::<MessagePeek>(raw.get()) else {
            continue;
        };
        let Some(Value::Array(blocks)) = msg.content else {
            continue;
        };
        for block in blocks {
            let Ok(b) = serde_json::from_value::<BlockPeek>(block) else {
                continue;
            };
            match (msg.role.as_str(), b.kind.as_str()) {
                ("assistant", "tool_use") => {
                    if let Some(id) = b.id {
                        let command_head = match b.name.as_deref() {
                            Some("Bash") => b
                                .input
                                .as_ref()
                                .and_then(|v| v.get("command"))
                                .and_then(Value::as_str)
                                .map(|s| s.chars().take(COMMAND_HEAD_MAX).collect()),
                            Some("Agent") | Some("Task") => b.input.as_ref().and_then(|input| {
                                let description =
                                    input.get("description").and_then(Value::as_str)?;
                                let head = match input.get("subagent_type").and_then(Value::as_str)
                                {
                                    Some(subagent_type) => {
                                        format!("{subagent_type}: {description}")
                                    }
                                    None => description.to_string(),
                                };
                                Some(head.chars().take(COMMAND_HEAD_MAX).collect())
                            }),
                            _ => None,
                        };
                        tool_uses.push(ToolUseEvent {
                            id,
                            name: b.name,
                            command_head,
                        });
                    }
                }
                ("user", "tool_result") => {
                    if let Some(id) = b.tool_use_id {
                        let is_error = b.is_error.unwrap_or(false);
                        let timed_out = is_error && content_mentions_timeout(&b.content);
                        tool_results.push(ToolResultEvent {
                            id,
                            is_error,
                            timed_out,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    (tool_uses, tool_results)
}

/// One tool call still awaiting its `tool_result`.
#[derive(Debug, Clone)]
pub struct RunningTool {
    pub tool: String,
    pub started_ms: i64,
    pub command_head: Option<String>,
}

/// One completed tool call, for the "ten slowest" list.
#[derive(Debug, Clone)]
pub struct SlowTool {
    pub tool: String,
    pub seconds: f64,
    pub command_head: Option<String>,
    pub ended_ms: i64,
}

/// Per-session tool aggregates.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub timeouts: u64,
    /// Keyed by `tool_use_id`. Capped at [`PENDING_TOOL_CAP`] per session.
    pub running: std::collections::HashMap<String, RunningTool>,
    /// Sorted by `seconds` descending, capped at [`SLOWEST_CAP`].
    pub slowest: Vec<SlowTool>,
}

/// One session's row.
#[derive(Debug, Clone, Default)]
pub struct WireSession {
    pub account: Option<String>,
    pub model: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub tools: ToolStats,
}

/// Cap on pending (running) tool-use ids per session — see the bridge.
pub const PENDING_TOOL_CAP: usize = 64;
/// Cap on tracked sessions — see the bridge.
pub const SESSION_CAP: usize = 512;
/// How many of the slowest completed tool calls each session keeps.
pub const SLOWEST_CAP: usize = 10;
/// A session unseen for this long is evicted — see the bridge ("an hour").
pub const SESSION_TTL_MS: i64 = 60 * 60 * 1000;

/// The bounded, in-memory table `tcr status --json`'s `sessions` array is read from. No I/O; a
/// caller (`Manager`) is responsible for locking.
#[derive(Debug, Default)]
pub struct WireSessionTracker {
    sessions: std::collections::HashMap<String, WireSession>,
}

impl WireSessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one request's parsed events into the table. `account`/`model` overwrite the
    /// session's last-known value when `Some`, and are left untouched when `None` (a request
    /// that could not be attributed to an account, or carried no `model` field, must not
    /// blank out a value a previous request already established).
    pub fn record_request(
        &mut self,
        session_id: &str,
        account: Option<String>,
        model: Option<String>,
        now_ms: i64,
        tool_uses: &[ToolUseEvent],
        tool_results: &[ToolResultEvent],
    ) {
        self.evict_stale(now_ms);

        let is_new = !self.sessions.contains_key(session_id);
        let entry = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| WireSession {
                first_seen_ms: now_ms,
                ..Default::default()
            });
        // The instant this session's previous response finished streaming — the "running"
        // start time for any tool_use first seen THIS request (see the bridge: "the moment
        // the proxy finished streaming the response that produced it"). A brand-new session
        // has no such instant, so its first tool_use starts "now" rather than at a fabricated
        // past time.
        let previous_response_end_ms = if is_new { now_ms } else { entry.last_seen_ms };

        entry.requests += 1;
        entry.last_seen_ms = now_ms;
        if account.is_some() {
            entry.account = account;
        }
        if model.is_some() {
            entry.model = model;
        }

        for tu in tool_uses {
            if entry.tools.running.contains_key(&tu.id) {
                continue;
            }
            if entry.tools.running.len() >= PENDING_TOOL_CAP {
                if let Some(oldest) = entry
                    .tools
                    .running
                    .iter()
                    .min_by_key(|(_, r)| r.started_ms)
                    .map(|(k, _)| k.clone())
                {
                    entry.tools.running.remove(&oldest);
                }
            }
            entry.tools.running.insert(
                tu.id.clone(),
                RunningTool {
                    tool: tu.name.clone().unwrap_or_default(),
                    started_ms: previous_response_end_ms,
                    command_head: tu.command_head.clone(),
                },
            );
        }

        for tr in tool_results {
            if let Some(running) = entry.tools.running.remove(&tr.id) {
                let seconds = (now_ms - running.started_ms).max(0) as f64 / 1000.0;
                entry.tools.calls += 1;
                if tr.is_error {
                    entry.tools.errors += 1;
                }
                if tr.timed_out {
                    entry.tools.timeouts += 1;
                }
                Self::insert_slowest(
                    &mut entry.tools.slowest,
                    SlowTool {
                        tool: running.tool,
                        seconds,
                        command_head: running.command_head,
                        ended_ms: now_ms,
                    },
                );
            }
        }

        self.evict_over_cap();
    }

    /// Add token counts learned from a response's usage — a separate call because usage is
    /// resolved later than the request (after the upstream response, sometimes after an SSE
    /// stream finishes), on a session that [`Self::record_request`] has already created. A
    /// session not (yet, or no longer) present is silently ignored — there is nothing to
    /// attribute the tokens to.
    pub fn record_usage(&mut self, session_id: &str, input: u64, output: u64, cache_read: u64) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.input_tokens += input;
            entry.output_tokens += output;
            entry.cache_read_tokens += cache_read;
        }
    }

    fn insert_slowest(slowest: &mut Vec<SlowTool>, tool: SlowTool) {
        slowest.push(tool);
        slowest.sort_by(|a, b| {
            b.seconds
                .partial_cmp(&a.seconds)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        slowest.truncate(SLOWEST_CAP);
    }

    fn evict_stale(&mut self, now_ms: i64) {
        self.sessions
            .retain(|_, s| now_ms.saturating_sub(s.last_seen_ms) <= SESSION_TTL_MS);
    }

    fn evict_over_cap(&mut self) {
        if self.sessions.len() > SESSION_CAP {
            if let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.last_seen_ms)
                .map(|(k, _)| k.clone())
            {
                self.sessions.remove(&oldest);
            }
        }
    }

    /// Every currently-live session, unordered. `now_ms` evicts stale entries first, so a
    /// session that crossed the one-hour idle line since its last write never surfaces.
    pub fn snapshot(&self, now_ms: i64) -> Vec<(String, WireSession)> {
        self.sessions
            .iter()
            .filter(|(_, s)| now_ms.saturating_sub(s.last_seen_ms) <= SESSION_TTL_MS)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, name: &str, command: Option<&str>) -> Value {
        serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": command.map_or(Value::Null, |c| serde_json::json!({"command": c})),
        })
    }

    fn agent_tool_use(
        id: &str,
        name: &str,
        description: &str,
        subagent_type: Option<&str>,
    ) -> Value {
        let mut input = serde_json::json!({"description": description});
        if let Some(st) = subagent_type {
            input["subagent_type"] = Value::String(st.to_string());
        }
        serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })
    }

    fn tool_result(id: &str, is_error: bool, text: &str) -> Value {
        serde_json::json!({
            "type": "tool_result",
            "tool_use_id": id,
            "is_error": is_error,
            "content": text,
        })
    }

    fn messages_raw(msgs: Value) -> Box<RawValue> {
        RawValue::from_string(msgs.to_string()).expect("valid json")
    }

    #[test]
    fn extract_session_id_reads_the_embedded_field() {
        let blob = r#"{"device_id":"d1","account_uuid":"a1","session_id":"sess-123"}"#;
        assert_eq!(extract_session_id(blob), Some("sess-123".to_string()));
    }

    #[test]
    fn extract_session_id_none_on_absence_or_garbage() {
        assert_eq!(extract_session_id(r#"{"device_id":"d1"}"#), None);
        assert_eq!(extract_session_id("not json"), None);
    }

    #[test]
    fn extract_tool_events_pairs_use_and_result_in_the_last_two_messages() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [tool_use("tu_1", "Bash", Some("ls -la"))]},
            {"role": "user", "content": [tool_result("tu_1", false, "ok")]},
        ]);
        let raw = messages_raw(messages);
        let (uses, results) = extract_tool_events(&raw);
        // Only the LAST TWO messages are read, so the assistant tool_use message is seen
        // (it's second-to-last) and the tool_result message (last) is seen too.
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].id, "tu_1");
        assert_eq!(uses[0].command_head.as_deref(), Some("ls -la"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "tu_1");
        assert!(!results[0].is_error);
    }

    #[test]
    fn extract_tool_events_fills_command_head_for_an_agent_tool_with_subagent_type() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [agent_tool_use(
                "tu_agent",
                "Agent",
                "F4 subagents on the wire",
                Some("henry:coder"),
            )]},
        ]);
        let raw = messages_raw(messages);
        let (uses, _results) = extract_tool_events(&raw);
        assert_eq!(uses.len(), 1);
        assert_eq!(
            uses[0].command_head.as_deref(),
            Some("henry:coder: F4 subagents on the wire")
        );
    }

    #[test]
    fn extract_tool_events_fills_command_head_for_a_task_tool_without_subagent_type() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [agent_tool_use(
                "tu_task",
                "Task",
                "review the diff",
                None,
            )]},
        ]);
        let raw = messages_raw(messages);
        let (uses, _results) = extract_tool_events(&raw);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].command_head.as_deref(), Some("review the diff"));
    }

    #[test]
    fn extract_tool_events_ignores_history_before_the_last_two() {
        let messages = serde_json::json!([
            {"role": "assistant", "content": [tool_use("stale", "Bash", Some("rm -rf /"))]},
            {"role": "user", "content": [tool_result("stale", false, "ok")]},
            {"role": "assistant", "content": "just text"},
            {"role": "user", "content": "another turn"},
        ]);
        let raw = messages_raw(messages);
        let (uses, results) = extract_tool_events(&raw);
        assert!(
            uses.is_empty(),
            "the stale tool_use is outside the last two messages"
        );
        assert!(results.is_empty());
    }

    #[test]
    fn a_tool_use_then_its_tool_result_yields_one_call_with_the_right_seconds() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_1".into(),
            name: Some("Bash".into()),
            command_head: Some("sleep 5".into()),
        }];
        // First request: the tool_use appears, no result yet.
        tracker.record_request(
            "sess-a",
            Some("alice@example.com".into()),
            Some("claude-x".into()),
            1_000,
            &uses,
            &[],
        );
        // Second request, 5500ms later: the tool_result arrives.
        let results = vec![ToolResultEvent {
            id: "tu_1".into(),
            is_error: false,
            timed_out: false,
        }];
        tracker.record_request(
            "sess-a",
            Some("alice@example.com".into()),
            Some("claude-x".into()),
            6_500,
            &[],
            &results,
        );

        let snap = tracker.snapshot(6_500);
        assert_eq!(snap.len(), 1);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.calls, 1);
        assert_eq!(session.tools.errors, 0);
        assert!(session.tools.running.is_empty());
        assert_eq!(session.tools.slowest.len(), 1);
        // Started at the PREVIOUS request's time (1_000, this session's first-ever request,
        // so "previous response end" is that request's own arrival), resolved at 6_500.
        assert_eq!(session.tools.slowest[0].seconds, 5.5);
        assert_eq!(session.tools.slowest[0].tool, "Bash");
    }

    #[test]
    fn an_unmatched_tool_use_id_stays_in_running() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_orphan".into(),
            name: Some("Read".into()),
            command_head: None,
        }];
        tracker.record_request("sess-b", None, None, 1_000, &uses, &[]);
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.calls, 0);
        assert_eq!(session.tools.running.len(), 1);
        assert!(session.tools.running.contains_key("tu_orphan"));
    }

    #[test]
    fn a_65th_pending_tool_use_evicts_the_oldest() {
        let mut tracker = WireSessionTracker::new();
        // Establish the session with a plain touch first (no tool_use), at a time distinctly
        // earlier than every subsequent one — a brand-new session's OWN first tool_use starts
        // at its own arrival time (see `record_request`'s doc), which would otherwise tie with
        // the second tool_use's start (the first request's own arrival time) and make "the
        // oldest" ambiguous between them.
        tracker.record_request("sess-c", None, None, 0, &[], &[]);
        for i in 0..PENDING_TOOL_CAP {
            let uses = vec![ToolUseEvent {
                id: format!("tu_{i}"),
                name: Some("Bash".into()),
                command_head: None,
            }];
            tracker.record_request("sess-c", None, None, 1_000 + i as i64, &uses, &[]);
        }
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        // Exactly at the cap (64 pending ids): nothing evicted yet.
        assert_eq!(session.tools.running.len(), PENDING_TOOL_CAP);
        assert!(session.tools.running.contains_key("tu_0"));

        // The 65th distinct pending id.
        let uses = vec![ToolUseEvent {
            id: "tu_overflow".into(),
            name: Some("Bash".into()),
            command_head: None,
        }];
        tracker.record_request("sess-c", None, None, 2_000, &uses, &[]);
        let snap = tracker.snapshot(3_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.running.len(), PENDING_TOOL_CAP);
        assert!(session.tools.running.contains_key("tu_overflow"));
        assert!(
            !session.tools.running.contains_key("tu_0"),
            "the oldest pending id (tu_0, started at 0) must have been evicted to make room"
        );
        assert!(
            session.tools.running.contains_key("tu_1"),
            "the next-oldest survives"
        );
    }

    #[test]
    fn a_session_unseen_for_an_hour_disappears() {
        let mut tracker = WireSessionTracker::new();
        tracker.record_request(
            "sess-d",
            Some("alice@example.com".into()),
            None,
            0,
            &[],
            &[],
        );
        assert_eq!(
            tracker.snapshot(SESSION_TTL_MS).len(),
            1,
            "exactly at the TTL boundary it is still live"
        );
        assert_eq!(
            tracker.snapshot(SESSION_TTL_MS + 1).len(),
            0,
            "one ms past the TTL it is gone"
        );
    }

    #[test]
    fn a_body_with_no_messages_changes_nothing() {
        // Modeled as: the caller found no `messages` field at all and so never calls
        // `extract_tool_events`; `record_request` with empty event slices must still update
        // request/account/model bookkeeping and touch nothing tool-related.
        let mut tracker = WireSessionTracker::new();
        tracker.record_request(
            "sess-e",
            Some("alice@example.com".into()),
            Some("claude-x".into()),
            1_000,
            &[],
            &[],
        );
        let snap = tracker.snapshot(1_000);
        let (_, session) = &snap[0];
        assert_eq!(session.requests, 1);
        assert_eq!(session.tools.calls, 0);
        assert!(session.tools.running.is_empty());
    }

    #[test]
    fn extract_tool_events_empty_on_absent_or_malformed_messages() {
        let raw = RawValue::from_string("null".to_string()).unwrap();
        let (uses, results) = extract_tool_events(&raw);
        assert!(uses.is_empty());
        assert!(results.is_empty());

        let raw = RawValue::from_string("\"not an array\"".to_string()).unwrap();
        let (uses, results) = extract_tool_events(&raw);
        assert!(uses.is_empty());
        assert!(results.is_empty());
    }

    #[test]
    fn a_timeout_error_is_counted_as_both_error_and_timeout() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_timeout".into(),
            name: Some("Bash".into()),
            command_head: Some("sleep 700".into()),
        }];
        tracker.record_request("sess-f", None, None, 1_000, &uses, &[]);
        let results = vec![ToolResultEvent {
            id: "tu_timeout".into(),
            is_error: true,
            timed_out: true,
        }];
        tracker.record_request("sess-f", None, None, 601_000, &[], &results);
        let snap = tracker.snapshot(601_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.calls, 1);
        assert_eq!(session.tools.errors, 1);
        assert_eq!(session.tools.timeouts, 1);
    }
}
