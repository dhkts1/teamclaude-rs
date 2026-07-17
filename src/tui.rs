//! The live ratatui dashboard: one row per account with quota bars, probe
//! health, and counters, plus a recent-request log. Repaints on a 500ms tick so
//! every quota bar is recomputed live against its reset — a window past its
//! reset can never render as still-full (the display half of bug #2).
//!
//! Terminal safety (behaviour #7): a [`TerminalGuard`] restores the terminal on
//! **any** exit path (normal, `?`, or unwind), and a panic hook restores it
//! before the default panic printer runs — so a crash never leaves the user in
//! raw-mode alt-screen. A single failed repaint is logged and swallowed rather
//! than crashing the loop, and paste / resize / focus events are non-fatal.

use std::collections::HashMap;
use std::io::{self};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::manager::Manager;
use crate::probe::ProbeStatus;
use crate::stats::{AccountSnapshot, QuotaState, SessionKind, SessionSnapshot, StatsSnapshot};

/// Restores the terminal to a sane state whenever it is dropped — normal exit,
/// an early `?`, or a panic unwind. Constructing it enters raw-mode + the
/// alternate screen; dropping it leaves them, best-effort (a `Drop` never panics).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
    }
}

/// Install a panic hook (once) that restores the terminal before the previous
/// hook prints the panic — otherwise a panic mid-render leaves a corrupt screen.
fn install_panic_hook() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
            previous(info);
        }));
    });
}

/// What a key event asks the loop to do.
enum Action {
    None,
    Quit,
    Up,
    Down,
    Disable,
    Enable,
}

/// Run the dashboard until the user quits (`q` or `Ctrl-C`). Returns once the
/// terminal has been restored by [`TerminalGuard`]'s drop.
pub async fn run(manager: Arc<Manager>) -> io::Result<()> {
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let mut selected: usize = 0;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let snapshot = manager.snapshot(OffsetDateTime::now_utc());
                let count = snapshot.accounts.len();
                if count == 0 {
                    selected = 0;
                } else if selected >= count {
                    selected = count - 1;
                }
                // A single failed repaint must not crash the process.
                if let Err(err) = terminal.draw(|frame| render(frame, &snapshot, selected)) {
                    tracing::warn!(error = %err, "tui repaint failed");
                }
            }
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => match key_action(&key) {
                        Action::Quit => break,
                        Action::Up => selected = selected.saturating_sub(1),
                        Action::Down => selected = selected.saturating_add(1),
                        Action::Disable => manager.set_disabled(selected, true),
                        Action::Enable => manager.set_disabled(selected, false),
                        Action::None => {}
                    },
                    // Paste / resize / focus / mouse are non-fatal; a multi-char
                    // paste can never crash the loop.
                    Some(Ok(_)) => {}
                    Some(Err(err)) => tracing::warn!(error = %err, "tui input error"),
                    None => break, // input stream closed
                }
            }
        }
    }
    Ok(())
}

/// Map a key press to an [`Action`]. Key-release events (kitty/Windows) are
/// ignored so a keypress does not fire twice.
fn key_action(key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Char('c') | KeyCode::Char('C')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Action::Quit
        }
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('d') | KeyCode::Char('D') => Action::Disable,
        KeyCode::Char('e') | KeyCode::Char('E') => Action::Enable,
        _ => Action::None,
    }
}

/// Keep the sessions pane usable even when the terminal is short: the accounts
/// panel never eats so much height that fewer than this many rows remain for the
/// rest of the frame.
const SESSIONS_MIN: u16 = 3;

/// Full height (rows) the accounts panel wants: one row per account + a header
/// row + top/bottom borders.
const ACCOUNTS_CHROME: u16 = 3;

/// Height (rows) the accounts panel should get. It takes its full height whenever
/// at least `SESSIONS_MIN` rows remain for the rest of the frame; when the terminal
/// is too short for that, it yields down to `total_height - SESSIONS_MIN` (so the
/// recent-log shrinks/vanishes entirely before a single account row is dropped),
/// but never below a 4-row floor so a tiny terminal still shows the header plus at
/// least one account.
fn account_area_height(total_height: u16, n_accounts: u16) -> u16 {
    let acct_full = n_accounts.saturating_add(ACCOUNTS_CHROME);
    acct_full
        .min(total_height.saturating_sub(SESSIONS_MIN))
        .max(4)
}

