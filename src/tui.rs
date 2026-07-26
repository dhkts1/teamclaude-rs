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
use crate::stats::{
    AccountSnapshot, GateReason, QuotaState, SessionKind, SessionSnapshot, StatsSnapshot,
};

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
    // A single-row fleet banner sits above everything — the fleet aggregate the
    // per-row table can't show (how many accounts are actually in rotation, and
    // when the first one returns when none are). The rest of the frame lays out
    // below it exactly as before.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    render_fleet_banner(frame, outer[0], snapshot, now);
    let body = outer[1];
    // Accounts is the primary data: budget its height first so it is the LAST
    // thing clipped. SESSIONS absorbs the vertical slack (grows when the terminal
    // is tall); the recent-log lives in whatever remains, so it shrinks/vanishes
    // before any account row is dropped.
    let acct_h = account_area_height(body.height, snapshot.accounts.len() as u16);
    let rest = body.height.saturating_sub(acct_h);
    let log_h = rest.saturating_sub(SESSIONS_MIN).min(9);
    let sessions_h = rest.saturating_sub(log_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(acct_h),
            Constraint::Length(sessions_h),
            Constraint::Length(log_h),
        ])
        .split(body);
    render_accounts(frame, chunks[0], snapshot, selected, now);
    render_sessions(frame, chunks[1], snapshot, now);
    render_log(frame, chunks[2], snapshot, now);
}

/// One row per account is the wrong altitude for "is the whole fleet up?" — the
/// live incident showed seven individually-busy rows and no aggregate. This
/// distilled fleet status is exactly that: how many accounts are in rotation, of
/// how many, and — only when NONE are — which account returns first and when.
struct FleetStatus {
    eligible: usize,
    total: usize,
    /// The soonest-returning account (name + time-until), computed ONLY when
    /// `eligible == 0`. `None` there means every gated account has an unknown
    /// clear-instant, so the banner says "next free unknown" rather than lie.
    next_free: Option<(String, TimeDuration)>,
}

/// Distil the account snapshots into a [`FleetStatus`]. An account is eligible
/// when its gate is [`GateReason::Ok`] and it is not disabled — the same hard
/// gates selection honours. When none are, the soonest known `free_at` names the
/// first account to return. Pure and terminal-free so it can be unit-tested.
fn fleet_status(accounts: &[AccountSnapshot], now: OffsetDateTime) -> FleetStatus {
    let total = accounts.len();
    let eligible = accounts
        .iter()
        .filter(|a| a.gate == GateReason::Ok && !a.disabled)
        .count();
    let next_free = if eligible == 0 {
        accounts
            .iter()
            .filter_map(|a| a.free_at.filter(|&f| f > now).map(|f| (a.name.clone(), f)))
            .min_by_key(|&(_, f)| f)
            .map(|(name, f)| (name, f - now))
    } else {
        None
    };
    FleetStatus {
        eligible,
        total,
        next_free,
    }
}

/// The single-row fleet banner: `FLEET n/total eligible`, turning red with a
/// `· next free <account> in <rel>` (or `· next free unknown`) tail the moment no
/// account is in rotation — the fleet-exhausted client-facing 429's honest mirror.
fn render_fleet_banner(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    now: OffsetDateTime,
) {
    let status = fleet_status(&snapshot.accounts, now);
    let mut text = format!("FLEET {}/{} eligible", status.eligible, status.total);
    if status.eligible == 0 {
        match status.next_free {
            Some((name, delta)) => {
                text.push_str(&format!(" · next free {name} in {}", rel(delta)));
            }
            None => text.push_str(" · next free unknown"),
        }
    }
    // Red + bold when the fleet is down (0 eligible); a calm green otherwise.
    let style = if status.eligible == 0 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

/// The shared skeleton behind the two data panes ([`render_accounts`] and
/// [`render_sessions`]): a bold header row + caller-mapped rows + column widths,
/// wrapped in an all-borders block with `title`, rendered into `area`. Each
/// caller supplies its own column set and per-row cell builder (incl. any
/// per-row selection/highlight styling); this owns only the identical
/// `Table::new(...).header(...).block(...)` + `render_widget` tail.
fn render_table<'a>(
    frame: &mut Frame,
    area: Rect,
    header: Row<'a>,
    widths: Vec<Constraint>,
    rows: Vec<Row<'a>>,
    title: impl Into<Line<'a>>,
) {
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title.into()));
    frame.render_widget(table, area);
}

