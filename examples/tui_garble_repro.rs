//! Repro harness for issue #323 (TUI panes overwrite each other, never recovers).
//!
//! Mirrors `tui::run`'s terminal handling exactly: the same alternate-screen entry,
//! the same panic hook shape (`src/tui.rs:69`), the same repaint loop. Then a
//! BACKGROUND task panics, as any task in the server may. The screen is captured
//! before and after by the caller (`scripts/repro-323.sh` under tmux).
//!
//! Run: cargo run --example tui_garble_repro -- [panic|clean|fixed]
//!   panic — the hook as it shipped: a background panic drops the screen (issue #323)
//!   fixed — the hook as it stands now: the screen survives and repaints
//!   clean — the same loop, no panic at all (control)

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

    if mode == "panic" || mode == "fixed" {
        // A background task dies, as one in the server may. Nothing here touches
        // the terminal; the hook does it for us.
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            panic!("a background task died");
        });
    }

    for tick in 0..30u32 {
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
                        .title(" recent · q quit · ↑↓/jk select "),
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