/// The accounts panel title: honest about how many accounts are actually on
/// screen. `" teamclaude-rs · accounts (7) "` when all fit, or
/// `" teamclaude-rs · accounts (5/7 ▼) "` when rows are clipped.
fn accounts_title(shown: u16, total: u16) -> String {
    if shown >= total {
        format!(" teamclaude-rs · accounts ({total}) ")
    } else {
        format!(" teamclaude-rs · accounts ({shown}/{total} ▼) ")
    }
}

/// Paint the whole frame: accounts table on top, sessions pane in the middle,
/// request log below.
fn render(frame: &mut Frame, snapshot: &StatsSnapshot, selected: usize) {
    let now = OffsetDateTime::now_utc();
    let area = frame.area();
    // Accounts is the primary data: budget its height first so it is the LAST
    // thing clipped. SESSIONS absorbs the vertical slack (grows when the terminal
    // is tall); the recent-log lives in whatever remains, so it shrinks/vanishes
    // before any account row is dropped.
    let acct_h = account_area_height(area.height, snapshot.accounts.len() as u16);
    let rest = area.height.saturating_sub(acct_h);
    let log_h = rest.saturating_sub(SESSIONS_MIN).min(9);
    let sessions_h = rest.saturating_sub(log_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(acct_h),
            Constraint::Length(sessions_h),
            Constraint::Length(log_h),
        ])
        .split(area);
    render_accounts(frame, chunks[0], snapshot, selected, now);
    render_sessions(frame, chunks[1], snapshot, now);
    render_log(frame, chunks[2], snapshot, now);
}