/// Which of the two accounts-table layouts a given pane width gets. The choice
/// is a *deterministic breakpoint*, not the constraint solver's squeeze: ratatui
/// silently shrinks over-wide `Length` columns to make a too-wide table fit, and
/// the first casualty is the trailing `%` of each utilization bar — the exact
/// number Gil needs most on a small screen. An honest display degrades by
/// *choosing* what to drop (here the Probe/Cache columns, and the bars in favour
/// of bare percentages) rather than letting the solver clip a number silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountsLayout {
    /// The full 13-column table, unchanged. Chosen at or above
    /// [`FULL_LAYOUT_MIN_WIDTH`].
    Full,
    /// The reduced 11-column table: Probe/Cache dropped, the three quota buckets
    /// rendered as bar-less right-aligned percentages — the number IS the cell,
    /// so it survives any further squeeze. Chosen below [`FULL_LAYOUT_MIN_WIDTH`].
    Compact,
}

/// The narrowest terminal width that still fits the full 13-column table without
/// the constraint solver clipping a column. Kept a literal with the arithmetic
/// shown so a future column edit must update it *consciously* rather than
/// silently re-introducing the squeeze it exists to prevent:
///
/// ```text
///   141  Σ the 13 column widths: 18+3+9+15+11+15+20+15+6+8+7+8+6
/// +  12  column_spacing (1 cell × 12 inter-column gaps)
/// +   2  the block's left + right borders
/// = 155
/// ```
const FULL_LAYOUT_MIN_WIDTH: u16 = 155;

/// Pick the accounts-table layout for a pane `width` — the pure, rendering-free
/// core of the responsive table, so the breakpoint is unit-testable without a
/// terminal. See [`AccountsLayout`] for *why* the breakpoint is deterministic.
fn accounts_layout(width: u16) -> AccountsLayout {
    if width >= FULL_LAYOUT_MIN_WIDTH {
        AccountsLayout::Full
    } else {
        AccountsLayout::Compact
    }
}

