//! F1: session + tool-call tracking for `tcr status --json`'s `sessions` array.
//!
//! Design and measurements: `docs/design/panel-tabs.md`.
//!
//! Two halves, deliberately separate:
//!
//! - Pure parsing (this module): given a request body already read into memory, pull out the
//!   `metadata.user_id`-embedded `session_id` and, from the TAIL of `messages`, any
//!   `tool_use` (assistant) and `tool_result` (user) blocks. The tail is not a fixed "last
//!   two" — Claude Code does not always end a request's history with exactly
//!   `[assistant(tool_use), user(tool_result)]`. Measured against this machine's own
//!   transcripts (`~/.claude/projects/*.jsonl`, walked via each entry's real `parentUuid`
//!   chain, so cross-thread/subagent interleaving in the same file cannot be mistaken for
//!   one thread's own history): a `tool_use` and its `tool_result` are always adjacent
//!   logical messages, but Claude Code can append further trailing `user` messages after
//!   the `tool_result` — a system-reminder is sent as its OWN message, not folded into the
//!   `tool_result`'s content array — before the next request actually goes out. So the tail
//!   walks backward from the newest message, gathering it into the scan window, until it
//!   reaches an `assistant` message (which is where the newest `tool_use` block, if any,
//!   lives) or [`TAIL_SCAN_CAP`] is hit. Never deserializes the whole conversation history
//!   into owned [`serde_json::Value`]s — `messages` is read borrowed as [`RawValue`] and
//!   only this bounded tail window is turned into owned values.
//! - [`WireSessionTracker`]: the bounded, in-memory table one request's parsed events get
//!   folded into. No I/O, no locking — [`crate::manager::Manager`] wraps one in a `Mutex`.
//!
//! A `tool_use` block reaches this table from two directions — a request's assistant message
//! ([`extract_tool_events`]) and the response the proxy just streamed (`proxy.rs`, which
//! builds its events through the shared [`tool_use_event_from_block`]) — because a Claude Code
//! request carries a `tool_use` and its `tool_result` together, so the request side alone can
//! never show a tool still running.
//!
//! `command_head` (capped to 120 chars; the per-tool shape is on
//! [`ToolUseEvent::command_head`]) is held in this in-memory table and is never written to
//! `~/.cache/teamclaude/logs` or any other log file — the same
//! body-content-never-hits-disk rule `src/proxy.rs` states for the request log.
//!
//! It DOES reach one file: the affinity-style snapshot in
//! [`crate::session_wire_persist`], which round-trips [`WireSession`] to
//! `~/.cache/teamclaude/session-wire.json`. Between #299 and 2026-09-13 both
//! [`RunningTool::command_head`] and [`SlowTool::command_head`] carried
//! `#[serde(skip_serializing)]` and that file held no command at all. Gil reversed it the
//! same day, on a measurement: after a restart, 72 of 95 restored SLOWEST TODAY rows read
//! as a bare `Bash`, because the class survived and the command did not — a slowest-today
//! list with no commands in it is junk, not a reduced-detail version of the real thing. So
//! the head persists, bounded by what it already was: 120 characters of a NORMALIZED
//! command (env prefixes, wrappers and the leading `cd` clause are stripped before it is
//! stored), a file path, or a grep pattern — never a request or response body, which this
//! table has never held. The file is written `0600` by [`crate::config::write_atomic`],
//! which both creates its temp file with that mode and normalises the destination after the
//! rename, so an operator's umask cannot loosen it.

use serde_json::value::RawValue;
use serde_json::Value;

/// A `tool_use` block found in an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseEvent {
    pub id: String,
    pub name: Option<String>,
    /// What this call is DOING, for the panel row that would otherwise print the bare tool
    /// name. Built by [`tool_use_event_from_block`], always capped at [`COMMAND_HEAD_MAX`]
    /// characters:
    ///
    /// - `Bash` — the NORMALIZED first 120 characters of `input.command`, see
    ///   [`normalize_bash_command`].
    /// - `Agent` / `Task` (a running subagent) — `input.description` (Claude Code's 3-5 word
    ///   summary), prefixed with `input.subagent_type` when present
    ///   (`"reviewer: check the wire fixtures"`).
    /// - `TaskOutput` — `TaskOutput · waiting on task <input.task_id>`.
    /// - `Read` / `Edit` / `Write` / `NotebookEdit` — the tool name and `input.file_path`
    ///   (`input.notebook_path` for a notebook).
    /// - `Grep` / `Glob` — the tool name and `input.pattern`.
    /// - `WebFetch` — the HOST of `input.url`, never its path or query.
    /// - `WebSearch` — `input.query`.
    /// - `None` for any other tool, and for any of the above whose field is absent; the panel
    ///   falls back to the tool name.
    ///
    /// Held in memory only — see the module doc's no-body-content-on-disk rule.
    pub command_head: Option<String>,
    /// A `Bash` tool call's coarse category — see [`CommandClass`]. `None` for any other
    /// tool, or when `command_head` is `None`.
    pub command_class: Option<CommandClass>,
}

/// A Bash command's coarse category, for the Tools tab's per-class rollup — mirrors
/// the operator harness's own slow-command classifier, so the wire and that harness
/// hook agree on both the rules and the names. Classified on the REDUCED command (after
/// env/wrapper/`cd`-clause stripping — see [`normalize_bash_command`]), never the raw one:
/// the hook's own measurement is that classifying a compound command by its first clause
/// hid a real defect (a slow `grep`) behind a `git` bucket for four days.
// `Serialize`/`Deserialize` because a classified command rides the session-wire
// snapshot (`src/session_wire_persist.rs`, #289): a row restored at boot has to
// carry the same class it carried while live, or the Tools tab would reclassify
// half its rows as `other` after every restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommandClass {
    /// `wait-for-line`, `merge-when-green`, or a command starting with `until`/`sleep` —
    /// never a defect, since taking long is the job.
    Wait,
    /// Any `;`, `&` or `|` left in the reduced command — a real compound, since a `cd
    /// <dir> && <clause>` already had its `cd` prefix reduced away before this check runs.
    Compound,
    /// `find`, `grep`, `fd`, `rg`, `bfs` or `du`.
    Search,
    /// `git fetch`/`push`/`pull`/`clone`/`ls-remote` — a network round trip.
    GitNet,
    /// Any other `git` subcommand.
    GitLocal,
    /// `cargo`, `pnpm`, `npm`, `yarn`, `uv`, `uvx`, `pytest`, `vitest`, `make`, `tsc`, `bun`
    /// or `cargo-q.sh`.
    Build,
    /// Everything else.
    Other,
}

impl CommandClass {
    /// The wire/panel name — kebab-case, matching `hooks/log-slow-bash.sh`'s own
    /// `class=` values verbatim, so a person reading both sees the same word.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandClass::Wait => "wait",
            CommandClass::Compound => "compound",
            CommandClass::Search => "search",
            CommandClass::GitNet => "git-net",
            CommandClass::GitLocal => "git-local",
            CommandClass::Build => "build",
            CommandClass::Other => "other",
        }
    }
}

/// Take the first whitespace-delimited token of `s`, requiring at least one whitespace
/// character AFTER it (a bare trailing token with nothing following is never "wrapped" —
/// same as the hook's `\s+` after each stripped prefix). Returns `(token, rest trimmed of
/// its leading whitespace)`.
fn take_token(s: &str) -> Option<(&str, &str)> {
    let ws = s.find(char::is_whitespace)?;
    Some((&s[..ws], s[ws..].trim_start()))
}

/// Is `token` an `IDENT=value` environment assignment? Generic — any valid shell identifier,
/// not restricted to one project's own prefix — mirroring the hook's
/// `^(FOO=bar\s+)+`.
fn is_env_assignment(token: &str) -> bool {
    let Some((ident, value)) = token.split_once('=') else {
        return false;
    };
    !ident.is_empty()
        && !value.is_empty()
        && ident
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip every leading `IDENT=value` token, in order — the hook's `^(FOO=bar\s+)+`.
fn strip_env_prefix(mut s: &str) -> &str {
    while let Some((token, rest)) = take_token(s) {
        if !is_env_assignment(token) {
            break;
        }
        s = rest;
    }
    s
}

/// Strip one leading wrapper (`timeout <n>`, `nice -n<n>` or `time`), or `None` if `s` does
/// not start with one. Mirrors the hook's `^(timeout\s+\d+\s+|nice\s+-n\d+\s+|time\s+)`.
fn strip_one_wrapper(s: &str) -> Option<&str> {
    let (first, rest) = take_token(s)?;
    match first {
        "timeout" => {
            let (arg, rest2) = take_token(rest)?;
            (!arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit())).then_some(rest2)
        }
        "time" => Some(rest),
        "nice" => {
            let (flag, rest2) = take_token(rest)?;
            let digits = flag.strip_prefix("-n")?;
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then_some(rest2)
        }
        _ => None,
    }
}

/// Strip every leading wrapper, in order — the hook's `(...)+` repetition.
fn strip_wrappers(mut s: &str) -> &str {
    while let Some(rest) = strip_one_wrapper(s) {
        s = rest;
    }
    s
}

/// Reduce `cd <dir> && <clause>` (or `cd <dir>; <clause>`) to just `<clause>` — the hook's
/// own measurement: 18% of its "compound" rows were exactly this shape, a free `cd` in front
/// of the real work.
///
/// The reduction runs whatever `<clause>` turns out to be, compound or not. It used to bail
/// when the remainder still held a `;`, `&` or `|`, and that bail is what put a path where a
/// verb belongs on the live panel (2026-09-13: three RUNNING NOW rows reading
/// `cd ~/src/example/st…`, every one of them a pipeline whose `cd` therefore survived). A `cd` prefix is never the interesting half of a row; [`classify_command`]
/// still reads `Compound` off the remainder, so the class is unchanged by dropping it.
///
/// `None` when `s` does not start with a standalone `cd`, or has no such separator.
fn reduce_cd_prefix(s: &str) -> Option<String> {
    let after_cd = s.strip_prefix("cd")?;
    if !after_cd.starts_with(char::is_whitespace) {
        return None;
    }
    let after_cd = after_cd.trim_start();
    let dir_end = after_cd.find(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))?;
    if dir_end == 0 {
        return None;
    }
    let after_dir = after_cd[dir_end..].trim_start();
    let after_sep = after_dir
        .strip_prefix("&&")
        .or_else(|| after_dir.strip_prefix(';'))?;
    let clause = after_sep.trim();
    if clause.is_empty() {
        return None;
    }
    Some(clause.to_string())
}

