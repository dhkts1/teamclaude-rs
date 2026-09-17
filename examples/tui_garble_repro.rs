//! Repro harness for issue #323 (TUI panes overwrite each other, never recovers).
//!
//! Mirrors `tui::run`'s terminal handling exactly: the same alternate-screen entry,
//! the same panic hook shape (`src/tui.rs:69`), the same repaint loop. Then a
//! BACKGROUND task panics, as any task in the server may. The screen is captured
//! before and after by the caller (`scripts/repro-323.sh` under tmux).
//!
//! Run: cargo run --example tui_garble_repro -- [panic|clean|fixed|desync|desync-fixed]
//!   panic  — the hook as it shipped: a background panic drops the screen (issue #323)
//!   fixed  — the hook as it stands now: the screen survives and repaints
//!   clean  — the same loop, no panic at all (control)
//!
//! The two `desync` modes are issue #323's SECOND report, on a build that already
//! had the panic fix. Something writes over the alternate screen with nothing
//! in-band for the loop to notice — a frame lost at `Backend::flush`, a stray
//! write, a scroll. ratatui's previous buffer still describes the frame it believes
//! is up, so its diff comes out empty and the damage is permanent.
//!
//!   desync       — no way out: the dashboard stays broken until the process dies
//!   desync-fixed — `r` clears and fully repaints, the escape hatch this adds

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

static SCREEN_DIRTIED_BY_PANIC: AtomicBool = AtomicBool::new(false);

/// Junk straight onto the alternate screen, behind ratatui's back. Stands in for
/// every writer the loop cannot see: a frame dropped at `Backend::flush`, another
/// thread's `println!`, a scroll. The observable result is the same either way —
/// ratatui's previous buffer stops matching the screen, and only a clear repairs it.
fn scribble_over_the_screen() {
    use std::io::Write as _;
    let mut out = io::stdout();
    for line in [
        "thread 'tokio-runtime-worker' something wrote here",
        "  and here, four rows of it",
        "  none of which ratatui knows about",
        "  so none of which it will ever repaint",
    ] {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

/// `broken` is the hook as it shipped: restore on ANY thread's panic.
/// `fixed` is the hook this repro drove us to: only the TUI's own thread ends the
/// process, so only that one takes the screen; anything else asks for a repaint.
fn install_panic_hook(broken: bool) {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let is_tui_thread = std::thread::current().name() == Some("main");
            if broken || is_tui_thread {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                previous(info);
                return;
            }
            SCREEN_DIRTIED_BY_PANIC.store(true, Ordering::SeqCst);
        }));
    });
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "panic".to_string());
    install_panic_hook(mode != "fixed");
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    if mode.starts_with("desync") {
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            scribble_over_the_screen();
        });
    }
    if mode == "panic" || mode == "fixed" {
        // A background task dies, as one in the server may. Nothing here touches
        // the terminal; the hook does it for us.
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            panic!("a background task died");
        });
    }

    for tick in 0..60u32 {
        // `r` is the escape hatch `desync-fixed` demonstrates and `desync` lacks.
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                let redraw = matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'));
                if redraw && mode == "desync-fixed" {
                    SCREEN_DIRTIED_BY_PANIC.store(true, Ordering::SeqCst);
                }
            }
        }
        if SCREEN_DIRTIED_BY_PANIC.swap(false, Ordering::SeqCst) {
            let _ = terminal.clear();
        }
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(8), Constraint::Min(0)])
                .split(frame.area());
            let accounts: Vec<Line> = (0..6)
                .map(|row| {
                    Line::from(vec![
                        Span::styled(
                            format!("account-{row:02}  "),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw(format!("{:>4}  {:>4}  active", row * 7, tick)),
                    ])
                })
                .collect();
            frame.render_widget(
                Paragraph::new(accounts)
                    .block(Block::default().borders(Borders::ALL).title(" accounts ")),
                chunks[0],
            );
            let log: Vec<Line> = (0..10)
                .map(|row| Line::from(format!("200 POST /v1/messages?beta=true  {row}s ago")))
                .collect();
            frame.render_widget(
                Paragraph::new(log).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" recent · q quit · ↑↓/jk select · r redraw "),
                ),
                chunks[1],
            );
        })?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