/// The accounts table. Responsive: [`AccountsLayout::Full`] renders all 13
/// columns; [`AccountsLayout::Compact`] (below [`FULL_LAYOUT_MIN_WIDTH`]) drops
/// Probe/Cache and renders the quota buckets as bar-less percentages so the
/// utilization numbers stay visible on a narrow pane instead of being silently
/// clipped by the constraint solver.
fn render_accounts(
    frame: &mut Frame,
    area: Rect,
    snapshot: &StatsSnapshot,
    selected: usize,
    now: OffsetDateTime,
) {
    let layout = accounts_layout(area.width);

    let header = match layout {
        AccountsLayout::Full => Row::new(vec![
            "Account", "Pri", "Status", "Gate", "Probe", "5h", "7d", "Fable", "Reqs", "In",
            "Cache", "Out", "Last",
        ]),
        AccountsLayout::Compact => Row::new(vec![
            "Account", "Pri", "Status", "Gate", "5h", "7d", "Fable", "Reqs", "In", "Out", "Last",
        ]),
    }
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = snapshot
        .accounts
        .iter()
        .enumerate()
        .map(|(i, account)| {
            let is_current = snapshot.current == Some(i);
            let marker = if is_current { "▶ " } else { "  " };
            // The weekly quota annotation: an account parked out of rotation on its
            // cap reads as "near"/"full" (yellow/red), while its Status stays
            // "active" — the red "error" is reserved for a dead cred. Same pairing
            // in both layouts (compact keeps the label on the bar-less percentage).
            let (quota_label, quota_style) = quota_cell(account.quota_state);
            // Why this account is out of rotation and when it returns — the
            // per-row half of the fleet banner (`OK` / `5H 47m` / `LOGIN` / …).
            let (gate_label, gate_style) = gate_chip(account, now);
            let last_used = fmt_age_opt(account.last_used, now);

            // Columns shared by both layouts, in their shared order.
            let name = Cell::from(format!("{marker}{}", account.name));
            let priority = Cell::from(account.priority.to_string());
            let status = Cell::from(account.status.clone()).style(status_style(&account.status));
            let gate = Cell::from(gate_label).style(gate_style);
            let reqs = Cell::from(account.requests.to_string());
            let input = Cell::from(fmt_tokens(account.input_tokens));
            let output = Cell::from(fmt_tokens(account.output_tokens));
            let last = Cell::from(last_used);

            let cells = match layout {
                // Full mode: the probe cell, the three 8-cell bars, and the cache
                // ratio — exactly the pre-responsive column set.
                AccountsLayout::Full => {
                    let (probe_label, probe_style) = probe_cell(account, now);
                    vec![
                        name,
                        priority,
                        status,
                        gate,
                        Cell::from(probe_label).style(probe_style),
                        Cell::from(bar(account.five_hour)),
                        Cell::from(format!("{}{quota_label}", bar(account.seven_day)))
                            .style(quota_style),
                        // Model-scoped weekly (the Fable `7d_oi` bucket). Visibility
                        // only: it never gates shared rotation (`eligible` ignores
                        // it), so no quota label — the gate chip already reads
                        // `FABLE-7D` when it parks Fable. `—` until first learned.
                        Cell::from(bar(account.seven_day_oi)),
                        reqs,
                        input,
                        Cell::from(fmt_cache_ratio(
                            account.cache_read_tokens,
                            account.input_tokens,
                        )),
                        output,
                        last,
                    ]
                }
                // Compact mode: Probe and Cache dropped; each quota bucket becomes a
                // bare percentage (`pct`) so the number itself is the cell and
                // survives further squeeze. The 7d bucket keeps its "near"/"full"
                // label + style, exactly as in full mode.
                AccountsLayout::Compact => vec![
                    name,
                    priority,
                    status,
                    gate,
                    Cell::from(pct(account.five_hour)),
                    Cell::from(format!("{}{quota_label}", pct(account.seven_day)))
                        .style(quota_style),
                    Cell::from(pct(account.seven_day_oi)),
                    reqs,
                    input,
                    output,
                    last,
                ],
            };

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
        })
        .collect();

    let widths = match layout {
        AccountsLayout::Full => vec![
            Constraint::Length(18),
            Constraint::Length(3),
            Constraint::Length(9),
            // Gate chip: fits the widest label + back-when (`FABLE-7D 47h30m`).
            Constraint::Length(15),
            Constraint::Length(11),
            // A learned bar is 15 chars (`[########] 100%`) — 14 clipped the `%`.
            Constraint::Length(15),
            // 7d bar + a "near"/"full" quota label — wider than the 5h column.
            Constraint::Length(20),
            // Fable weekly bar, same shape as 5h (no quota label).
            Constraint::Length(15),
            Constraint::Length(6),
            Constraint::Length(8),
            // Cache hit ratio (`cache_read / input`) as a percentage, or "-" when
            // no input has been counted yet.
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(6),
        ],
        // 91 widths + 10 spacing + 2 borders ≈ 103 cols. The three bar columns
        // collapse to bare percentages and Probe/Cache are gone.
        AccountsLayout::Compact => vec![
            Constraint::Length(18),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Length(15),
            // 5h as a bare right-aligned percentage (` 47%`).
            Constraint::Length(4),
            // 7d percentage + its "near"/"full" label (fits `100% full`).
            Constraint::Length(9),
            // Fable percentage; the header word sets the width, not the value.
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(6),
        ],
    };

    // Rows that actually fit = panel height minus header + 2 borders, clamped to
    // the pool size. When `shown < total` the title flags the clip so a hidden
    // account is never silent.
    let total = snapshot.accounts.len() as u16;
    let shown = area.height.saturating_sub(ACCOUNTS_CHROME).min(total);
    render_table(
        frame,
        area,
        header,
        widths,
        rows,
        accounts_title(shown, total),
    );
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
    /// A single session under the account it is PINNED to.
    Session {
        id: String,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
        /// The account that served this session's most recent request, when that
        /// is NOT its pinned account — i.e. the request was diverted while the pin
        /// was held. `None` on the normal case. The session still lives under its
        /// pinned account's header; this only annotates the row.
        diverted_to: Option<String>,
    },
    /// The single collapsed aggregate for ALL fallback sessions — those with no
    /// stable client identity. Rendered LAST, dim, with no children, so unpinned
    /// traffic stays visible as one honest row instead of flooding the pane.
    /// Absent entirely when there are no fallback sessions.
    Unpinned {
        count: usize,
        requests: u64,
        last_seen: Option<OffsetDateTime>,
    },
}

