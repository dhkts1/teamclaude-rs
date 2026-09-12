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

/// How many of a tool's most recent call durations are kept for [`ToolBucket::seconds_p50`] —
/// see the bridge ("a bounded reservoir per tool for the median").
pub const TOOL_DURATION_RESERVOIR_CAP: usize = 256;

/// One tool's aggregate stats within a session — the source for `SessionToolsRow::by_tool`
/// (`crates/tcr-status-wire`). Keyed on `tool_use.name` verbatim; `Read`, `Grep`, `Glob` and
/// `Edit` are deliberately NOT merged here (the panel groups them) — see the bridge.
#[derive(Debug, Clone, Default)]
pub struct ToolBucket {
    pub calls: u64,
    pub errors: u64,
    /// Completed calls of this tool whose duration was 60 seconds or more.
    pub over_one_minute: u64,
    /// Bounded reservoir of the last [`TOOL_DURATION_RESERVOIR_CAP`] completed calls'
    /// durations (seconds), for a wall-clock-cheap running median.
    durations: std::collections::VecDeque<f64>,
}

impl ToolBucket {
    fn record(&mut self, seconds: f64, is_error: bool) {
        self.calls += 1;
        if is_error {
            self.errors += 1;
        }
        if seconds >= 60.0 {
            self.over_one_minute += 1;
        }
        self.durations.push_back(seconds);
        if self.durations.len() > TOOL_DURATION_RESERVOIR_CAP {
            self.durations.pop_front();
        }
    }

    /// The median over the retained reservoir — not a true all-time median once the
    /// reservoir has evicted older samples, which is the bounded-memory tradeoff the bridge
    /// accepts. `0.0` on a bucket with no completed calls yet.
    pub fn seconds_p50(&self) -> f64 {
        if self.durations.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = self.durations.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }
}

/// Per-session tool aggregates.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub timeouts: u64,
    /// Completed calls (any tool) whose duration was 60 seconds or more — the session-wide
    /// total; [`ToolBucket::over_one_minute`] carries the same count per tool.
    pub over_one_minute: u64,
    /// Keyed by `tool_use_id`. Capped at [`PENDING_TOOL_CAP`] per session.
    pub running: std::collections::HashMap<String, RunningTool>,
    /// Sorted by `seconds` descending, capped at [`SLOWEST_CAP`].
    pub slowest: Vec<SlowTool>,
    /// Per-tool aggregates keyed on `tool_use.name`.
    pub by_tool: std::collections::HashMap<String, ToolBucket>,
}

/// How many wall-clock minutes [`ReqPerMinuteRing`] retains.
pub const REQ_PER_MINUTE_LEN: usize = 30;

/// Requests seen in each of the last [`REQ_PER_MINUTE_LEN`] wall-clock minutes, oldest first,
/// always exactly that many entries — the source for `SessionRow::req_per_minute`
/// (`crates/tcr-status-wire`). Advanced on [`Self::record`] (a new request bumps the current
/// minute's bucket) and again, read-only, by [`Self::projected`] (`snapshot` calls this so an
/// idle session decays toward zeros instead of freezing on its last-seen minute).
#[derive(Debug, Clone)]
pub struct ReqPerMinuteRing {
    buckets: std::collections::VecDeque<u16>,
    /// The wall-clock minute number (`ms / 60_000`) the newest (last) bucket represents.
    head_minute: i64,
}

impl Default for ReqPerMinuteRing {
    fn default() -> Self {
        Self {
            buckets: std::iter::repeat_n(0, REQ_PER_MINUTE_LEN).collect(),
            head_minute: 0,
        }
    }
}

impl ReqPerMinuteRing {
    /// Shift the ring forward to `minute`, pushing a zero bucket per elapsed minute (capped
    /// at [`REQ_PER_MINUTE_LEN`] shifts, since anything beyond that clears the whole ring
    /// anyway). A `minute` at or before the current head is a no-op — this ring never runs
    /// backward.
    fn advance_to(&mut self, minute: i64) {
        let diff = minute - self.head_minute;
        if diff <= 0 {
            return;
        }
        let shift = diff.min(REQ_PER_MINUTE_LEN as i64) as usize;
        for _ in 0..shift {
            self.buckets.pop_front();
            self.buckets.push_back(0);
        }
        self.head_minute = minute;
    }