/// Does `s` start with `word` as a whole word (end of string, or followed by whitespace)?
fn starts_with_word(s: &str, word: &str) -> bool {
    s.strip_prefix(word)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Classify an already-REDUCED command (see [`normalize_bash_command`]) — the hook's own
/// `elif` chain, same order: `wait` first (never a defect), then `compound` (a genuine one,
/// since `cd` was already stripped), then by the first word's basename.
fn classify_command(reduced: &str) -> CommandClass {
    if reduced.contains("wait-for-line")
        || reduced.contains("merge-when-green")
        || starts_with_word(reduced, "until")
        || starts_with_word(reduced, "sleep")
    {
        return CommandClass::Wait;
    }
    if reduced.contains([';', '&', '|']) {
        return CommandClass::Compound;
    }
    let mut words = reduced.split_whitespace();
    let w0 = words.next().unwrap_or("");
    let w0_base = w0.rsplit('/').next().unwrap_or(w0);
    let w1 = words.next().unwrap_or("");
    if matches!(w0_base, "find" | "grep" | "fd" | "rg" | "bfs" | "du") {
        return CommandClass::Search;
    }
    if w0_base == "git" {
        return if matches!(w1, "fetch" | "push" | "pull" | "clone" | "ls-remote") {
            CommandClass::GitNet
        } else {
            CommandClass::GitLocal
        };
    }
    if matches!(
        w0_base,
        "cargo"
            | "pnpm"
            | "npm"
            | "yarn"
            | "uv"
            | "uvx"
            | "pytest"
            | "vitest"
            | "make"
            | "tsc"
            | "bun"
            | "cargo-q.sh"
    ) {
        return CommandClass::Build;
    }
    CommandClass::Other
}

/// Reduce a Bash command to its IDENTITY — the one clause that says what it is — so the
/// 120-char cap in [`normalize_bash_command`] truncates a short, meaningful string instead of
/// cutting the raw command mid-token. Ports the identity-reduction spec worked out and
/// validated against 40,589 real commands harvested from `~/.claude/projects/` (see
/// `docs/design/tools-tab.md`); the Python prototype's `identity()` is the line-by-line source
/// of truth this module mirrors.
///
/// In order: split on `;`, `&&`, `||`, `|` and newline, but ONLY where unquoted and at bracket
/// depth zero ([`shell_scan`] walks the string once, tracking quote/depth state, because a
/// regex cannot do this — `sed -i 's|a|b|'` carries a `|` inside its own quotes); drop a
/// segment that is only an env assignment and unwrap `T=$(...)`; drop `|| exit` / `|| true`
/// error handlers; strip env prefixes, wrappers and redirections per segment; skip trivial
/// verbs unless every segment is trivial; give an interpreter its script's basename; shorten
/// long absolute paths; and cut at a quote/bracket-aware boundary that is repaired FORWARD
/// (closing the quote, closing brackets, marking with `…`) rather than trimmed backward — a
/// backward trim is what produced a bare `rg -n` for 532 commands in the prototype's own
/// history, so [`balance_identity`] never does that.
mod identity {
    /// One position from [`shell_scan`]: the character, its bracket depth, and whether it
    /// sits inside an unescaped quote. Mirrors the Python prototype's `scan()` generator,
    /// including its quirk that an ESCAPED character's `quoted` flag reflects whatever quote
    /// state was already active, not the escape itself — a backslash only suppresses the next
    /// character from opening/closing a quote or bracket, it does not itself put that
    /// character inside a quote.
    struct ScanPos {
        ch: char,
        depth: usize,
        quoted: bool,
    }

    /// Walk `s` once, character by character, tracking bash quoting (`'...'` disables
    /// backslash escapes; `"..."` does not) and bracket depth (`(`, `[`, `{`). Every caller
    /// that needs to know "is this character inside quotes, or inside brackets" reads it from
    /// here rather than re-deriving it — see the module doc for why a regex cannot replace
    /// this walk.
    fn shell_scan(s: &str) -> Vec<ScanPos> {
        let mut out = Vec::with_capacity(s.len());
        let mut q: Option<char> = None;
        let mut depth: usize = 0;
        let mut esc = false;
        for ch in s.chars() {
            if esc {
                esc = false;
                out.push(ScanPos {
                    ch,
                    depth,
                    quoted: q.is_some(),
                });
                continue;
            }
            if ch == '\\' && q != Some('\'') {
                esc = true;
                out.push(ScanPos {
                    ch,
                    depth,
                    quoted: q.is_some(),
                });
                continue;
            }
            if let Some(qc) = q {
                if ch == qc {
                    q = None;
                }
                out.push(ScanPos {
                    ch,
                    depth,
                    quoted: true,
                });
                continue;
            }
            if ch == '"' || ch == '\'' {
                q = Some(ch);
                out.push(ScanPos {
                    ch,
                    depth,
                    quoted: true,
                });
                continue;
            }
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
            out.push(ScanPos {
                ch,
                depth,
                quoted: false,
            });
        }
        out
    }

    /// Split `cmd` on `;`, `&&`, `||`, `|` and newline, only where unquoted and at bracket
    /// depth zero. Blank segments (after trimming) are dropped, matching the prototype's own
    /// `[s for s in out if s.strip()]`.
    fn split_segments(cmd: &str) -> Vec<String> {
        let chars: Vec<char> = cmd.chars().collect();
        let state = shell_scan(cmd);
        let n = chars.len();
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < n {
            let pos = &state[i];
            if !pos.quoted && pos.depth == 0 {
                if i + 1 < n {
                    let two = (chars[i], chars[i + 1]);
                    if two == ('&', '&') || two == ('|', '|') {
                        out.push(chars[start..i].iter().collect::<String>());
                        start = i + 2;
                        i += 2;
                        continue;
                    }
                }
                if matches!(chars[i], ';' | '\n' | '|') {
                    out.push(chars[start..i].iter().collect::<String>());
                    start = i + 1;
                    i += 1;
                    continue;
                }
            }
            i += 1;
        }
        out.push(chars[start..].iter().collect::<String>());
        out.into_iter().filter(|s| !s.trim().is_empty()).collect()
    }

    /// Trailing characters `safe_cut` and `balance` strip after a cut — the prototype's
    /// `rstrip(' \\|&<>=-\t')`.
    fn rstrip_cut_junk(s: &str) -> String {
        s.trim_end_matches(|c| " \\|&<>=-\t".contains(c))
            .to_string()
    }

    /// Cut `s` to at most `n` characters at a quote/bracket-aware boundary — the last
    /// unquoted, depth-zero space at or before `n` — but only when that boundary keeps at
    /// least 60% of the budget. A boundary that throws away more than that is lossy, not
    /// safe: `rg -n "a|b|c" path` has its last unquoted space at index 5, so cutting there
    /// yields a bare `rg -n`. Below that threshold, cut at the budget itself and let
    /// [`balance_identity`] repair whatever it lands inside.
    fn safe_cut(s: &str, n: usize) -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= n {
            return rstrip_cut_junk(s);
        }
        let state = shell_scan(s);
        let mut last = 0usize;
        for (i, pos) in state.iter().enumerate() {
            if i > n {
                break;
            }
            if pos.ch == ' ' && !pos.quoted && pos.depth == 0 {
                last = i;
            }
        }
        let cut: String = if last > 0 && last >= (n * 6) / 10 {
            chars[..last].iter().collect()
        } else {
            chars[..n].iter().collect()
        };
        rstrip_cut_junk(&cut)
    }

    /// Remove `2>&1`/`>&1`, process substitution (`<(...)`/`>(...)`, replaced with a `‹sub›`
    /// marker so it is never left an eaten orphan `)`), and any other `N< `/`N> ` redirection —
    /// all only where unquoted and at bracket depth zero.
    fn strip_redirects(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let state = shell_scan(s);
        let n = chars.len();
        let mut keep = String::new();
        let mut i = 0usize;
        while i < n {
            let pos = &state[i];
            if !pos.quoted && pos.depth == 0 {
                if chars_eq(&chars, i, "2>&1") {
                    i += 4;
                    continue;
                }
                if chars_eq(&chars, i, ">&1") {
                    i += 3;
                    continue;
                }
                if matches!(chars[i], '<' | '>') && i + 1 < n && chars[i + 1] == '(' {
                    let mut d = 0i32;
                    let mut j = i + 1;
                    while j < n {
                        if chars[j] == '(' {
                            d += 1;
                        } else if chars[j] == ')' {
                            d -= 1;
                            if d == 0 {
                                j += 1;
                                break;
                            }
                        }
                        j += 1;
                    }
                    keep.push_str("\u{2039}sub\u{203a}");
                    i = j;
                    continue;
                }
                if chars[i].is_ascii_digit() && i + 1 < n && matches!(chars[i + 1], '<' | '>') {
                    i += 1;
                    continue;
                }
                if matches!(chars[i], '<' | '>') {
                    let mut j = i;
                    while j < n && matches!(chars[j], '<' | '>' | '&') {
                        j += 1;
                    }
                    while j < n && chars[j] == ' ' {
                        j += 1;
                    }
                    while j < n && !matches!(chars[j], ' ' | '\t') {
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
            keep.push(chars[i]);
            i += 1;
        }
        keep.trim().to_string()
    }

    fn chars_eq(chars: &[char], i: usize, pat: &str) -> bool {
        let pat: Vec<char> = pat.chars().collect();
        i + pat.len() <= chars.len() && chars[i..i + pat.len()] == pat[..]
    }

    /// Make a cut identity well-formed WITHOUT throwing its content away: drop a dangling
    /// escape, close an open quote, close any open brackets, and mark the repair with `…`.
    /// Trimming BACKWARD to the last safe point instead is what produced a bare `rg -n` for
    /// 532 commands in the prototype's own history — a command whose whole payload is one
    /// long quoted token (`printf '...'`, `jq '{...}'`) has no safe cut point before the width
    /// budget, so trimming back gives the bare verb, which says nothing.
    fn balance_identity(s: &str) -> String {
        let mut q: Option<char> = None;
        let mut esc = false;
        let mut stack: Vec<char> = Vec::new();
        for ch in s.chars() {
            if esc {
                esc = false;
                continue;
            }
            if ch == '\\' && q != Some('\'') {
                esc = true;
                continue;
            }
            if let Some(qc) = q {
                if ch == qc {
                    q = None;
                }
                continue;
            }
            if ch == '"' || ch == '\'' {
                q = Some(ch);
                continue;
            }
            match ch {
                '(' | '[' | '{' => stack.push(ch),
                ')' | ']' | '}' if !stack.is_empty() => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if q.is_none() && stack.is_empty() {
            return s.to_string();
        }
        let mut out = s.trim_end_matches(['\\', ' ']).to_string();
        if !out.is_empty() && (q.is_some() || !stack.is_empty()) {
            out.push('\u{2026}');
        }
        if let Some(qc) = q {
            out.push(qc);
        }
        for c in stack.iter().rev() {
            out.push(match c {
                '(' => ')',
                '[' => ']',
                '{' => '}',
                _ => unreachable!("stack only ever holds an open bracket"),
            });
        }
        out
    }

    fn is_path_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '+' | '-')
    }

    /// Shorten every run of 3+ absolute-path components (`/a/b/c/d` -> `.../c/d`) — a long
    /// path cannot widen the panel row any more than a long command can.
    fn shorten_paths(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut out = String::new();
        let mut i = 0usize;
        while i < n {
            if chars[i] == '/' {
                let mut j = i;
                let mut components: Vec<String> = Vec::new();
                while j < n && chars[j] == '/' {
                    let seg_start = j + 1;
                    let mut k = seg_start;
                    while k < n && is_path_char(chars[k]) {
                        k += 1;
                    }
                    if k == seg_start {
                        break;
                    }
                    components.push(chars[seg_start..k].iter().collect());
                    j = k;
                }
                if components.len() >= 3 {
                    let last_two = &components[components.len() - 2..];
                    out.push_str(".../");
                    out.push_str(&last_two.join("/"));
                    i = j;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// Collapse a run of 2+ whitespace characters to a single space. A single embedded
    /// whitespace character (one real newline, from a quoted multi-line string that
    /// [`split_segments`] never split on) is left exactly as it is — [`normalize_bash_command`]
    /// turns that one into `⏎` afterward.
    fn collapse_ws(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut out = String::new();
        let mut i = 0usize;
        while i < n {
            if chars[i].is_whitespace() {
                let mut j = i;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                out.push(if j - i >= 2 { ' ' } else { chars[i] });
                i = j;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    /// Finish an identity candidate: collapse whitespace, shorten paths, cut to `maxlen`, then
    /// repair. The prototype's `_fin`.
    fn finish(s: &str, maxlen: usize) -> String {
        let collapsed = collapse_ws(s);
        let trimmed = collapsed.trim();
        let shortened = shorten_paths(trimmed);
        let cut = safe_cut(&shortened, maxlen);
        balance_identity(&cut).trim().to_string()
    }

    /// Every leading `IDENT=value` env assignment, in order — `value` is a double-quoted
    /// string, a single-quoted string, or a run of non-whitespace (the prototype's `ENV`).
    fn strip_env(s: &str) -> String {
        let mut cur = s.trim_start();
        while let Some(rest) = strip_one_env(cur) {
            cur = rest;
        }
        cur.to_string()
    }

    /// End positions (char count from the string start) that `(?:"[^"]*"|'[^']*'|\S*)` could
    /// land on, starting at `i`, tried in the regex's own left-to-right alternative order — the
    /// quoted alternative first (if `chars[i]` opens one), then the always-available `\S*` run.
    /// A caller whose own trailing requirement rejects the first candidate must fall through to
    /// the next one, exactly as the Python reference's regex engine backtracks into the next
    /// alternative when the first one's local match cannot be extended to satisfy what follows
    /// it — e.g. `PATH="$(echo "$PATH" | ...)"` has its first `"..."` alternative close at the
    /// FIRST embedded quote (regex has no idea the value contains nested quoting), which is not
    /// followed by whitespace, so the real match falls through to the bare `\S*` run instead.
    fn env_value_ends(chars: &[char], i: usize) -> Vec<usize> {
        let n = chars.len();
        let mut ends = Vec::new();
        if i < n && (chars[i] == '"' || chars[i] == '\'') {
            let q = chars[i];
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == q) {
                ends.push(close + 1);
            }
        }
        let mut j = i;
        while j < n && !chars[j].is_whitespace() {
            j += 1;
        }
        ends.push(j);
        ends
    }

    fn strip_one_env(s: &str) -> Option<&str> {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut i = 0usize;
        if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            return None;
        }
        i += 1;
        while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            return None;
        }
        i += 1;
        for end in env_value_ends(&chars, i) {
            if end < n && chars[end].is_whitespace() {
                let byte_off = s.char_indices().nth(end).map(|(b, _)| b)?;
                return Some(s[byte_off..].trim_start());
            }
        }
        None
    }

    /// Is `seg` (once trimmed) JUST one `IDENT=value` assignment and nothing else? The
    /// prototype's `ASSIGN_ONLY`.
    fn is_assign_only(seg: &str) -> bool {
        let s = seg.trim_start();
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut i = 0usize;
        if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            return false;
        }
        i += 1;
        while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            return false;
        }
        i += 1;
        env_value_ends(&chars, i)
            .into_iter()
            .any(|end| chars[end..].iter().all(|c| c.is_whitespace()))
    }

    /// Every leading wrapper (`timeout N`, `nice`, `env`, `command`, `exec`,
    /// `henry-unthrottled`, `stdbuf ARG`, `caffeinate`, `sudo`), in order — the prototype's
    /// `WRAP`.
    fn strip_wrap(s: &str) -> String {
        let mut cur = s.trim_start();
        while let Some(rest) = strip_one_wrap(cur) {
            cur = rest;
        }
        cur.to_string()
    }

    /// Take the first whitespace-delimited token of `s`, requiring at least one whitespace
    /// character after it, returning `(token, rest trimmed of its leading whitespace)`.
    fn take_ws_token(s: &str) -> Option<(&str, &str)> {
        let ws = s.find(char::is_whitespace)?;
        Some((&s[..ws], s[ws..].trim_start()))
    }

    fn is_timeout_duration(tok: &str) -> bool {
        let core = tok.strip_suffix(['s', 'm', 'h']).unwrap_or(tok);
        !core.is_empty() && core.chars().all(|c| c.is_ascii_digit())
    }

    fn strip_one_wrap(s: &str) -> Option<&str> {
        let (first, rest) = take_ws_token(s)?;
        match first {
            "env" | "command" | "exec" | "henry-unthrottled" | "sudo" => Some(rest),
            "timeout" => {
                if let Some((tok, after)) = take_ws_token(rest) {
                    if let Some(stripped) = tok.strip_prefix('-') {
                        let _ = stripped;
                        if let Some((tok2, after2)) = take_ws_token(after) {
                            if is_timeout_duration(tok2) {
                                return Some(after2);
                            }
                        }
                        return None;
                    }
                    if is_timeout_duration(tok) {
                        return Some(after);
                    }
                }
                None
            }
            "nice" => {
                if let Some((tok, after)) = take_ws_token(rest) {
                    if let Some(numpart) = tok.strip_prefix("-n") {
                        if !numpart.is_empty() {
                            let d = numpart.strip_prefix('-').unwrap_or(numpart);
                            if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
                                return Some(after);
                            }
                        } else if let Some((tok2, after2)) = take_ws_token(after) {
                            let d = tok2.strip_prefix('-').unwrap_or(tok2);
                            if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
                                return Some(after2);
                            }
                        }
                    }
                }
                Some(rest)
            }
            "stdbuf" => {
                let (_, after) = take_ws_token(rest)?;
                Some(after)
            }
            "caffeinate" => {
                if let Some((tok, after)) = take_ws_token(rest) {
                    if tok.starts_with('-') {
                        return Some(after);
                    }
                }
                Some(rest)
            }
            _ => None,
        }
    }

    /// Alternate env/wrap stripping until stable — a wrapper can reveal a new env prefix and
    /// vice versa (`timeout 5 FOO=1 cargo test`). The prototype's `_strip`.
    fn strip_env_and_wrap(s: &str) -> String {
        let mut cur = s.to_string();
        loop {
            let before = cur.clone();
            cur = strip_env(&cur);
            cur = strip_wrap(&cur);
            if cur == before {
                break;
            }
        }
        cur.trim().to_string()
    }

    /// Does `seg` (trimmed) match `IDENT=$(...)` with nothing else? Returns the inside of the
    /// `$( )`. The prototype's inline `re.match(r'^\s*[A-Za-z_]\w*=\$\((.*)\)\s*$', ..., re.S)`.
    fn match_assign_dollar_paren(seg: &str) -> Option<String> {
        let s = seg.trim_start();
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut i = 0usize;
        if i >= n || !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            return None;
        }
        i += 1;
        while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            return None;
        }
        i += 1;
        if !(i + 1 < n && chars[i] == '$' && chars[i + 1] == '(') {
            return None;
        }
        let open = i + 2;
        let last_close = chars.iter().rposition(|&c| c == ')')?;
        if last_close < open || !chars[last_close + 1..].iter().all(|c| c.is_whitespace()) {
            return None;
        }
        Some(chars[open..last_close].iter().collect())
    }

    /// First occurrence anywhere in `s` of `>` (optionally followed by whitespace) then 1+
    /// path-like characters — the prototype's `re.search(r'>\s*([\w./@+\-]+)', s)`.
    fn find_redirect_target(s: &str) -> Option<String> {
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        for i in 0..n {
            if chars[i] == '>' {
                let mut j = i + 1;
                while j < n && chars[j].is_whitespace() {
                    j += 1;
                }
                let start = j;
                while j < n && is_target_char(chars[j]) {
                    j += 1;
                }
                if j > start {
                    return Some(chars[start..j].iter().collect());
                }
            }
        }
        None
    }

    fn is_target_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '@' | '+' | '-')
    }

    /// The leading run of `[\w./+-]` characters — the prototype's `LEAD`.
    fn lead_match(s: &str) -> Option<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0usize;
        while i < chars.len() && is_lead_char(chars[i]) {
            i += 1;
        }
        if i == 0 {
            None
        } else {
            Some(chars[..i].iter().collect())
        }
    }

    fn is_lead_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '+' | '-')
    }

    fn basename(s: &str) -> &str {
        s.rsplit('/').next().unwrap_or(s)
    }

    /// `\s*\|\|\s*(exit|true|false|return)\b[^;&\n]*`, removed globally (not quote/depth
    /// aware — the prototype's own sub runs over the whole raw string, quotes and all, so this
    /// mirrors that exactly rather than "fixing" it).
    fn strip_error_handlers(c: &str) -> String {
        let chars: Vec<char> = c.chars().collect();
        let n = chars.len();
        let mut out = String::new();
        let mut i = 0usize;
        while i < n {
            if let Some(end) = match_error_handler(&chars, i) {
                i = end;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    fn match_error_handler(chars: &[char], i: usize) -> Option<usize> {
        let n = chars.len();
        let mut j = i;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j + 1 >= n || chars[j] != '|' || chars[j + 1] != '|' {
            return None;
        }
        let mut k = j + 2;
        while k < n && chars[k].is_whitespace() {
            k += 1;
        }
        for kw in ["exit", "true", "false", "return"] {
            let kwlen = kw.chars().count();
            if k + kwlen <= n {
                let slice: String = chars[k..k + kwlen].iter().collect();
                if slice == kw {
                    let after = k + kwlen;
                    let boundary_ok = after >= n
                        || !(chars[after].is_ascii_alphanumeric() || chars[after] == '_');
                    if boundary_ok {
                        let mut e = after;
                        while e < n && !matches!(chars[e], ';' | '&' | '\n') {
                            e += 1;
                        }
                        return Some(e);
                    }
                }
            }
        }
        None
    }

    /// `re.findall(r'\$\(([^()]{4,})\)', c)` — every non-overlapping `$( )` whose inside has
    /// 4+ characters that are not `(` or `)`.
    fn find_dollar_paren_candidates(c: &str) -> Vec<String> {
        let chars: Vec<char> = c.chars().collect();
        let n = chars.len();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < n {
            if i + 1 < n && chars[i] == '$' && chars[i + 1] == '(' {
                let mut j = i + 2;
                while j < n && chars[j] != '(' && chars[j] != ')' {
                    j += 1;
                }
                if j - (i + 2) >= 4 && j < n && chars[j] == ')' {
                    out.push(chars[i + 2..j].iter().collect());
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// `-[a-z]*[ce][a-z]*(\s|$)` anchored at the start of `rest` — equivalent to "a `-` then a
    /// nonempty run of lowercase letters that contains a `c` or an `e`, then a boundary",
    /// since the whole run either side of the required `[ce]` is itself `[a-z]*`.
    fn matches_dash_ce(rest: &str) -> bool {
        let chars: Vec<char> = rest.chars().collect();
        let n = chars.len();
        if n == 0 || chars[0] != '-' {
            return false;
        }
        let mut i = 1;
        while i < n && chars[i].is_ascii_lowercase() {
            i += 1;
        }
        if i == 1 {
            return false;
        }
        if !chars[1..i].iter().any(|&c| c == 'c' || c == 'e') {
            return false;
        }
        i == n || chars[i].is_whitespace()
    }

    const TRIV: &[&str] = &[
        "echo", "cd", "mkdir", "export", "set", "true", "false", ":", "printf", "touch", "source",
        ".", "pushd", "popd", "umask", "unset", "wait", "sleep", "exit", "return", "pwd", "clear",
    ];
    const INTERP: &[&str] = &[
        "bash", "sh", "zsh", "python", "python3", "node", "bun", "uv", "uvx", "ruby", "perl",
        "deno", "npx",
    ];

    const MAXLEN: usize = 62;

    fn starts_with_control_keyword(c: &str) -> bool {
        ["for", "while", "until", "if", "case"]
            .iter()
            .any(|kw| starts_with_word_boundary(c, kw))
    }

    fn starts_with_word_boundary(s: &str, word: &str) -> bool {
        s.strip_prefix(word)
            .is_some_and(|rest| match rest.chars().next() {
                None => true,
                Some(c) => !(c.is_alphanumeric() || c == '_'),
            })
    }

    /// The prototype's `identity(cmd, maxlen=62)`, recursive calls included.
    pub(super) fn identity(cmd: &str) -> String {
        identity_at(cmd, MAXLEN)
    }

    fn identity_at(cmd: &str, maxlen: usize) -> String {
        if cmd.trim().is_empty() {
            return String::new();
        }
        let c = cmd.trim().to_string();
        if starts_with_control_keyword(&c) {
            let segs = split_segments(&c);
            let first = segs.into_iter().next().unwrap_or_else(|| c.clone());
            return finish(&strip_env_and_wrap(&first), maxlen);
        }
        let c = strip_error_handlers(&c);
        let mut fallback = String::new();
        for seg in split_segments(&c) {
            if is_assign_only(&seg) {
                continue;
            }
            let seg = match_assign_dollar_paren(&seg).unwrap_or(seg);
            let mut s = strip_env_and_wrap(&seg);
            s = s.trim_start_matches('(').trim().to_string();
            loop {
                if s.ends_with(')') && s.matches('(').count() < s.matches(')').count() {
                    s.pop();
                    while s.ends_with(|c: char| c.is_whitespace()) {
                        s.pop();
                    }
                } else {
                    break;
                }
            }
            let opens = s.matches('(').count();
            let closes = s.matches(')').count();
            if closes > opens {
                let mut to_remove = closes - opens;
                let mut result = String::with_capacity(s.len());
                for ch in s.chars() {
                    if ch == ')' && to_remove > 0 {
                        to_remove -= 1;
                        continue;
                    }
                    result.push(ch);
                }
                s = result;
            }
            if s.is_empty() {
                continue;
            }
            let heredoc = s.contains("<<");
            let tgt = find_redirect_target(&s);
            s = strip_redirects(&s);
            if s.is_empty() {
                continue;
            }
            let Some(lead) = lead_match(&s) else {
                continue;
            };
            let verb = basename(&lead).to_string();
            if verb.is_empty() {
                continue;
            }
            let lead_char_len = lead.chars().count();
            let rest: String = s.chars().skip(lead_char_len).collect::<String>();
            let rest = rest.trim().to_string();
            if TRIV.contains(&verb.as_str()) {
                if fallback.is_empty() {
                    fallback = finish(&s, maxlen);
                }
                continue;
            }
            if heredoc {
                return match &tgt {
                    Some(t) => finish(&format!("{verb} > {t}"), maxlen),
                    None => format!("{} \u{2039}heredoc\u{203a}", finish(&verb, maxlen)),
                };
            }
            if INTERP.contains(&verb.as_str()) {
                if matches_dash_ce(&rest) || rest.starts_with("- ") {
                    return format!(
                        "{} \u{2039}script\u{203a}",
                        finish(&format!("{verb} -c"), maxlen)
                    );
                }
                for tk in rest.split_whitespace() {
                    let bare = tk.trim_matches(|c| c == '"' || c == '\'');
                    if bare.contains("$(") || bare.contains('`') {
                        return finish(&format!("{verb} {rest}"), maxlen);
                    }
                    if bare.starts_with('-') || (bare.contains('=') && !bare.starts_with('/')) {
                        continue;
                    }
                    let basename_tk = basename(bare);
                    if let Some(idx) = rest.find(tk) {
                        let tail = &rest[idx..];
                        let replaced = format!("{basename_tk}{}", &tail[tk.len()..]);
                        return finish(&format!("{verb} {replaced}"), maxlen);
                    }
                }
                return verb;
            }
            return finish(&format!("{verb} {rest}"), maxlen);
        }
        for cand in find_dollar_paren_candidates(&c) {
            let got = identity_at(&cand, maxlen);
            if !got.is_empty() {
                let first_word = got.split_whitespace().next().unwrap_or("");
                if !TRIV.contains(&first_word) {
                    return got;
                }
            }
        }
        if !fallback.is_empty() {
            fallback
        } else {
            finish(&strip_env_and_wrap(&c), maxlen)
        }
    }
}

/// Normalize a raw Bash `input.command` into a `(head, class)` pair for the Tools tab —
/// ports `hooks/log-slow-bash.sh`'s `jq` normalization verbatim, in order: strip leading
/// whitespace, strip leading env assignments, strip wrappers, reduce a `cd <dir> &&
/// <clause>` prefix (only when `<clause>` is itself simple), strip wrappers again (`cd
/// <dir> && timeout 15 rg ...` is the shape `prefer-fd-rg` itself emits) — THEN reduce the
/// result to its [`identity::identity`] (the one clause that says what the command is) before
/// truncating to [`COMMAND_HEAD_MAX`], so the cap trims a short meaningful string instead of a
/// long one mid-token. The class is read off the REDUCED command, before either the identity
/// step or the truncation — display formatting must never change what a command classifies as
/// (#289's own rule, unchanged by #336's identity reduction).
pub fn normalize_bash_command(raw: &str) -> (String, CommandClass) {
    let s = strip_wrappers(strip_env_prefix(raw.trim_start()));
    let reduced = reduce_cd_prefix(s).unwrap_or_else(|| s.to_string());
    let reduced = strip_wrappers(&reduced).to_string();
    let class = classify_command(&reduced);
    let head = identity::identity(&reduced)
        .replace('\n', "⏎")
        .chars()
        .take(COMMAND_HEAD_MAX)
        .collect();
    (head, class)
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

/// Upper bound on how many of `messages`' trailing entries [`extract_tool_events`] will ever
/// parse while walking backward for the assistant turn that owns the newest `tool_use` — see
/// the module doc. Comfortably covers the observed shape (one or two trailing non-assistant
/// messages, e.g a system-reminder sent as its own message) while keeping the parse cost
/// small and fixed even for a turn with no tool_use at all, where the walk never finds an
/// `assistant` message and runs all the way to the cap.
const TAIL_SCAN_CAP: usize = 20;

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

/// The minimal shape read from `messages`: only `role` and `content`, and only for the
/// bounded tail window [`extract_tool_events`] walks — see the module doc for why the rest
/// of the history is never materialized.
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

    // Walk backward from the newest message, parsing each into the tail window, until an
    // `assistant` message is reached (where the newest `tool_use` block, if any, lives) or
    // `TAIL_SCAN_CAP` is hit — see the module doc for why a fixed "last two" undercounts.
    // `tail` ends up oldest-first, matching `all`'s own order, once reversed below.
    let mut tail: Vec<MessagePeek> = Vec::new();
    for raw in all.iter().rev().take(TAIL_SCAN_CAP) {
        let Ok(msg) = serde_json::from_str::<MessagePeek>(raw.get()) else {
            continue;
        };
        let is_assistant = msg.role == "assistant";
        tail.push(msg);
        if is_assistant {
            break;
        }
    }
    tail.reverse();

    for msg in tail {
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
                        tool_uses.push(tool_use_event_from_block(id, b.name, b.input.as_ref()));
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

/// Build one [`ToolUseEvent`] from a `tool_use` block's `id`, `name` and `input`.
///
/// Factored out of [`extract_tool_events`] because a `tool_use` block now reaches this table
/// from TWO directions and both must produce the identical event: a REQUEST body's assistant
/// message (here) and the RESPONSE the proxy has just finished streaming (`proxy.rs`'s SSE and
/// non-streamed parse paths). The response side is what makes `running` non-empty at all — a
/// Claude Code request carries a `tool_use` and its matching `tool_result` in the SAME body, so
/// a request-only table opens and closes every call within one call and shows nothing in
/// flight (measured 2026-09-13 against the live proxy: 11 sessions, `running: 0` in every one,
/// while at least three were mid-tool).
///
/// Every head produced here is display context for a panel row, capped at
/// [`COMMAND_HEAD_MAX`] characters — see [`ToolUseEvent::command_head`] for the per-tool shape
/// and the module doc for where it may and may not be held.
pub fn tool_use_event_from_block(
    id: String,
    name: Option<String>,
    input: Option<&Value>,
) -> ToolUseEvent {
    let mut command_class = None;
    let field = |key: &str| input.and_then(|v| v.get(key)).and_then(Value::as_str);
    let capped = |s: String| -> String { s.chars().take(COMMAND_HEAD_MAX).collect() };
    let command_head = match name.as_deref() {
        Some("Bash") => field("command").map(|s| {
            let (head, class) = normalize_bash_command(s);
            command_class = Some(class);
            head
        }),
        Some("Agent") | Some("Task") => field("description").map(|description| {
            let head = match field("subagent_type") {
                Some(subagent_type) => format!("{subagent_type}: {description}"),
                None => description.to_string(),
            };
            capped(head)
        }),
        // A running `TaskOutput` is a session WAITING on a subagent, not working — without
        // this it renders as the bare word `TaskOutput`, which is what four of the live
        // slowest-six rows said on 2026-09-13.
        Some("TaskOutput") => field("task_id")
            .map(|task_id| capped(format!("TaskOutput · waiting on task {task_id}"))),
        Some(tool @ ("Read" | "Edit" | "Write" | "NotebookEdit")) => field("file_path")
            .or_else(|| field("notebook_path"))
            .map(|path| capped(format!("{tool} {path}"))),
        Some(tool @ ("Grep" | "Glob")) => {
            field("pattern").map(|pattern| capped(format!("{tool} {pattern}")))
        }
        // The HOST only: a fetch URL's path and query carry the search term or record id,
        // and the row only needs to say where the call went.
        Some("WebFetch") => field("url").map(|url| capped(format!("WebFetch {}", url_host(url)))),
        Some("WebSearch") => field("query").map(|query| capped(format!("WebSearch {query}"))),
        // Every tool without a dedicated arm above (`Monitor` and anything added after this
        // list) still gets a head: the bare tool name, so the row says what ran instead of
        // rendering blank. `None` here is what produced the head-less `slowest`/`timed_out`
        // rows this module's doc-comment and `WireSessionTracker::restore` both describe.
        _ => name.clone().map(capped),
    };
    ToolUseEvent {
        id,
        name,
        command_head,
        command_class,
    }
}

/// The host part of a URL. Hand-rolled rather than adding a URL crate for one display field:
/// drop the scheme, cut at the first `/`, `?` or `#`, then drop any `user@` credentials prefix
/// (which is exactly the part that must never reach a panel row). A string carrying none of
/// those separators is its own host.
fn url_host(raw: &str) -> &str {
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// One tool call still awaiting its `tool_result`.
///
/// `Serialize`/`Deserialize` (and on every struct below it, down to
/// [`WireSession`]) exist for exactly one reader: [`crate::session_wire_persist`],
/// which round-trips a session's live state to disk verbatim so a restored row
/// is the SAME struct the tracker already knows how to project, not a second
/// parallel shape that could drift from it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunningTool {
    pub tool: String,
    pub started_ms: i64,
    /// `#[serde(default)]` — NOT `skip_serializing`: the head persists (Gil, 2026-09-13,
    /// reversing #299; the module doc has the why). `default` stays so a cache file written
    /// by a build between #299 and that ruling, which omits the field entirely, still loads
    /// rather than taking every session in the file down with it.
    #[serde(default)]
    pub command_head: Option<String>,
    pub command_class: Option<CommandClass>,
}

/// One completed tool call, for the "ten slowest" list — and, with the same fields, for
/// [`ToolStats::timed_out`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SlowTool {
    pub tool: String,
    pub seconds: f64,
    /// Same `#[serde(default)]` reasoning as [`RunningTool::command_head`].
    #[serde(default)]
    pub command_head: Option<String>,
    pub command_class: Option<CommandClass>,
    pub ended_ms: i64,
}

/// How many of a tool's most recent call durations are kept for [`ToolBucket::seconds_p50`] —
/// see the bridge ("a bounded reservoir per tool for the median").
pub const TOOL_DURATION_RESERVOIR_CAP: usize = 256;

/// One tool's aggregate stats within a session — the source for `SessionToolsRow::by_tool`
/// (`crates/tcr-status-wire`). Keyed on `tool_use.name` verbatim; `Read`, `Grep`, `Glob` and
/// `Edit` are deliberately NOT merged here (the panel groups them) — see the bridge.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    pub timeouts: u64,
    /// [`Self::timeouts`] split by what the command WAS, keyed on
    /// [`CommandClass::as_str`] for a `Bash` call that carried one and on the tool's own
    /// name (`"Agent"`) for everything else — so the map always sums to `timeouts` and the
    /// panel's "TIMED OUT TODAY" section and its headline count read one number.
    ///
    /// `BTreeMap` rather than `HashMap` because this one is DISPLAYED: a stable key order
    /// keeps the section from reshuffling between two snapshots that say the same thing.
    /// `#[serde(default)]` so a cache file written before this field existed still loads.
    #[serde(default)]
    pub timeouts_by_class: std::collections::BTreeMap<String, u64>,
    /// The commands that timed out, newest first, capped at [`TIMED_OUT_CAP`].
    /// `#[serde(default)]` for the same restore reason as [`Self::timeouts_by_class`].
    #[serde(default)]
    pub timed_out: Vec<SlowTool>,
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelTokenTally {
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

/// One session's row.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WireSession {
    pub account: Option<String>,
    pub model: Option<String>,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
    pub requests: u64,
    /// BASE input only — never cache reads or cache creation, both counted separately below.
    /// The 2026-09-14 incident: this field used to hold `UsageRecord::input_total()`, which
    /// already folds `cache_read_tokens` in, so the panel's hit-ratio (`cache_read /
    /// (input_tokens + cache_creation_tokens + cache_read_tokens)`) double-counted cache reads
    /// and every session rendered ~50% no matter its real hit rate.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// Cache-write tokens (5-minute plus 1-hour TTL creation), kept apart from
    /// `input_tokens` for the same reason as `cache_read_tokens` — see its doc-comment.
    /// `#[serde(default)]` so a cache file written before this field existed still loads.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    pub tools: ToolStats,
    /// See [`ReqPerMinuteRing`].
    pub req_per_minute: ReqPerMinuteRing,
    /// Per-model raw token tallies, keyed on the model id a usage record carried (or
    /// [`UNKNOWN_MODEL`] when it carried none) — the source `crate::manager::wire_sessions`
    /// prices into `SessionRow::cost_usd`, one model's price at a time, then sums.
    pub by_model: std::collections::HashMap<String, ModelTokenTally>,
}

/// Tools whose `tool_use` block is the LAST content block of a turn, so the client never sends
/// another API request off the back of it: the proxy sees the `tool_use`, but nothing after it
/// carries the matching `tool_result`, and it sits in `running` until [`RUNNING_UNBOUNDED_LOST_MS`]
/// calls it lost. Measured against the live proxy, 2026-09-17: 51 of 57 `running` entries were
/// `StructuredOutput` (the Agent SDK's terminal tool), ages 2.6 to 246 minutes.
///
/// A `tool_result` for one of these DOES arrive occasionally (the live `sessions_summary.byTool`
/// row that day: `{'tool': 'StructuredOutput', 'calls': 15, 'errors': 15}`, 15 of 66 seen) — the
/// close path itself is not broken, the tool is simply terminal most of the time. Since
/// [`WireSessionTracker::insert_running`] never puts these in `running`, that rare late
/// `tool_result` has nothing to remove and is silently NOT counted into `calls`/`errors`/`by_tool`
/// either: the whole completion pipeline is gated on a `running` entry existing, so skipping the
/// insert also skips the count. That is the trade this constant makes — no ghost row, at the cost
/// of the already-rare (23%) completion count for exactly these tools.
///
/// A set, not a single name, so the next terminal tool is one line here.
///
/// A name list, not a shortened backstop for every uncapped tool: measured live across the
/// whole fleet the same day, stuck-vs-completed by tool was `StructuredOutput` 59/(59+15) =
/// 79.7%, against `Bash` 5/4846 = 0.1%, `Edit` 1/176 = 0.6%, `Write` 1/225 = 0.4%, and
/// `AskUserQuestion` 1/8 = 12.5% (n=8, and that tool genuinely blocks on a human, so its one
/// stuck entry is most likely real work, not a ghost). `StructuredOutput` sits two orders of
/// magnitude above every ordinary tool — categorically different, not a straggler — which is
/// what makes a name list proportionate here; shortening [`RUNNING_UNBOUNDED_LOST_MS`] for
/// every uncapped tool would misclassify the `Bash`/`Edit`/`Write`/`AskUserQuestion` rows above,
/// which are ordinary in-flight calls, exactly what `running` exists to show. The ghost count
/// was still climbing on the unpatched proxy while this was measured (51 at 19:31, 59 by
/// 20:0x, one unchanged process) — roughly one new ghost per four minutes of fleet activity,
/// capped only by [`PENDING_TOOL_CAP`] (64) per session.
pub const TERMINAL_TOOLS: &[&str] = &["StructuredOutput"];

/// Cap on pending (running) tool-use ids per session — see the bridge.
pub const PENDING_TOOL_CAP: usize = 64;
/// Cap on tracked sessions — see the bridge.
pub const SESSION_CAP: usize = 512;
/// How many of the slowest completed tool calls each session keeps.
pub const SLOWEST_CAP: usize = 10;
/// How many timed-out commands each session keeps, newest first
/// ([`ToolStats::timed_out`]). Twice [`SLOWEST_CAP`]: a timeout is rarer than a slow call
/// but its list is the one a person reads to spot a REPEATING command, and a pattern needs
/// more than ten rows to be visible. Still bounded — this rides the session-wire snapshot
/// to disk, once per session.
pub const TIMED_OUT_CAP: usize = 20;
/// A session unseen for this long is evicted — see the bridge ("an hour").
pub const SESSION_TTL_MS: i64 = 60 * 60 * 1000;

/// How long a `Bash` entry may sit in `running` before the panel calls it LOST rather than
/// running: the Bash tool's own 600 s timeout plus a 60 s grace for the round trip that
/// carries the `tool_result` back to the proxy.
///
/// Past that line the call cannot still be running — the client killed it at the timeout —
/// so an entry still here is one whose `tool_result` never reached this process (a request
/// served by a previous proxy, or a client that never sent the closing turn). Showing it as
/// running is the ghost Gil saw on 2026-09-13: two `Bash · 0s to timeout` rows at 17 minutes,
/// with a red ring, that nothing would ever close.
pub const RUNNING_BASH_LOST_MS: i64 = (600 + 60) * 1000;

/// The same line for every tool the Bash timeout does not govern — `Agent`, `Task`,
/// `TaskOutput` and the file tools have no known deadline, so this is a liveness backstop
/// rather than a deduction: six hours is longer than any real subagent this fleet has run
/// and shorter than a stale entry's useful life. Anything still `running` past it is a lost
/// result, not work in flight.
pub const RUNNING_UNBOUNDED_LOST_MS: i64 = 6 * 60 * 60 * 1000;

/// Is this `running` entry a call still in flight, or a result that was lost?
///
/// See [`RUNNING_BASH_LOST_MS`] and [`RUNNING_UNBOUNDED_LOST_MS`]. Read at PROJECTION time
/// (`crate::manager::wire_sessions::wire_sessions_snapshot`) rather than folded into the
/// table, so a `tool_result` that does arrive late still closes its entry and lands in
/// `slowest` with a real duration.
pub fn running_tool_is_lost(tool: &str, started_ms: i64, now_ms: i64) -> bool {
    let age_ms = now_ms.saturating_sub(started_ms);
    let limit = if tool == "Bash" {
        RUNNING_BASH_LOST_MS
    } else {
        RUNNING_UNBOUNDED_LOST_MS
    };
    age_ms > limit
}

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
            Self::insert_running(&mut entry.tools, tu, previous_response_end_ms);
        }

        for tr in tool_results {
            if let Some(running) = entry.tools.running.remove(&tr.id) {
                let seconds = (now_ms - running.started_ms).max(0) as f64 / 1000.0;
                entry.tools.calls += 1;
                if tr.is_error {
                    entry.tools.errors += 1;
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
                let completed = SlowTool {
                    tool: running.tool,
                    seconds,
                    command_head: running.command_head,
                    command_class: running.command_class,
                    ended_ms: now_ms,
                };
                if tr.timed_out {
                    entry.tools.timeouts += 1;
                    // The class if the classifier gave this call one, the tool's own name
                    // otherwise — so the map sums to `timeouts` for every tool, not just
                    // the `Bash` calls that have a class.
                    let key = completed
                        .command_class
                        .map_or_else(|| completed.tool.clone(), |c| c.as_str().to_string());
                    *entry.tools.timeouts_by_class.entry(key).or_insert(0) += 1;
                    entry.tools.timed_out.insert(0, completed.clone());
                    entry.tools.timed_out.truncate(TIMED_OUT_CAP);
                }
                Self::insert_slowest(&mut entry.tools.slowest, completed);
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
    /// `base_input` accumulates into `entry.input_tokens` — BASE input only, never the quota
    /// figure (`UsageRecord::input_total()`), which already folds cache reads and cache
    /// creation in; folding the quota figure in here is the 2026-09-14 double-count incident
    /// (see [`WireSession::input_tokens`]). `model`, `cache_5m` and `cache_1h` fold into
    /// [`WireSession::by_model`] so a session that spans two models can be priced per-model
    /// and summed, rather than averaged.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &mut self,
        session_id: &str,
        model: Option<&str>,
        base_input: u64,
        cache_5m: u64,
        cache_1h: u64,
        cache_read: u64,
        output: u64,
    ) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.input_tokens += base_input;
            entry.cache_creation_tokens += cache_5m + cache_1h;
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

    /// Insert tool calls parsed from the RESPONSE this session's last request produced, as
    /// running from `now_ms` — the instant the stream ended, which is when the client actually
    /// started the tool. This is the half that makes `running` non-empty: see
    /// [`tool_use_event_from_block`] for why the request side alone never can.
    ///
    /// Deliberately NOT [`Self::record_request`]: this is the second half of a request already
    /// recorded, so it must not count another request, move `last_seen_ms`, or feed the
    /// per-minute ring. A session that is absent is ignored rather than created — the request
    /// half runs first and creates it, and a response alone names nothing to attribute to.
    ///
    /// The matching `tool_result` arrives in the NEXT request and closes the entry through
    /// [`Self::record_request`]'s result loop, which measures the duration from the
    /// `started_ms` set here; that same request also replays the `tool_use`, and the
    /// already-running skip in [`Self::insert_running`] is what keeps this earlier, truer
    /// start instant instead of overwriting it.
    pub fn record_response_tool_uses(
        &mut self,
        session_id: &str,
        now_ms: i64,
        tool_uses: &[ToolUseEvent],
    ) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            for tu in tool_uses {
                Self::insert_running(&mut entry.tools, tu, now_ms);
            }
        }
    }

    /// Put one `tool_use` into `running`, evicting the oldest entry when [`PENDING_TOOL_CAP`]
    /// is reached. An id already running is left ALONE — its recorded start is the earlier and
    /// therefore truer one (the response-side insert), and a later request replaying the same
    /// block must not reset the clock.
    fn insert_running(tools: &mut ToolStats, tu: &ToolUseEvent, started_ms: i64) {
        if tools.running.contains_key(&tu.id) {
            return;
        }
        // A terminal tool (see [`TERMINAL_TOOLS`]) never gets a `tool_result`, so it must never
        // enter `running` in the first place — every caller of this function crosses this one
        // gate, rather than each of them filtering before calling in.
        if tu
            .name
            .as_deref()
            .is_some_and(|name| TERMINAL_TOOLS.contains(&name))
        {
            return;
        }
        if tools.running.len() >= PENDING_TOOL_CAP {
            if let Some(oldest) = tools
                .running
                .iter()
                .min_by_key(|(_, r)| r.started_ms)
                .map(|(k, _)| k.clone())
            {
                tools.running.remove(&oldest);
            }
        }
        tools.running.insert(
            tu.id.clone(),
            RunningTool {
                tool: tu.name.clone().unwrap_or_default(),
                started_ms,
                command_head: tu.command_head.clone(),
                command_class: tu.command_class,
            },
        );
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

    /// Fold restored sessions (`crate::session_wire_persist::load`'s output) into this
    /// table. Existing entries win — a session already tracked by a request served
    /// between boot and this call is fresher than anything on disk, same rule
    /// `Manager::restore_affinity` follows for pins — so this is meant to run once, at
    /// boot, before the listener binds.
    ///
    /// Every restored session's `tools.running` is DROPPED. A running list cannot be
    /// restored truthfully: the `tool_result` that closes one of these entries was addressed
    /// to the process that died, so nothing this process ever sees will close it, and the
    /// panel would draw it as a live call forever (measured 2026-09-13: two `Bash` rows at 17
    /// minutes against a 600 s timeout, restored across a restart). Counts, `slowest` and the
    /// per-tool buckets are all facts about calls that already FINISHED, so they restore
    /// unchanged — except `slowest` and `timed_out`, which each drop any entry whose
    /// `command_head` is `None`. Between #299 (heads stripped before save) and #302 (heads
    /// persisted again), every entry written to `session-wire.json` lost its head; restoring
    /// one gives a row with no command, and it sits there — Gil's screenshot, 2026-09-14 —
    /// until the session ages out (`SESSION_TTL_MS`). Since #298 every tool gets a head
    /// (`tool_use_event_from_block`'s default arm now falls back to the tool's own name), so
    /// a file written by the current build never has a head-less entry and this filter is a
    /// no-op on it; `calls`, `timeouts` and the other counts describe calls that happened
    /// regardless of whether a head survived, so they are untouched.
    pub fn restore(&mut self, sessions: std::collections::HashMap<String, WireSession>) {
        for (session_id, mut session) in sessions {
            session.tools.running.clear();
            session.tools.slowest.retain(|t| t.command_head.is_some());
            session.tools.timed_out.retain(|t| t.command_head.is_some());
            self.sessions.entry(session_id).or_insert(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cd <dir> && <simple clause>` reduces to just the clause — the hook's own
    /// measurement: 18% of its "compound" rows were this exact shape, a free `cd` hiding a
    /// simple command that deserved a real class.
    #[test]
    fn cd_and_a_simple_clause_reduces_to_the_clause() {
        let (head, class) = normalize_bash_command("cd ~/src/example && cargo test -p example");
        assert_eq!(head, "cargo test -p example");
        assert_eq!(class, CommandClass::Build);
    }

    /// A wrapper sitting right after the `cd`'s `&&` — `cd <dir> && timeout 15 rg ...` is the
    /// shape `prefer-fd-rg` itself emits, and the very reason the hook strips wrappers a
    /// SECOND time after the `cd` reduction, not just once up front.
    #[test]
    fn a_wrapper_in_front_of_the_reduced_clause_is_stripped_too() {
        let (head, class) = normalize_bash_command("cd ~/src/example && timeout 15 rg -n TODO");
        assert_eq!(head, "rg -n TODO");
        assert_eq!(class, CommandClass::Search);
    }

    /// An env-var prefix in front of the real command is stripped, and the class is read off
    /// what remains.
    #[test]
    fn an_env_prefix_is_stripped_before_classifying() {
        let (head, class) = normalize_bash_command("CI=1 FOO=bar cargo build --release");
        assert_eq!(head, "cargo build --release");
        assert_eq!(class, CommandClass::Build);
    }

    /// A genuine compound loses its `cd` too, and stays `compound`.
    ///
    /// This assertion used to read the other way — the reduction bailed when the remainder
    /// held another `;`, `&` or `|`, so the whole string including the `cd` was the head.
    /// That is exactly what put `cd ~/src/example/st…` on three RUNNING NOW rows
    /// (2026-09-13): a path where the row's one line should carry a verb. The class is read
    /// off the remainder and is unchanged by the drop, which is why dropping it is safe.
    ///
    /// The head assertion below was updated again for the identity reduction (`identity`
    /// module): a compound's head used to be the WHOLE reduced text; now it is that text's
    /// IDENTITY — the first non-trivial clause — which is `cargo build` here, not `cargo
    /// build && cargo test`. `class` is untouched, since it still reads the `&&` off the
    /// reduced text before identity ever runs.
    #[test]
    fn a_genuine_compound_loses_its_cd_and_stays_compound() {
        let (head, class) = normalize_bash_command("cd ~/src/example && cargo build && cargo test");
        assert_eq!(head, "cargo build");
        assert_eq!(class, CommandClass::Compound);
    }

    /// The live shape from finding 3: a `cd` in front of a PIPELINE. The row must say what
    /// the pipeline starts with, not which directory it ran in.
    ///
    /// The head is the pipeline's IDENTITY (its first clause, `rg -n TODO`) since the
    /// identity reduction landed, not the whole pipeline text; `class` still reads `Compound`
    /// off the pre-identity reduced text.
    #[test]
    fn a_cd_in_front_of_a_pipeline_is_reduced() {
        let (head, class) = normalize_bash_command("cd ~/src/example && rg -n TODO | head -20");
        assert_eq!(head, "rg -n TODO");
        assert_eq!(class, CommandClass::Compound);
    }

    /// `;` as the separator, with a compound remainder — the same rule as `&&`. Head is the
    /// identity of the first clause (`cargo build`), not the whole `a; b` text, since the
    /// identity reduction landed.
    #[test]
    fn a_cd_with_a_semicolon_separator_is_reduced() {
        let (head, _) = normalize_bash_command("cd ~/src/example; cargo build; cargo test");
        assert_eq!(head, "cargo build");
    }

    /// A bare `cd X` has no clause to reduce TO, so it is left alone — dropping it would
    /// leave the row with an empty head. `cd` is a trivial verb, so identity falls back to
    /// showing the bare `cd` clause itself rather than reducing further.
    #[test]
    fn a_bare_cd_is_left_alone() {
        let (head, _) = normalize_bash_command("cd ~/src/example");
        assert_eq!(head, "cd ~/src/example");
    }

    /// `cd` is matched as a whole word: `cdx` is somebody's own command, not a directory
    /// change, and its first token is what the class is read off. The head is `cdx`'s
    /// identity (its own clause, dropping `&& cargo build`) since the identity reduction
    /// landed — `cdx` is not a trivial verb, so it is not itself reduced further.
    #[test]
    fn a_command_merely_starting_with_cd_is_left_alone() {
        let (head, _) = normalize_bash_command("cdx ~/src/example && cargo build");
        assert_eq!(head, "cdx ~/src/example");
    }

    /// A backslash-continued line is, to the identity reducer, an UNQUOTED newline: it splits
    /// there just as it would on `;`, so the identity is just `cargo test`, dropping the
    /// continuation entirely. `class` is still read off the whole reduced text (still
    /// contains the raw newline, still `Build`) before identity ever runs.
    ///
    /// This assertion used to read `"cargo test \\⏎  -p example"` — the WHOLE reduced text
    /// with its newline swapped for `⏎` for display. That was the pre-identity behavior; see
    /// `an_embedded_quoted_newline_still_becomes_the_display_marker` below for where `⏎` is
    /// still reachable now (a newline INSIDE quotes, which the identity reducer never splits
    /// on).
    #[test]
    fn a_backslash_continued_line_splits_at_the_unquoted_newline() {
        let raw = "cd ~/src/example && cargo test \\\n  -p example";
        let (head, class) = normalize_bash_command(raw);
        assert_eq!(head, "cargo test");
        assert_eq!(class, CommandClass::Build);
    }

    /// A real newline INSIDE quotes is never a split point (only unquoted separators are), so
    /// it survives identity reduction — and [`normalize_bash_command`] still swaps it for `⏎`
    /// before the display cap, exactly as it did for the whole reduced text before the
    /// identity step existed.
    #[test]
    fn an_embedded_quoted_newline_still_becomes_the_display_marker() {
        let (head, _) = normalize_bash_command("printf 'a\nb'");
        assert_eq!(head, "printf 'a⏎b'");
    }

    /// One classification per class name — matches `hooks/log-slow-bash.sh`'s own class
    /// list, so a wire reader and the harness hook agree on the word.
    #[test]
    fn one_classification_per_class_name() {
        let cases: &[(&str, CommandClass)] = &[
            ("wait-for-line.sh --until foo", CommandClass::Wait),
            ("scripts/merge-when-green.sh 123", CommandClass::Wait),
            (
                "until grep -q ready log.txt; do sleep 1; done",
                CommandClass::Wait,
            ),
            ("sleep 30", CommandClass::Wait),
            ("cd ~/src/example && a; b", CommandClass::Compound),
            ("rg -n TODO ~/src/example", CommandClass::Search),
            ("git fetch origin main", CommandClass::GitNet),
            ("git status", CommandClass::GitLocal),
            ("cargo test -p example", CommandClass::Build),
            ("echo hello", CommandClass::Other),
        ];
        for (raw, expected) in cases {
            let (_, class) = normalize_bash_command(raw);
            assert_eq!(class, *expected, "for command {raw:?}");
        }
    }

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
                "check the wire fixtures",
                Some("reviewer"),
            )]},
        ]);
        let raw = messages_raw(messages);
        let (uses, _results) = extract_tool_events(&raw);
        assert_eq!(uses.len(), 1);
        assert_eq!(
            uses[0].command_head.as_deref(),
            Some("reviewer: check the wire fixtures")
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
    fn a_trailing_reminder_message_after_the_tool_result_must_not_hide_the_pair() {
        // Real shape, confirmed against this machine's own Claude Code transcripts
        // (~/.claude/projects/*.jsonl, parentUuid-chained — NOT a cross-thread artifact):
        // Claude Code appends a system-reminder as its OWN trailing `user` message,
        // separate from the `tool_result` message that precedes it, rather than folding
        // the reminder text into the tool_result's own content array. So by the time the
        // client actually fires the next request, the array's last THREE elements are
        // [assistant(tool_use), user(tool_result), user(reminder-only text)] — the
        // matching pair sits at [-3, -2], one step outside "the last two messages".
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [tool_use("tu_reminded", "Bash", Some("ls -la"))]},
            {"role": "user", "content": [tool_result("tu_reminded", false, "ok")]},
            {"role": "user", "content": [{"type": "text", "text": "<system-reminder>...</system-reminder>"}]},
        ]);
        let raw = messages_raw(messages);
        let (uses, results) = extract_tool_events(&raw);

        let mut tracker = WireSessionTracker::new();
        tracker.record_request(
            "sess-reminded",
            Some("alice@example.com".into()),
            Some("claude-x".into()),
            1_000,
            &uses,
            &results,
        );

        let snap = tracker.snapshot(1_000);
        let (_, session) = &snap[0];
        assert_eq!(
            session.tools.calls, 1,
            "the tool call must still be counted even though a trailing reminder message \
             pushed the real pair one slot outside the last two messages"
        );
    }

    #[test]
    fn a_tool_use_then_its_tool_result_yields_one_call_with_the_right_seconds() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_1".into(),
            name: Some("Bash".into()),
            command_head: Some("sleep 5".into()),
            command_class: None,
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

    /// The RUNNING NOW half: a `tool_use` parsed out of the RESPONSE is running from stream
    /// end until the NEXT request's `tool_result` closes it — and that next request replays
    /// the same block without resetting its clock.
    #[test]
    fn a_response_side_tool_use_runs_until_the_next_requests_tool_result() {
        let mut tracker = WireSessionTracker::new();
        // The request that produced the turn — its own body carried no tool events.
        tracker.record_request("sess-r", None, None, 1_000, &[], &[]);

        // The response finished streaming at 2_000 carrying one tool_use.
        let uses = vec![ToolUseEvent {
            id: "tu_1".into(),
            name: Some("Bash".into()),
            command_head: Some("cargo test --release".into()),
            command_class: None,
        }];
        tracker.record_response_tool_uses("sess-r", 2_000, &uses);

        let mid = tracker.snapshot(2_500);
        let (_, session) = &mid[0];
        assert_eq!(
            session.tools.running.len(),
            1,
            "RUNNING NOW is non-empty between the response and the next request"
        );
        assert_eq!(session.tools.running["tu_1"].started_ms, 2_000);
        assert_eq!(
            session.requests, 1,
            "a response-side insert is the same request's second half, not another request"
        );

        // The next request, 7 seconds after the stream ended, replays the same tool_use AND
        // carries its tool_result.
        tracker.record_request(
            "sess-r",
            None,
            None,
            9_000,
            &uses,
            &[ToolResultEvent {
                id: "tu_1".into(),
                is_error: false,
                timed_out: false,
            }],
        );

        let snap = tracker.snapshot(9_000);
        let (_, session) = &snap[0];
        assert!(session.tools.running.is_empty(), "the result closed it");
        assert_eq!(session.tools.calls, 1);
        assert_eq!(
            session.tools.slowest[0].seconds, 7.0,
            "measured from the response-side start (2_000) — the replayed tool_use must not \
             reset it to this request's previous-response instant (1_000, which would read 8.0)"
        );
    }

    /// A response can only add to a session the request half already created: the request is
    /// recorded first, at the same call site, and nothing else may conjure a session.
    #[test]
    fn a_response_side_tool_use_for_an_unknown_session_is_ignored() {
        let mut tracker = WireSessionTracker::new();
        tracker.record_response_tool_uses(
            "sess-never-seen",
            2_000,
            &[ToolUseEvent {
                id: "tu_1".into(),
                name: Some("Bash".into()),
                command_head: None,
                command_class: None,
            }],
        );
        assert!(tracker.snapshot(2_000).is_empty());
    }

    /// Cause 2: every row said `TaskOutput` and nothing else because `command_head` was `None`
    /// for all but Bash/Agent/Task.
    #[test]
    fn command_head_says_what_a_non_bash_tool_is_doing() {
        let cases: [(&str, Value, Option<&str>); 8] = [
            (
                "TaskOutput",
                serde_json::json!({"task_id": "task_9"}),
                Some("TaskOutput · waiting on task task_9"),
            ),
            (
                "Read",
                serde_json::json!({"file_path": "/tmp/example.rs"}),
                Some("Read /tmp/example.rs"),
            ),
            (
                "NotebookEdit",
                serde_json::json!({"notebook_path": "/tmp/example.ipynb"}),
                Some("NotebookEdit /tmp/example.ipynb"),
            ),
            (
                "Grep",
                serde_json::json!({"pattern": "fn main"}),
                Some("Grep fn main"),
            ),
            (
                "Glob",
                serde_json::json!({"pattern": "**/*.rs"}),
                Some("Glob **/*.rs"),
            ),
            (
                // The host only — a path and query carry the search term or record id.
                "WebFetch",
                serde_json::json!({"url": "https://example.com/orders/42?token=abc"}),
                Some("WebFetch example.com"),
            ),
            (
                "WebSearch",
                serde_json::json!({"query": "rust sse parser"}),
                Some("WebSearch rust sse parser"),
            ),
            // A tool with no dedicated rule falls back to its own name as the head — see
            // `a_tool_with_no_dedicated_head_arm_gets_its_own_name_as_the_head` for why `None`
            // here was the #299→#302 bug, not a deliberate fallback.
            (
                "SomeFutureTool",
                serde_json::json!({"whatever": 1}),
                Some("SomeFutureTool"),
            ),
        ];

        for (name, input, expected) in cases {
            let messages = messages_raw(serde_json::json!([
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "tu_1", "name": name, "input": input}],
                }
            ]));
            let (uses, _) = extract_tool_events(&messages);
            assert_eq!(uses.len(), 1, "for {name}");
            assert_eq!(uses[0].command_head.as_deref(), expected, "head for {name}");
            assert!(
                uses[0].command_class.is_none(),
                "class is Bash-only ({name})"
            );
        }
    }

    /// The field the head reads can be absent (a malformed or future input shape): that is a
    /// missing head, never a fabricated one.
    #[test]
    fn a_missing_head_field_leaves_the_head_none() {
        let messages = messages_raw(serde_json::json!([
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "tu_1", "name": "Read", "input": {}}],
            }
        ]));
        let (uses, _) = extract_tool_events(&messages);
        assert_eq!(uses[0].command_head, None);
    }

    /// A head is capped like a Bash command's — a long path cannot widen a panel row.
    #[test]
    fn a_long_head_is_capped_like_a_bash_command() {
        let long_path = format!("/tmp/{}", "a".repeat(400));
        let messages = messages_raw(serde_json::json!([
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": long_path},
                }],
            }
        ]));
        let (uses, _) = extract_tool_events(&messages);
        let head = uses[0].command_head.as_deref().expect("a head");
        assert_eq!(head.chars().count(), COMMAND_HEAD_MAX);
        assert!(head.starts_with("Read /tmp/aaa"));
    }

    #[test]
    fn an_unmatched_tool_use_id_stays_in_running() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_orphan".into(),
            name: Some("Read".into()),
            command_head: None,
            command_class: None,
        }];
        tracker.record_request("sess-b", None, None, 1_000, &uses, &[]);
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.calls, 0);
        assert_eq!(session.tools.running.len(), 1);
        assert!(session.tools.running.contains_key("tu_orphan"));
    }

    /// A terminal tool's `tool_use` (parsed from a response) must never enter `running` — it
    /// will never get a `tool_result`, so an entry here is a ghost that only the six-hour
    /// backstop would ever clear.
    #[test]
    fn a_terminal_tool_use_never_enters_running() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![ToolUseEvent {
            id: "tu_structured".into(),
            name: Some("StructuredOutput".into()),
            command_head: None,
            command_class: None,
        }];
        // `record_response_tool_uses` is the path the live ghosts actually took (a response's
        // own tool_use, inserted at stream-end) — exercise the gate through it directly, on a
        // session `record_request` has already created.
        tracker.record_request("sess-terminal", None, None, 500, &[], &[]);
        tracker.record_response_tool_uses("sess-terminal", 1_000, &uses);
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.running.len(), 0);
    }

    /// The positive control: a real long-running tool (`Agent`) and an ordinary `Bash` call in
    /// the SAME batch as a terminal tool must still land in `running` — proving the gate is
    /// selective to [`TERMINAL_TOOLS`], not a blanket off-switch on `insert_running`.
    #[test]
    fn a_bash_and_agent_tool_use_still_land_in_running_beside_a_terminal_tool() {
        let mut tracker = WireSessionTracker::new();
        let uses = vec![
            ToolUseEvent {
                id: "tu_bash".into(),
                name: Some("Bash".into()),
                command_head: None,
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_agent".into(),
                name: Some("Agent".into()),
                command_head: None,
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_structured".into(),
                name: Some("StructuredOutput".into()),
                command_head: None,
                command_class: None,
            },
        ];
        tracker.record_request("sess-mixed", None, None, 1_000, &uses, &[]);
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.running.len(), 2);
        assert!(session.tools.running.contains_key("tu_bash"));
        assert!(session.tools.running.contains_key("tu_agent"));
        assert!(!session.tools.running.contains_key("tu_structured"));
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
                command_class: None,
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
            command_class: None,
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

    /// A restored session comes back with its finished-call facts and an EMPTY running list.
    ///
    /// The ghost this pins (2026-09-13): the `tool_result` that would close a running entry
    /// was addressed to the process that died, so a restored entry never closes and the panel
    /// draws it as live forever. Counts and `slowest` describe calls that already finished,
    /// so they survive the restart untouched.
    #[test]
    fn restore_drops_running_and_keeps_the_finished_facts() {
        let mut source = WireSessionTracker::new();
        source.record_request("sess-ghost", None, None, 1_000, &[], &[]);
        let uses = vec![
            ToolUseEvent {
                id: "tu_done".into(),
                name: Some("Bash".into()),
                command_head: Some("cargo test --release".into()),
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_ghost".into(),
                name: Some("Bash".into()),
                command_head: Some("sleep 900".into()),
                command_class: None,
            },
        ];
        source.record_response_tool_uses("sess-ghost", 2_000, &uses);
        source.record_request(
            "sess-ghost",
            None,
            None,
            5_000,
            &[],
            &[ToolResultEvent {
                id: "tu_done".into(),
                is_error: false,
                timed_out: false,
            }],
        );
        let saved: std::collections::HashMap<String, WireSession> =
            source.snapshot(5_000).into_iter().collect();
        assert_eq!(
            saved["sess-ghost"].tools.running.len(),
            1,
            "the source table still holds the unclosed entry"
        );

        let mut restored = WireSessionTracker::new();
        restored.restore(saved);
        let snap = restored.snapshot(5_000);
        let (_, session) = &snap[0];
        assert!(
            session.tools.running.is_empty(),
            "no restored entry may be shown as running"
        );
        assert_eq!(session.tools.calls, 1, "the finished call survives");
        assert_eq!(session.tools.slowest.len(), 1);
        assert_eq!(session.requests, 2);
    }

    /// The #299→#302 window wrote `slowest`/`timed_out` entries with `command_head: None` to
    /// `session-wire.json`; restoring one gave a row with no command that sat there until the
    /// session aged out (2026-09-14, `slowest total 180, headless 80` on the live proxy). A
    /// restored session must drop those entries but keep the headed ones, and `calls` — a
    /// fact about calls that happened, head or no head — must not move.
    #[test]
    fn restore_drops_headless_slowest_and_timed_out_entries_but_keeps_calls() {
        let mut source = WireSessionTracker::new();
        source.record_request("sess-headless", None, None, 1_000, &[], &[]);
        let uses = vec![
            ToolUseEvent {
                id: "tu_headless".into(),
                name: Some("Bash".into()),
                command_head: None,
                command_class: None,
            },
            ToolUseEvent {
                id: "tu_headed".into(),
                name: Some("Bash".into()),
                command_head: Some("cargo test --release".into()),
                command_class: None,
            },
        ];
        source.record_response_tool_uses("sess-headless", 2_000, &uses);
        source.record_request(
            "sess-headless",
            None,
            None,
            5_000,
            &[],
            &[
                ToolResultEvent {
                    id: "tu_headless".into(),
                    is_error: false,
                    timed_out: true,
                },
                ToolResultEvent {
                    id: "tu_headed".into(),
                    is_error: false,
                    timed_out: false,
                },
            ],
        );
        let saved: std::collections::HashMap<String, WireSession> =
            source.snapshot(5_000).into_iter().collect();
        assert_eq!(saved["sess-headless"].tools.slowest.len(), 2);
        assert_eq!(saved["sess-headless"].tools.timed_out.len(), 1);
        assert_eq!(saved["sess-headless"].tools.calls, 2);

        let mut restored = WireSessionTracker::new();
        restored.restore(saved);
        let snap = restored.snapshot(5_000);
        let (_, session) = &snap[0];
        assert_eq!(
            session.tools.slowest.len(),
            1,
            "the head-less entry is dropped"
        );
        assert_eq!(
            session.tools.slowest[0].command_head.as_deref(),
            Some("cargo test --release"),
            "the headed entry survives"
        );
        assert!(
            session.tools.timed_out.is_empty(),
            "the head-less timed-out entry is dropped too"
        );
        assert_eq!(
            session.tools.calls, 2,
            "the count describes calls that happened, unaffected by whether a head survived"
        );
    }

    /// The default arm of [`tool_use_event_from_block`] — every tool without a dedicated
    /// arm, `Monitor` included — must fall back to the tool's own name rather than `None`.
    /// This is the other half of the #299→#302 fix: after it, no NEW entry can ever be
    /// head-less, so [`restore`]'s filter above is a no-op on a healthy file.
    #[test]
    fn a_tool_with_no_dedicated_head_arm_gets_its_own_name_as_the_head() {
        let event = tool_use_event_from_block("tu_1".into(), Some("Monitor".into()), None);
        assert_eq!(event.command_head.as_deref(), Some("Monitor"));
    }

    /// A running entry too old to still be running is a LOST result — the projection's own
    /// check, at both limits and on both sides of each.
    #[test]
    fn a_running_entry_past_its_limit_reads_as_lost() {
        assert!(
            !running_tool_is_lost("Bash", 0, RUNNING_BASH_LOST_MS),
            "exactly at the timeout-plus-grace line it may still be running"
        );
        assert!(
            running_tool_is_lost("Bash", 0, RUNNING_BASH_LOST_MS + 1),
            "one ms past it, the result was lost"
        );
        assert!(
            !running_tool_is_lost("Agent", 0, RUNNING_BASH_LOST_MS + 1),
            "a subagent has no Bash timeout — it may legitimately run for hours"
        );
        assert!(
            !running_tool_is_lost("Agent", 0, RUNNING_UNBOUNDED_LOST_MS),
            "exactly at the backstop it is still live"
        );
        assert!(
            running_tool_is_lost("TaskOutput", 0, RUNNING_UNBOUNDED_LOST_MS + 1),
            "one ms past the backstop, any tool is lost"
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
            command_class: None,
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

    /// Two timeouts of different command classes split into `timeouts_by_class` (which sums
    /// to `timeouts`) and land in `timed_out` newest first, carrying the head the call had
    /// while it was running. The classes come from the real classifier via
    /// [`tool_use_event_from_block`], not hand-set, so this test fails if the classifier's
    /// names and the wire's keys ever stop agreeing.
    #[test]
    fn timeouts_split_by_command_class_and_keep_the_commands() {
        let mut tracker = WireSessionTracker::new();
        let wait = tool_use_event_from_block(
            "tu_wait".into(),
            Some("Bash".into()),
            Some(&serde_json::json!({
                "command": "until grep -q ready build.log; do sleep 5; done"
            })),
        );
        let push = tool_use_event_from_block(
            "tu_push".into(),
            Some("Bash".into()),
            Some(&serde_json::json!({ "command": "git push origin main" })),
        );
        assert_eq!(wait.command_class, Some(CommandClass::Wait));
        assert_eq!(push.command_class, Some(CommandClass::GitNet));

        tracker.record_request("sess-t", None, None, 1_000, &[wait, push], &[]);
        // The `until` loop times out first, the `git push` a minute later — so the push is
        // the newer of the two and must lead `timed_out`.
        tracker.record_request(
            "sess-t",
            None,
            None,
            601_000,
            &[],
            &[ToolResultEvent {
                id: "tu_wait".into(),
                is_error: true,
                timed_out: true,
            }],
        );
        tracker.record_request(
            "sess-t",
            None,
            None,
            661_000,
            &[],
            &[ToolResultEvent {
                id: "tu_push".into(),
                is_error: true,
                timed_out: true,
            }],
        );

        let snap = tracker.snapshot(661_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.timeouts, 2);
        assert_eq!(
            session.tools.timeouts_by_class,
            [("wait".to_string(), 1u64), ("git-net".to_string(), 1u64)]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        assert_eq!(
            session.tools.timeouts_by_class.values().sum::<u64>(),
            session.tools.timeouts,
            "the per-class split must sum to the headline count"
        );
        assert_eq!(session.tools.timed_out.len(), 2);
        assert_eq!(
            session.tools.timed_out[0].command_head.as_deref(),
            Some("git push origin main"),
            "newest first"
        );
        assert_eq!(
            session.tools.timed_out[0].command_class,
            Some(CommandClass::GitNet)
        );
        assert_eq!(session.tools.timed_out[0].ended_ms, 661_000);
        // `until` is a control-flow keyword, so identity reduction keeps only its first
        // clause (up to the `;`), dropping `do sleep 5; done` — the loop's OWN condition is
        // what says what it's waiting on, not its body.
        assert_eq!(
            session.tools.timed_out[1].command_head.as_deref(),
            Some("until grep -q ready build.log")
        );
    }

    /// A timed-out call that is not a classified `Bash` command counts under its TOOL name,
    /// so the map still sums to `timeouts` rather than dropping the row on the floor.
    #[test]
    fn a_non_bash_timeout_counts_under_its_tool_name() {
        let mut tracker = WireSessionTracker::new();
        let agent = tool_use_event_from_block(
            "tu_agent".into(),
            Some("Agent".into()),
            Some(&serde_json::json!({ "description": "check the wire fixtures" })),
        );
        assert_eq!(agent.command_class, None);
        tracker.record_request("sess-a", None, None, 1_000, &[agent], &[]);
        tracker.record_request(
            "sess-a",
            None,
            None,
            2_000,
            &[],
            &[ToolResultEvent {
                id: "tu_agent".into(),
                is_error: true,
                timed_out: true,
            }],
        );
        let snap = tracker.snapshot(2_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.timeouts, 1);
        assert_eq!(session.tools.timeouts_by_class.get("Agent"), Some(&1));
    }

    /// `timed_out` is bounded like every other per-session list here, and drops the OLDEST
    /// rows when it overflows — the newest timeout is the one worth reading.
    #[test]
    fn timed_out_is_capped_and_keeps_the_newest() {
        let mut tracker = WireSessionTracker::new();
        for i in 0..(TIMED_OUT_CAP + 5) {
            let id = format!("tu_{i}");
            let use_event = tool_use_event_from_block(
                id.clone(),
                Some("Bash".into()),
                Some(&serde_json::json!({ "command": format!("sleep {i}") })),
            );
            let clock = 1_000 + i as i64 * 1_000;
            tracker.record_request("sess-c", None, None, clock, &[use_event], &[]);
            tracker.record_request(
                "sess-c",
                None,
                None,
                clock + 500,
                &[],
                &[ToolResultEvent {
                    id,
                    is_error: true,
                    timed_out: true,
                }],
            );
        }
        let snap = tracker.snapshot(1_000 + (TIMED_OUT_CAP + 5) as i64 * 1_000);
        let (_, session) = &snap[0];
        assert_eq!(session.tools.timeouts, (TIMED_OUT_CAP + 5) as u64);
        assert_eq!(session.tools.timed_out.len(), TIMED_OUT_CAP);
        assert_eq!(
            session.tools.timed_out[0].command_head.as_deref(),
            Some(format!("sleep {}", TIMED_OUT_CAP + 4).as_str())
        );
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
                    command_class: None,
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
                command_class: None,
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
                command_class: None,
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

    /// Split a golden-fixture TSV into `(raw, expected)` pairs. Almost every row is one
    /// physical line, but a handful of `expected` columns hold a genuine embedded newline
    /// (the reference reduction can retain ONE inside a quote, and the file's own escaping
    /// only escapes a raw command's newlines for column 1 — column 2 is written verbatim). A
    /// continuation line (no tab at all) is folded into the PREVIOUS record's `expected`
    /// field with the real newline it was split on put back.
    fn parse_golden_tsv(contents: &str) -> Vec<(String, String)> {
        let mut records: Vec<(String, String)> = Vec::new();
        for line in contents.lines() {
            match line.split_once('\t') {
                Some((raw, expected)) => records.push((raw.to_string(), expected.to_string())),
                None => {
                    if let Some((_, expected)) = records.last_mut() {
                        expected.push('\n');
                        expected.push_str(line);
                    }
                }
            }
        }
        records
    }

    /// Undo the golden file's escaping of a raw command's embedded newlines (`\n`, two literal
    /// characters) back to a real newline character, so the reconstructed raw text is what
    /// [`identity::identity`] actually runs on. This is LOSSY in one direction the golden file
    /// itself cannot avoid: a command whose ORIGINAL text already contained a literal 2-char
    /// `\n` sequence (a regex escape inside a `sed`/`perl`/`rg` pattern, say) is indistinguishable
    /// in column 1 from an escaped real newline — both are the same two characters once
    /// written to the TSV. `golden_corpus_agreement`'s mismatch count is dominated by exactly
    /// this: proven by feeding the reference Python port itself the same reconstruction (see
    /// the FINAL-REPORT), which reproduces the identical mismatch set — so it is the fixture's
    /// own round-trip, not a port defect.
    fn unescape_golden_command(raw_escaped: &str) -> String {
        raw_escaped.replace("\\n", "\n")
    }

    /// The number of golden rows whose reconstructed raw command is genuinely ambiguous (see
    /// [`unescape_golden_command`]) — the reference Python port itself disagrees with the
    /// golden file on exactly this many rows when fed the identical reconstruction, so this is
    /// the fixture's own irreducible ceiling, not a budget for new Rust-vs-Python drift. A rise
    /// above this number is a real regression; report it.
    const KNOWN_GOLDEN_ROUND_TRIP_AMBIGUITIES: usize = 141;

    /// Agreement check against the 40,589-row golden fixture (real Bash commands harvested
    /// from `~/.claude/projects/`, paired with the reference Python port's expected identity —
    /// see `docs/design/tools-tab.md`). `#[ignore]`d and the fixture is never committed to this
    /// PUBLIC repo: the corpus is one person's real command history (live paths, customer
    /// UUIDs turn up in it), and a test that panics when an external, non-repo file is absent
    /// would break `cargo test --all` for everyone else. Run explicitly:
    /// `cargo test --lib -- --ignored golden_corpus_agreement`, optionally with
    /// `GOLDEN_TSV_PATH` pointing elsewhere.
    #[test]
    #[ignore = "reads an external, non-repo fixture of real command history — see doc comment"]
    fn golden_corpus_agreement() {
        let path = std::env::var("GOLDEN_TSV_PATH")
            .unwrap_or_else(|_| "/tmp/tcr-ghosts/GOLDEN.tsv".to_string());
        let Ok(contents) = std::fs::read_to_string(&path) else {
            eprintln!("golden_corpus_agreement: {path} not present, skipping");
            return;
        };
        let records = parse_golden_tsv(&contents);
        assert!(!records.is_empty(), "parsed zero rows out of {path}");

        let mut mismatches: Vec<(usize, String, String, String)> = Vec::new();
        for (i, (raw_escaped, expected)) in records.iter().enumerate() {
            let raw = unescape_golden_command(raw_escaped);
            let got = identity::identity(&raw);
            if &got != expected {
                mismatches.push((i, raw_escaped.clone(), expected.clone(), got));
            }
        }

        let total = records.len();
        let agree = total - mismatches.len();
        eprintln!(
            "golden_corpus_agreement: {agree}/{total} exact matches, {} mismatches",
            mismatches.len()
        );
        for (i, raw, expected, got) in mismatches.iter().take(5) {
            eprintln!("  row {i}: raw={raw:?}\n    expected={expected:?}\n    got={got:?}");
        }

        assert_eq!(
            mismatches.len(),
            KNOWN_GOLDEN_ROUND_TRIP_AMBIGUITIES,
            "mismatch count moved off the known fixture-ambiguity ceiling — see \
             KNOWN_GOLDEN_ROUND_TRIP_AMBIGUITIES's doc comment"
        );
    }
}