/// Group sessions into a PINNED-account→sessions tree. STABLE sessions build the
/// tree: for each account — in order of its FIRST appearance in the (stably
/// ordered) input — emit an [`TreeRow::Account`] header carrying the group's
/// session count, its summed requests, and the group's YOUNGEST `last_seen`,
/// followed by that account's sessions in input order. Grouping keys on
/// [`SessionSnapshot::account`], the PIN, so a session whose last request was
/// merely diverted keeps its row under its own account and is annotated with
/// `diverted_to` instead of jumping to another group. ALL fallback
/// (non-`stable`) sessions instead fold into one trailing [`TreeRow::Unpinned`]
/// aggregate, rendered LAST and only when non-empty — so identity-less telemetry
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
                // Only a genuine divert annotates the row: the account that served
                // last differs from the one the session is pinned to.
                diverted_to: (session.last_served_account != session.account)
                    .then(|| session.last_served_account.clone()),
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

/// The live sessions pane, drawn as a PINNED-account→sessions tree: each account
/// holding pins becomes a Cyan+bold header `▾ <name> · <count>` carrying the
/// group's summed requests and youngest age, with its sessions indented beneath as
/// short ids — so load balance across accounts and each session's affinity read at
/// a glance. Rows arrive in a stable (account, id) order and [`session_tree`]
/// preserves it, so a row never moves because a request was served.
///
/// A session whose LAST request was diverted off its pin (a Fable title call, one
/// request during a short hold) keeps its row under its own account and gets a dim
/// `→<account>` suffix naming where that one request actually went. That is the
/// honest reading: the pin did not move, so neither does the row.
fn render_sessions(frame: &mut Frame, area: Rect, snapshot: &StatsSnapshot, now: OffsetDateTime) {
    let header = Row::new(vec!["Session", "Reqs", "Last"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let widths = vec![
        Constraint::Length(28),
        Constraint::Length(6),
        Constraint::Length(8),
    ];

    let age = |seen: Option<OffsetDateTime>| fmt_age_opt(seen, now);

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
                    diverted_to,
                } => {
                    // Four-space indent nests the session under its PINNED account.
                    let mut spans = vec![Span::raw(format!("    {}", truncate(&id, 10)))];
                    if let Some(account) = diverted_to {
                        // Dim so the row still reads as belonging to its pin: the
                        // session did not move, one request went elsewhere. Widths:
                        // 4 + 10 + 2 + 10 = 26, inside the 28-cell column.
                        spans.push(Span::styled(
                            format!(" →{}", truncate(&account, 10)),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    Row::new(vec![
                        Cell::from(Line::from(spans)),
                        Cell::from(requests.to_string()),
                        Cell::from(age(last_seen)),
                    ])
                }
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

    render_table(frame, area, header, widths, rows, " sessions ");
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

/// The bar-less quota cell used in [`AccountsLayout::Compact`]: the SAME
/// percentage [`bar`] prints, with the 8-cell bar dropped so the number itself
/// is the whole cell. A plain right-aligned `%` cannot lose its digits to the
/// constraint solver the way a bar's trailing `%` does — which is the entire
/// point of compact mode. `—` mirrors [`bar`]'s never-learned dash so an
/// un-probed bucket reads identically in both layouts.
fn pct(util: Option<f64>) -> String {
    match util {
        None => "—".to_string(),
        Some(util) => format!("{:>3}%", (util * 100.0).round() as i64),
    }
}

/// The probe-health cell: an age since the last probe plus a coloured status.
fn probe_cell(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    let age = fmt_age_opt(account.last_probe, now);
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

/// The per-row gate chip: WHY this account is out of rotation and WHEN it
/// returns, mirroring the [`GateReason`] the manager computed and formatting the
/// `free_at` clear-instant as a compact back-when. `OK`/`OFF` are dim (not a
/// problem); a dead credential's `LOGIN` is red-bold (needs a human); every
/// quota/hold gate is red. An unknown clear-instant drops the back-when (a bare
/// `5H`) — the display never invents a time the manager could not promise.
fn gate_chip(account: &AccountSnapshot, now: OffsetDateTime) -> (String, Style) {
    let red = Style::default().fg(Color::Red);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    // A quota/Fable gate's back-when: the compact `rel`, or just the prefix when
    // the reset is unknown.
    let back = |prefix: &str| match account.free_at {
        Some(f) if f > now => format!("{prefix} {}", rel(f - now)),
        _ => prefix.to_string(),
    };
    match account.gate {
        GateReason::Ok => ("OK".to_string(), dim),
        GateReason::Hold => {
            // A hold is short (<= 1h), so raw seconds read best ("HOLD 12s").
            let label = match account.free_at {
                Some(f) if f > now => format!("HOLD {}s", (f - now).whole_seconds().max(1)),
                _ => "HOLD".to_string(),
            };
            (label, red)
        }
        GateReason::FiveHour => (back("5H"), red),
        GateReason::SevenDay => (back("7D"), red),
        GateReason::FableWeekly => (back("FABLE-7D"), red),
        GateReason::Standard => (back("STD"), red),
        GateReason::Login => (
            "LOGIN".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        GateReason::Disabled => ("OFF".to_string(), dim),
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

/// Format the prompt-cache hit ratio (`cache_read / input_total`) as a whole
/// percentage. R3 GATE: `input_total == 0` renders "-" (never NaN / no divide),
/// so a never-served account shows a dash rather than a bogus 0%.
fn fmt_cache_ratio(cache_read: u64, input_total: u64) -> String {
    if input_total == 0 {
        "-".to_string()
    } else {
        format!("{:.0}%", cache_read as f64 / input_total as f64 * 100.0)
    }
}

/// Age of an optional instant relative to `now`, or an em-dash when absent —
/// the "—"-or-[`fmt_age`] pattern shared by the account, session, and probe cells.
fn fmt_age_opt(seen: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    seen.map_or_else(|| "—".to_string(), |t| fmt_age(now - t))
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

/// A compact "time until" for the fleet banner and gate chips: seconds under a
/// minute, whole minutes under an hour, `HhMMm` under two days, whole days
/// beyond. Distinct from [`fmt_age`] (a single-unit ELAPSED age): a gate's
/// back-when keeps hour+minute detail within a day so `2h05m` is not flattened
/// to `2h`.
fn rel(d: TimeDuration) -> String {
    let secs = d.whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h{:02}m", secs / 3_600, (secs % 3_600) / 60)
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
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

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

    /// A session sitting on its pin — pinned to and last served by `account`.
    fn snap(id: &str, account: &str, requests: u64, last_seen: Option<i64>) -> SessionSnapshot {
        SessionSnapshot {
            id: id.to_string(),
            account: account.to_string(),
            last_served_account: account.to_string(),
            requests,
            last_seen: last_seen.map(at),
            kind: SessionKind::Stable,
        }
    }

    /// A session PINNED to `account` whose most recent request was DIVERTED to
    /// `served_by` — the pin never moved.
    fn diverted(
        id: &str,
        account: &str,
        served_by: &str,
        requests: u64,
        last_seen: Option<i64>,
    ) -> SessionSnapshot {
        SessionSnapshot {
            last_served_account: served_by.to_string(),
            ..snap(id, account, requests, last_seen)
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
    fn session_tree_groups_a_diverted_session_under_its_pin() {
        // `d1` is pinned to acct-a; its last request was diverted to acct-b. It
        // must stay in acct-a's group — the pin never moved — and carry the
        // divert as an annotation rather than a change of home.
        let sessions = vec![
            snap("a1", "acct-a", 4, Some(100)),
            diverted("d1", "acct-a", "acct-b", 7, Some(90)),
        ];
        let rows = session_tree(&sessions);

        assert_eq!(rows.len(), 3, "one header, two sessions — NO acct-b group");
        assert_eq!(header(&rows[0]), ("acct-a", 2));
        match (&rows[1], &rows[2]) {
            (
                TreeRow::Session {
                    diverted_to: home, ..
                },
                TreeRow::Session {
                    id,
                    diverted_to: away,
                    ..
                },
            ) => {
                assert_eq!(id, "d1");
                assert_eq!(*home, None, "a session sitting on its pin is not annotated");
                assert_eq!(
                    away.as_deref(),
                    Some("acct-b"),
                    "the diverted session names where its one request actually went"
                );
            }
            _ => panic!("rows 1 and 2 should both be sessions"),
        }
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

    /// A minimal account snapshot carrying just the fields the gate chip and fleet
    /// banner read — everything else defaulted — for those render tests.
    fn snap_gate(name: &str, gate: GateReason, free_at: Option<OffsetDateTime>) -> AccountSnapshot {
        AccountSnapshot {
            name: name.to_string(),
            priority: 0,
            status: "active".to_string(),
            disabled: matches!(gate, GateReason::Disabled),
            five_hour: None,
            five_hour_reset: None,
            seven_day: None,
            seven_day_reset: None,
            seven_day_oi: None,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            last_used: None,
            rate_limited_until: None,
            probe_status: ProbeStatus::Never,
            last_probe: None,
            probe_error: None,
            quota_state: QuotaState::Normal,
            gate,
            free_at,
        }
    }

    /// A stable, far-from-epoch anchor so `now + delta` never underflows.
    fn anchor() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + TimeDuration::days(3650)
    }

    #[test]
    fn rel_buckets() {
        // Under a minute: raw seconds.
        assert_eq!(rel(TimeDuration::seconds(0)), "0s");
        assert_eq!(rel(TimeDuration::seconds(45)), "45s");
        assert_eq!(rel(TimeDuration::seconds(59)), "59s");
        // Under an hour: whole minutes.
        assert_eq!(rel(TimeDuration::seconds(60)), "1m");
        assert_eq!(rel(TimeDuration::minutes(47)), "47m");
        assert_eq!(rel(TimeDuration::minutes(59)), "59m");
        // Under 48h: hours + zero-padded minutes.
        assert_eq!(rel(TimeDuration::hours(1)), "1h00m");
        assert_eq!(rel(TimeDuration::minutes(125)), "2h05m");
        assert_eq!(
            rel(TimeDuration::hours(47) + TimeDuration::minutes(30)),
            "47h30m"
        );
        // 48h and beyond: whole days.
        assert_eq!(rel(TimeDuration::hours(48)), "2d");
        assert_eq!(rel(TimeDuration::days(3)), "3d");
        // Negative (a reset already in the past) clamps to zero.
        assert_eq!(rel(TimeDuration::seconds(-10)), "0s");
    }

    #[test]
    fn gate_chip_labels_each_reason() {
        let now = anchor();
        let at = |secs: i64| Some(now + TimeDuration::seconds(secs));

        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Ok, None), now).0,
            "OK"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Login, None), now).0,
            "LOGIN"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Disabled, None), now).0,
            "OFF"
        );
        // A hold shows raw seconds ("HOLD 12s").
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::Hold, at(12)), now).0,
            "HOLD 12s"
        );
        // Quota/Fable gates carry the compact `rel` back-when.
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::FiveHour, at(47 * 60)), now).0,
            "5H 47m"
        );
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::SevenDay, at(2 * 86_400)), now).0,
            "7D 2d"
        );
        assert_eq!(
            gate_chip(
                &snap_gate("a", GateReason::FableWeekly, at(3 * 86_400)),
                now
            )
            .0,
            "FABLE-7D 3d"
        );
        // An unknown clear-instant drops the back-when — the display never invents
        // a time the manager could not promise.
        assert_eq!(
            gate_chip(&snap_gate("a", GateReason::FiveHour, None), now).0,
            "5H"
        );
    }

    #[test]
    fn fleet_status_counts_eligible_and_skips_next_free() {
        let now = anchor();
        let accounts = vec![
            snap_gate("a", GateReason::Ok, None),
            snap_gate(
                "b",
                GateReason::FiveHour,
                Some(now + TimeDuration::seconds(300)),
            ),
            snap_gate("c", GateReason::Ok, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!((status.eligible, status.total), (2, 3));
        // Some account is in rotation, so no "next free" is computed.
        assert!(status.next_free.is_none());
    }

    #[test]
    fn fleet_status_names_soonest_recovery_when_none_eligible() {
        let now = anchor();
        // All gated; b returns first (300s) even though it is listed second, and the
        // never-self-freeing Login account is skipped.
        let accounts = vec![
            snap_gate(
                "a",
                GateReason::SevenDay,
                Some(now + TimeDuration::seconds(5_000)),
            ),
            snap_gate(
                "b",
                GateReason::FiveHour,
                Some(now + TimeDuration::seconds(300)),
            ),
            snap_gate("login", GateReason::Login, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!(status.eligible, 0);
        let (name, delta) = status.next_free.expect("some account has a known free_at");
        assert_eq!(name, "b");
        assert_eq!(delta, TimeDuration::seconds(300));
    }

    #[test]
    fn fleet_status_unknown_when_all_gated_without_reset() {
        let now = anchor();
        // Every account is out with NO known clear-instant → next_free is None, so
        // the banner honestly says "unknown" rather than promising a time.
        let accounts = vec![
            snap_gate("a", GateReason::Login, None),
            snap_gate("b", GateReason::Disabled, None),
        ];
        let status = fleet_status(&accounts, now);
        assert_eq!(status.eligible, 0);
        assert!(status.next_free.is_none());
    }

    /// Flatten a rendered buffer into one string per row so a render test can
    /// assert on the visible text without caring about cell geometry.
    fn buffer_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// A one-account snapshot with learned quota buckets (5h 47%, 7d 90% at
    /// `state`, Fable 62%) for the render tests — adapts [`snap_gate`], whose
    /// buckets are all `None`.
    fn util_snapshot(state: QuotaState) -> StatsSnapshot {
        let mut account = snap_gate("acct", GateReason::Ok, None);
        account.five_hour = Some(0.47);
        account.seven_day = Some(0.9);
        account.seven_day_oi = Some(0.62);
        account.quota_state = state;
        StatsSnapshot {
            accounts: vec![account],
            current: Some(0),
            recent: vec![],
            sessions: vec![],
        }
    }

    #[test]
    fn accounts_layout_picks_mode_at_threshold() {
        // The breakpoint is exact: FULL_LAYOUT_MIN_WIDTH still fits the full
        // table; one column narrower drops to compact.
        assert_eq!(accounts_layout(FULL_LAYOUT_MIN_WIDTH), AccountsLayout::Full);
        assert_eq!(
            accounts_layout(FULL_LAYOUT_MIN_WIDTH - 1),
            AccountsLayout::Compact
        );
    }

    #[test]
    fn pct_renders_percentage_or_dash() {
        // The same number `bar` prints, minus the bar; `—` when never learned.
        assert_eq!(pct(Some(0.47)), " 47%");
        assert_eq!(pct(Some(1.0)), "100%");
        assert_eq!(pct(None), "—");
    }

    #[test]
    fn render_wide_keeps_full_columns_and_bars() {
        // A pane at/above the threshold gets every column and the 8-cell bars.
        let snapshot = util_snapshot(QuotaState::Normal);
        let backend = TestBackend::new(170, 12);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_accounts(frame, frame.area(), &snapshot, 0, anchor()))
            .expect("render succeeds");
        let text = buffer_rows(terminal.backend().buffer()).join("\n");

        assert!(text.contains("Probe"), "full mode shows the Probe header");
        assert!(text.contains("Cache"), "full mode shows the Cache header");
        assert!(text.contains('['), "full mode draws at least one bar cell");
    }

    #[test]
    fn render_narrow_shows_percentages_and_drops_probe_cache() {
        // Below the threshold: bar-less percentages, the 7d quota label, and NO
        // Probe/Cache columns — the numbers survive the squeeze by construction.
        let snapshot = util_snapshot(QuotaState::Exhausted);
        let backend = TestBackend::new(104, 12);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_accounts(frame, frame.area(), &snapshot, 0, anchor()))
            .expect("render succeeds");
        let text = buffer_rows(terminal.backend().buffer()).join("\n");

        assert!(
            text.contains("Fable"),
            "compact mode keeps the Fable header"
        );
        assert!(text.contains(" 47%"), "compact shows the 5h percentage");
        assert!(text.contains("62%"), "compact shows the Fable percentage");
        assert!(text.contains("full"), "compact keeps the 7d quota label");
        assert!(!text.contains("Probe"), "compact drops the Probe column");
        assert!(!text.contains("Cache"), "compact drops the Cache column");
    }

    #[test]
    fn render_sessions_keeps_a_diverted_session_under_its_pin() {
        // One session pinned to `alice` whose most recent request was diverted to
        // `bob`. The pane must show it as alice's — with the divert marked — and
        // must NOT open a bob group, because no pin moved.
        let snapshot = StatsSnapshot {
            accounts: vec![],
            current: None,
            recent: vec![],
            sessions: vec![diverted("a1f3", "alice", "bob", 24, Some(100))],
        };
        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).expect("test backend builds a terminal");
        terminal
            .draw(|frame| render_sessions(frame, frame.area(), &snapshot, at(160)))
            .expect("render succeeds");
        let rows = buffer_rows(terminal.backend().buffer());
        let text = rows.join("\n");

        assert!(
            text.contains("▾ alice · 1"),
            "the session is grouped under its PINNED account\n{text}"
        );
        assert!(
            !text.contains("▾ bob"),
            "an account that merely served one diverted request gets NO group\n{text}"
        );
        let pin_row = rows
            .iter()
            .position(|row| row.contains("▾ alice"))
            .expect("the alice header is drawn");
        let session_row = rows
            .iter()
            .position(|row| row.contains("a1f3"))
            .expect("the session row is drawn");
        assert!(
            session_row > pin_row,
            "the session nests beneath its pin's header\n{text}"
        );
        assert!(
            rows[session_row].contains("→bob"),
            "the divert stays visible as a marker on the row\n{text}"
        );
    }
}