    fn record(&mut self, now_ms: i64) {
        self.advance_to(now_ms.div_euclid(60_000));
        if let Some(last) = self.buckets.back_mut() {
            *last = last.saturating_add(1);
        }
    }

    /// A read-only projection to `now_ms`, oldest first — never mutates stored state, so
    /// `snapshot` (which takes `&self`) can decay an idle session's ring for display without
    /// needing a write lock.
    pub fn projected(&self, now_ms: i64) -> Vec<u16> {
        let mut copy = self.clone();
        copy.advance_to(now_ms.div_euclid(60_000));
        copy.buckets.into_iter().collect()
    }
}

/// Model this session has never been attributed a usage record under.
const UNKNOWN_MODEL: &str = "unknown";

/// One model's raw token tally within a session — the input to pricing, kept apart from
/// [`WireSession::input_tokens`] and friends (the QUOTA counters, unchanged in meaning) so a
/// session that spans two models can be priced per-model and summed, rather than priced once
/// against whichever model happened to be current. `input` here is BASE input only (excludes
/// both cache dimensions), matching [`crate::usage::UsageRecord::input`] — never re-derive it
/// from `cache_5m + cache_1h + cache_read` here, since that is what pricing itself does.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelTokenTally {
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
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
    /// See [`ReqPerMinuteRing`].
    pub req_per_minute: ReqPerMinuteRing,
    /// Per-model raw token tallies, keyed on the model id a usage record carried (or
    /// [`UNKNOWN_MODEL`] when it carried none) — the source `crate::manager::wire_sessions`
    /// prices into `SessionRow::cost_usd`, one model's price at a time, then sums.
    pub by_model: std::collections::HashMap<String, ModelTokenTally>,
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
        entry.req_per_minute.record(now_ms);
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
                if seconds >= 60.0 {
                    entry.tools.over_one_minute += 1;
                }
                entry
                    .tools
                    .by_tool
                    .entry(running.tool.clone())
                    .or_default()
                    .record(seconds, tr.is_error);
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
    ///
    /// `quota_input` is the SAME quota-counter figure this always accumulated
    /// (`entry.input_tokens`, unchanged meaning) — `model`, `base_input`, `cache_5m` and
    /// `cache_1h` are new (wire 2): they fold into [`WireSession::by_model`] so a session that
    /// spans two models can be priced per-model and summed, rather than averaged.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &mut self,
        session_id: &str,
        model: Option<&str>,
        quota_input: u64,
        base_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read: u64,
        output: u64,
    ) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.input_tokens += quota_input;
            entry.output_tokens += output;
            entry.cache_read_tokens += cache_read;
            let tally = entry
                .by_model
                .entry(model.unwrap_or(UNKNOWN_MODEL).to_string())
                .or_default();
            tally.input += base_input;
            tally.cache_5m += cache_5m;
            tally.cache_1h += cache_1h;
            tally.cache_read += cache_read;
            tally.output += output;
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

    /// `by_tool` keeps separate buckets per tool NAME (three tools, two of which get more
    /// than one completed call), tracks each bucket's own calls/errors, and reports the
    /// median of its own reservoir — not a global median across every tool.
    #[test]
    fn by_tool_tracks_calls_errors_and_a_per_tool_median() {
        let mut tracker = WireSessionTracker::new();
        // Bash: three completed calls back-to-back on a strictly increasing clock, each
        // one's start pinned to the PREVIOUS call's own end (see `record_request`'s doc on
        // `previous_response_end_ms`) — durations 1s, 3s, 5s (median 3s), the second an error.
        let mut clock_ms = 0i64;
        for (i, (secs, is_error)) in [(1.0, false), (3.0, true), (5.0, false)]
            .into_iter()
            .enumerate()
        {
            let id = format!("bash_{i}");
            tracker.record_request(
                "sess-tools",
                None,
                None,
                clock_ms,
                &[ToolUseEvent {
                    id: id.clone(),
                    name: Some("Bash".into()),
                    command_head: None,
                }],
                &[],
            );
            clock_ms += (secs * 1000.0) as i64;
            tracker.record_request(
                "sess-tools",
                None,
                None,
                clock_ms,
                &[],
                &[ToolResultEvent {
                    id,
                    is_error,
                    timed_out: false,
                }],
            );
        }
        // Read: one completed call, 2s, no error — same rule: starts where the clock left off.
        tracker.record_request(
            "sess-tools",
            None,
            None,
            clock_ms,
            &[ToolUseEvent {
                id: "read_0".into(),
                name: Some("Read".into()),
                command_head: None,
            }],
            &[],
        );
        clock_ms += 2_000;
        tracker.record_request(
            "sess-tools",
            None,
            None,
            clock_ms,
            &[],
            &[ToolResultEvent {
                id: "read_0".into(),
                is_error: false,
                timed_out: false,
            }],
        );

        let snap = tracker.snapshot(clock_ms);
        let (_, session) = &snap[0];
        assert_eq!(
            session.tools.by_tool.len(),
            2,
            "Bash and Read stay separate buckets"
        );
        let bash = &session.tools.by_tool["Bash"];
        assert_eq!(bash.calls, 3);
        assert_eq!(bash.errors, 1);
        assert_eq!(bash.seconds_p50(), 3.0, "the middle of [1, 3, 5]");
        let read = &session.tools.by_tool["Read"];
        assert_eq!(read.calls, 1);
        assert_eq!(read.errors, 0);
        assert_eq!(read.seconds_p50(), 2.0);
    }

    /// A completed call of 60 seconds or more counts toward both the per-tool
    /// `over_one_minute` and the session-wide total — independent of `errors`/`timeouts`, so a
    /// slow-but-successful call still counts.
    #[test]
    fn over_one_minute_counts_slow_completed_calls_regardless_of_error_status() {
        let mut tracker = WireSessionTracker::new();
        tracker.record_request(
            "sess-slow",
            None,
            None,
            0,
            &[ToolUseEvent {
                id: "tu_slow".into(),
                name: Some("Bash".into()),
                command_head: Some("sleep 90".into()),
            }],
            &[],
        );
        tracker.record_request(
            "sess-slow",
            None,
            None,
            90_000,
            &[],
            &[ToolResultEvent {
                id: "tu_slow".into(),
                is_error: false,
                timed_out: false,
            }],
        );
        let snap = tracker.snapshot(90_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.over_one_minute, 1);
        assert_eq!(session.tools.by_tool["Bash"].over_one_minute, 1);
        assert_eq!(session.tools.errors, 0, "a slow call need not be an error");
    }

    /// After 31 simulated minutes of one request per minute, the ring has dropped the very
    /// first minute's count and kept exactly the last 30, oldest first.
    #[test]
    fn req_per_minute_ring_drops_the_first_minute_after_31_minutes() {
        let mut tracker = WireSessionTracker::new();
        for minute in 0..31 {
            tracker.record_request("sess-ring", None, None, minute * 60_000, &[], &[]);
        }
        let snap = tracker.snapshot(30 * 60_000);
        let (_, session) = &snap[0];
        let ring = session.req_per_minute.projected(30 * 60_000);
        assert_eq!(ring.len(), REQ_PER_MINUTE_LEN);
        assert_eq!(
            ring,
            vec![1u16; REQ_PER_MINUTE_LEN],
            "minutes 1..=30 each got exactly one request; minute 0 was pushed out"
        );
    }

    /// A session with no NEW requests for a while decays toward zeros when projected forward
    /// — the ring must not freeze on whatever it last recorded.
    #[test]
    fn req_per_minute_decays_to_zero_when_projected_past_the_last_request() {
        let mut tracker = WireSessionTracker::new();
        tracker.record_request("sess-idle", None, None, 0, &[], &[]);
        let snap = tracker.snapshot(0);
        let (_, session) = &snap[0];
        // Immediately: minute 0 shows one request.
        assert_eq!(session.req_per_minute.projected(0).last(), Some(&1));
        // 35 minutes later, with no new request, minute 0 has scrolled off entirely.
        let projected = session.req_per_minute.projected(35 * 60_000);
        assert_eq!(
            projected,
            vec![0u16; REQ_PER_MINUTE_LEN],
            "35 idle minutes is more than the 30-minute window, so every bucket is zero"
        );
    }
}