/// The accounts table.
fn render_accounts(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    selected: usize,
    now: OffsetDateTime,
) {
    let header = Row::new(vec![
        "Account", "Pri", "Status", "Probe", "5h", "7d", "Reqs", "In", "Out", "Last",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows = snapshot.accounts.iter().enumerate().map(|(i, account)| {
        let is_current = snapshot.current == Some(i);
        let marker = if is_current { "▶ " } else { "  " };
        let (probe_label, probe_style) = probe_cell(account, now);
        // The weekly bar carries an honest quota label: an account parked out of
        // rotation on its cap reads as "near"/"full" (yellow/red on the BAR), while
        // its Status stays "active" — the red "error" is reserved for a dead cred.
        let (quota_label, quota_style) = quota_cell(account.quota_state);
        let last_used = account
            .last_used
            .map(|t| fmt_age(now - t))
            .unwrap_or_else(|| "—".to_string());

        let cells = vec![
            Cell::from(format!("{marker}{}", account.name)),
            Cell::from(account.priority.to_string()),
            Cell::from(account.status.clone()).style(status_style(&account.status)),
            Cell::from(probe_label).style(probe_style),
            Cell::from(bar(account.five_hour)),
            Cell::from(format!("{}{quota_label}", bar(account.seven_day))).style(quota_style),
            Cell::from(account.requests.to_string()),
            Cell::from(fmt_tokens(account.input_tokens)),
            Cell::from(fmt_tokens(account.output_tokens)),
            Cell::from(last_used),
        ];

        let mut row = Row::new(cells);
        if account.disabled {
            row = row.style(
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            );
        }
        if i == selected {
            row = row.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        row
    });

    let widths = [
        Constraint::Length(18),
        Constraint::Length(3),
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Length(14),
        // 7d bar + a "near"/"full" quota label — wider than the 5h column.
        Constraint::Length(20),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
    ];

    // Rows that actually fit = panel height minus header + 2 borders, clamped to
    // the pool size. When `shown < total` the title flags the clip so a hidden
    // account is never silent.
    let total = snapshot.accounts.len() as u16;
    let shown = area.height.saturating_sub(ACCOUNTS_CHROME).min(total);
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(accounts_title(shown, total)),
    );
    frame.render_widget(table, area);
}

/// One row in the sessions pane's account→session tree: either an account
/// header carrying the group's aggregates, or one of that account's sessions.
enum TreeRow {
    /// A group header: the account name, its session count, its summed request
    /// total, and the youngest `last_seen` across the group.
    Account {
        name: String,
        count: usize,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
    /// A single session under the account above it.
    Session {
        id: String,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
    /// The single collapsed aggregate for ALL per-connection (fallback) sessions —
    /// those with no stable client identity. Rendered LAST, dim, with no children,
    /// so unpinned traffic stays visible as one honest row instead of flooding the
    /// pane. Absent entirely when there are no fallback sessions.
    Unpinned {
        count: usize,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
}

/// Group most-recent-first sessions into an account→sessions tree. STABLE sessions
/// build the tree: for each account — in order of its FIRST appearance, so the
/// most-recently-active account leads — emit an [`TreeRow::Account`] header carrying
/// the group's session count, its summed requests, and the group's YOUNGEST
/// `last_seen`, followed by that account's sessions in input order. ALL fallback
/// (non-`stable`) sessions instead fold into one trailing [`TreeRow::Unpinned`]
/// aggregate, rendered LAST and only when non-empty — so per-connection telemetry
/// traffic collapses to one dim row rather than flooding the pane. Pure and
/// terminal-free so it can be unit-tested directly.
fn session_tree(sessions: &[SessionSnapshot]) -> Vec<TreeRow> {
    // Group STABLE members by account while recording each account's first-appearance
    // order — an empty freshly-inserted bucket marks a not-yet-seen account. Fallback
    // sessions bypass the tree and accumulate into the single unpinned aggregate.
    let mut order: Vec<&str> = Vec::new();
    let mut groups: HashMap<&str, Vec<&SessionSnapshot>> = HashMap::new();
    let mut unpinned_count = 0usize;
    let mut unpinned_requests = 0u64;
    let mut unpinned_last_seen: Option<OffsetDateTime> = None;
    for session in sessions {
        if session.kind == SessionKind::Fallback {
            unpinned_count += 1;
            unpinned_requests += session.requests;
            // Youngest last_seen across the fallback set; a never-seen session
            // (None) is skipped so it can never win over one that has been seen.
            if let Some(seen) = session.last_seen {
                unpinned_last_seen = Some(unpinned_last_seen.map_or(seen, |cur| cur.max(seen)));
            }
            continue;
        }
        let members = groups.entry(session.account.as_str()).or_default();
        if members.is_empty() {
            order.push(session.account.as_str());
        }
        members.push(session);
    }

    let mut rows = Vec::with_capacity(sessions.len() + order.len() + 1);
    for account in order {
        let members = &groups[account];
        let requests = members.iter().map(|s| s.requests).sum();
        // Youngest last_seen is the max instant; a never-seen session (None) is
        // filtered out so it can never win over one that has been seen.
        let last_seen = members.iter().filter_map(|s| s.last_seen).max();
        rows.push(TreeRow::Account {
            name: account.to_string(),
            count: members.len(),
            requests,
            last_seen,
        });
        for session in members {
            rows.push(TreeRow::Session {
                id: session.id.clone(),
                requests: session.requests,
                last_seen: session.last_seen,
            });
        }
    }
    // The unpinned aggregate is always LAST and present only when non-empty.
    if unpinned_count > 0 {
        rows.push(TreeRow::Unpinned {
            count: unpinned_count,
            requests: unpinned_requests,
            last_seen: unpinned_last_seen,
        });
    }
    rows
}

/// The live sessions pane, drawn as an account→sessions tree: each account being
/// served becomes a Cyan+bold header `▾ <name> · <count>` carrying the group's
/// summed requests and youngest age, with its sessions indented beneath as short
/// ids — so load balance across accounts and each session's affinity read at a
/// glance. Rows arrive most-recent-first; [`session_tree`] preserves that recency
/// both across account groups and within each group.
fn render_sessions(frame: &mut Frame, area: Rect, snapshot: &StatsSnapshot, now: OffsetDateTime) {
    let header = Row::new(vec!["Session", "Reqs", "Last"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let widths = [
        Constraint::Length(28),
        Constraint::Length(6),
        Constraint::Length(8),
    ];

    let age = |seen: Option<OffsetDateTime>| {
        seen.map(|t| fmt_age(now - t))
            .unwrap_or_else(|| "—".to_string())
    };

    let capacity = area.height.saturating_sub(3) as usize;
    let rows: Vec<Row> = if snapshot.sessions.is_empty() {
        // Empty pane reads clearly when affinity is off / nothing served yet.
        vec![Row::new(vec![
            Cell::from("(no active sessions)").style(Style::default().fg(Color::DarkGray))
        ])]
    } else {
        session_tree(&snapshot.sessions)
            .into_iter()
            .take(capacity.max(1))
            .map(|row| match row {
                TreeRow::Account {
                    name,
                    count,
                    requests,
                    last_seen,
                } => Row::new(vec![
                    Cell::from(format!("▾ {} · {count}", truncate(&name, 20))),
                    Cell::from(requests.to_string()),
                    Cell::from(age(last_seen)),
                ])
                // Cyan + bold header matches the log/account cell styling.
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                TreeRow::Session {
                    id,
                    requests,
                    last_seen,
                } => Row::new(vec![
                    // Four-space indent nests the session under its account.
                    Cell::from(format!("    {}", truncate(&id, 10))),
                    Cell::from(requests.to_string()),
                    Cell::from(age(last_seen)),
                ]),
                TreeRow::Unpinned {
                    count,
                    requests,
                    last_seen,
                } => Row::new(vec![
                    Cell::from(format!("▸ (unpinned) · {count} conns")),
                    Cell::from(requests.to_string()),
                    Cell::from(age(last_seen)),
                ])
                // Dim gray de-emphasizes fallback traffic, matching disabled rows.
                .style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            })
            .collect()
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" sessions "));
    frame.render_widget(table, area);
}

/// The recent-request log pane. Rows are already most-recent-first.
fn render_log(frame: &mut Frame, area: Rect, snapshot: &StatsSnapshot, now: OffsetDateTime) {
    let capacity = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = snapshot
        .recent
        .iter()
        .take(capacity)
        .map(|entry| {
            Line::from(vec![
                Span::styled(
                    format!("{:>3} ", entry.status),
                    Style::default().fg(status_color(entry.status)),
                ),
                Span::raw(format!("{:<6} ", entry.method)),
                Span::raw(format!("{:<24} ", truncate(&entry.path, 24))),
                Span::styled(entry.account.clone(), Style::default().fg(Color::Cyan)),
                Span::raw(format!("  {} ago", fmt_age(now - entry.time))),
            ])
        })
        .collect();

    let log = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" recent · q quit · ↑↓/jk select · d/e disable/enable "),
    );
    frame.render_widget(log, area);
}

/// An 8-cell utilization bar plus a percentage, or a dash if never learned.
fn bar(util: Option<f64>) -> String {
    const WIDTH: usize = 8;
    match util {
        None => format!("[{}]   —", "·".repeat(WIDTH)),
        Some(util) => {
            let clamped = util.clamp(0.0, 1.0);
            let filled = (clamped * WIDTH as f64).round() as usize;
            let filled = filled.min(WIDTH);
            let mut bar = String::with_capacity(WIDTH + 8);
            bar.push('[');
            for cell in 0..WIDTH {
                bar.push(if cell < filled { '#' } else { '·' });
            }
            bar.push(']');
            bar.push_str(&format!(" {:>3}%", (util * 100.0).round() as i64));
            bar
        }
    }
}

/// The probe-health cell: an age since the last probe plus a coloured status.
fn probe_cell(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    let age = account
        .last_probe
        .map(|t| fmt_age(now - t))
        .unwrap_or_else(|| "—".to_string());
    match account.probe_status {
        ProbeStatus::Ok => (format!("ok {age}"), Style::default().fg(Color::Green)),
        ProbeStatus::Error => (format!("ERR {age}"), Style::default().fg(Color::Red)),
        ProbeStatus::Timeout => (format!("T/O {age}"), Style::default().fg(Color::Red)),
        // Endpoint busy/throttled (usage-endpoint 429 or a transient upstream 5xx)
        // — benign, not a serving failure. Yellow, never red: the account's own
        // quota bar is still valid; only the probe was deflected.
        ProbeStatus::RateLimited => (format!("busy {age}"), Style::default().fg(Color::Yellow)),
        ProbeStatus::Never => ("never".to_string(), Style::default().fg(Color::DarkGray)),
    }
}

/// The weekly-bar quota annotation: a short honest label plus a colour, driven
/// by how close the account is to its own threshold. Never red-for-error — a
/// quota-parked account is operationally active; only the utilization is high.
fn quota_cell(state: QuotaState) -> (&'static str, Style) {
    match state {
        QuotaState::Normal => ("", Style::default()),
        QuotaState::NearLimit => (" near", Style::default().fg(Color::Yellow)),
        QuotaState::Exhausted => (" full", Style::default().fg(Color::Red)),
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Green),
        "throttled" => Style::default().fg(Color::Yellow),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

fn status_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        429 => Color::Magenta,
        400..=499 => Color::Yellow,
        500..=599 => Color::Red,
        _ => Color::Gray,
    }
}

/// Humanize a token count: `1234` → `1.2k`, `2_000_000` → `2.0M`.
fn fmt_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// Humanize a duration as a compact age (`45s`, `12m`, `3h`, `2d`).
fn fmt_age(delta: TimeDuration) -> String {
    let secs = delta.whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Truncate a string to `max` chars, appending `…` when it was cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_area_height_full_when_tall() {
        // Plenty of room: accounts gets its full height (7 rows + header + 2 borders).
        assert_eq!(account_area_height(40, 7), 10);
    }

    #[test]
    fn account_area_height_yields_log_before_clipping_accounts() {
        // Fits accounts (10) + SESSIONS_MIN (3) = 13 but not the full 9-row log:
        // the log shrank, every account is still shown.
        assert_eq!(account_area_height(13, 7), 10);
    }

    #[test]
    fn account_area_height_clips_only_when_forced() {
        // Genuinely tiny: accounts yields to total - SESSIONS_MIN, but never below 4.
        assert_eq!(account_area_height(8, 7), 5);
        assert!(account_area_height(8, 7) >= 4);
        assert_eq!(account_area_height(4, 7), 4);
    }

    #[test]
    fn accounts_title_shows_total_when_all_visible() {
        assert_eq!(accounts_title(7, 7), " teamclaude-rs · accounts (7) ");
    }

    #[test]
    fn accounts_title_flags_clip() {
        assert_eq!(accounts_title(5, 7), " teamclaude-rs · accounts (5/7 ▼) ");
    }

    #[test]
    fn bar_fills_proportionally_and_shows_percent() {
        assert!(bar(Some(0.0)).contains("0%"));
        assert!(bar(Some(1.0)).starts_with("[########]"));
        assert!(bar(Some(1.0)).contains("100%"));
        // Overage clamps the fill but still reports the real percentage.
        assert!(bar(Some(1.5)).starts_with("[########]"));
        assert!(bar(Some(1.5)).contains("150%"));
        assert!(bar(None).contains('—'));
    }

    #[test]
    fn fmt_tokens_humanizes() {
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn fmt_age_buckets() {
        assert_eq!(fmt_age(TimeDuration::seconds(5)), "5s");
        assert_eq!(fmt_age(TimeDuration::seconds(125)), "2m");
        assert_eq!(fmt_age(TimeDuration::hours(3)), "3h");
        assert_eq!(fmt_age(TimeDuration::seconds(-10)), "0s");
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("/v1/messages/long/path", 8), "/v1/mes…");
    }

    /// A fixed instant `secs` after the epoch — larger `secs` is younger, so the
    /// group's youngest `last_seen` is the max of these.
    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + TimeDuration::seconds(secs)
    }

    fn snap(id: &str, account: &str, requests: u64, last_seen: Option<i64>) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            account: account.to_string(),
            requests,
            last_seen: last_seen.map(at),
            kind: SessionKind::Stable,
        }
    }

    /// A fallback (unpinned) session — no stable client identity, so it folds into
    /// the single trailing aggregate row instead of the account tree.
    fn unpinned(id: &str, account: &str, requests: u64, last_seen: Option<i64>) -> SessionSnapshot {
        SessionSnapshot {
            kind: SessionKind::Fallback,
            ..snap(id, account, requests, last_seen)
        }
    }

    /// The (name, count) of an account header row, or panic — keeps the ordering
    /// assertions short and readable.
    fn header(row: &TreeRow) -> (&str, usize) {
        match row {
            TreeRow::Account { name, count, .. } => (name.as_str(), *count),
            TreeRow::Session { id, .. } => panic!("expected an account header, got session {id}"),
            TreeRow::Unpinned { count, .. } => {
                panic!("expected an account header, got unpinned aggregate of {count}")
            }
        }
    }

    /// The id of a session row, or panic.
    fn session_id(row: &TreeRow) -> &str {
        match row {
            TreeRow::Session { id, .. } => id.as_str(),
            TreeRow::Account { name, .. } => panic!("expected a session, got header {name}"),
            TreeRow::Unpinned { count, .. } => {
                panic!("expected a session, got unpinned aggregate of {count}")
            }
        }
    }

    #[test]
    fn session_tree_preserves_recency_order_across_and_within_groups() {
        // Most-recent-first input: acct-a, then acct-b, then acct-a again.
        let sessions = vec![
            snap("a1", "acct-a", 1, Some(100)),
            snap("b1", "acct-b", 1, Some(90)),
            snap("a2", "acct-a", 1, Some(80)),
        ];
        let rows = session_tree(&sessions);

        // acct-a appeared first, so its group leads with both sessions in input
        // order, then acct-b's group.
        assert_eq!(rows.len(), 5);
        assert_eq!(header(&rows[0]), ("acct-a", 2));
        assert_eq!(session_id(&rows[1]), "a1");
        assert_eq!(session_id(&rows[2]), "a2");
        assert_eq!(header(&rows[3]), ("acct-b", 1));
        assert_eq!(session_id(&rows[4]), "b1");
    }

    #[test]
    fn session_tree_account_row_sums_requests_and_takes_youngest_last_seen() {
        let sessions = vec![
            snap("a1", "acct-a", 12, Some(50)),
            snap("a2", "acct-a", 8, Some(200)), // youngest of the group
            snap("a3", "acct-a", 5, None),      // never seen — must not win
        ];
        let rows = session_tree(&sessions);

        match &rows[0] {
            TreeRow::Account {
                count,
                requests,
                last_seen,
                ..
            } => {
                assert_eq!(*count, 3);
                assert_eq!(*requests, 25);
                assert_eq!(*last_seen, Some(at(200)));
            }
            _ => panic!("row 0 should be the account header"),
        }
    }

    #[test]
    fn session_tree_single_account_yields_one_header_and_all_children() {
        let sessions = vec![
            snap("s1", "solo", 1, Some(10)),
            snap("s2", "solo", 1, Some(9)),
            snap("s3", "solo", 1, Some(8)),
        ];
        let rows = session_tree(&sessions);

        assert_eq!(rows.len(), 4); // one header + three sessions
        assert_eq!(header(&rows[0]), ("solo", 3));
        for row in &rows[1..] {
            assert!(
                matches!(row, TreeRow::Session { .. }),
                "children must be sessions"
            );
        }
    }

    #[test]
    fn session_tree_empty_input_is_empty() {
        assert!(session_tree(&[]).is_empty());
    }

    #[test]
    fn session_tree_folds_all_unpinned_into_one_trailing_row() {
        // One stable session builds the account tree; three fallback sessions
        // across two accounts must collapse into a single aggregate row.
        let sessions = vec![
            snap("a1", "acct-a", 3, Some(100)),
            unpinned("u1", "acct-a", 5, Some(90)),
            unpinned("u2", "acct-b", 7, Some(80)),
            unpinned("u3", "acct-a", 2, None), // never seen — must not win last_seen
        ];
        let rows = session_tree(&sessions);

        // acct-a header + its single stable session, then ONE unpinned aggregate.
        assert_eq!(rows.len(), 3);
        assert_eq!(header(&rows[0]), ("acct-a", 1));
        assert_eq!(session_id(&rows[1]), "a1");
        match &rows[2] {
            TreeRow::Unpinned {
                count,
                requests,
                last_seen,
            } => {
                assert_eq!(*count, 3); // three fallback conns, account-agnostic
                assert_eq!(*requests, 14); // 5 + 7 + 2
                assert_eq!(*last_seen, Some(at(90))); // youngest across the set
            }
            _ => panic!("row 2 should be the unpinned aggregate"),
        }
    }

    #[test]
    fn session_tree_unpinned_row_renders_last() {
        // A fallback session leads the recency-ordered input, yet the aggregate
        // must still render AFTER every account group.
        let sessions = vec![
            unpinned("u1", "acct-a", 1, Some(100)),
            snap("a1", "acct-a", 1, Some(90)),
            snap("b1", "acct-b", 1, Some(80)),
        ];
        let rows = session_tree(&sessions);

        let last = rows.last().expect("rows are non-empty");
        assert!(
            matches!(last, TreeRow::Unpinned { count: 1, .. }),
            "the unpinned aggregate must be the final row"
        );
        // Nothing before the final row is an unpinned aggregate.
        for row in &rows[..rows.len() - 1] {
            assert!(
                !matches!(row, TreeRow::Unpinned { .. }),
                "only the last row may be the unpinned aggregate"
            );
        }
    }

    #[test]
    fn session_tree_no_unpinned_row_when_all_stable() {
        let sessions = vec![
            snap("a1", "acct-a", 1, Some(10)),
            snap("a2", "acct-a", 1, Some(9)),
        ];
        let rows = session_tree(&sessions);
        assert!(
            !rows.iter().any(|r| matches!(r, TreeRow::Unpinned { .. })),
            "all-stable input must yield no unpinned row"
        );
    }
}
