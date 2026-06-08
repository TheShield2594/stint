//! Interactive TUI dashboard for Stint.

mod app;
mod timeline;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use stint_core::storage::sqlite::SqliteStorage;

use self::app::App;

/// RAII guard that restores terminal state on drop.
struct TerminalGuard;

impl TerminalGuard {
    /// Enters raw mode and alternate screen, returning the guard.
    fn init() -> Result<Self, Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Runs the interactive dashboard.
///
/// Opens the database, enters the alternate screen, and runs the event loop
/// until the user quits. Terminal state is restored on exit, error, or panic
/// via the TerminalGuard RAII guard.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = SqliteStorage::default_path();
    let storage = SqliteStorage::open(&path)?;

    // Set up terminal — guard restores state on any exit path
    let _guard = TerminalGuard::init()?;

    // Install panic hook to restore terminal on crash
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(storage);

    // Event loop
    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        // Poll for events with 1-second timeout (drives the live timer tick)
        if event::poll(Duration::from_secs(1))? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => app.should_quit = true,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,
                    (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                        app.selected_panel = app.selected_panel.next();
                    }
                    (KeyCode::Char('y'), _) => {
                        app.selected_panel = app::Panel::Timeline;
                        app.toggle_timeline_view();
                    }
                    (KeyCode::Char('t'), _) => {
                        app.selected_panel = app::Panel::Timeline;
                        if !app.is_timeline_today() {
                            app.toggle_timeline_view();
                        }
                    }
                    (KeyCode::Up, _) => app.scroll_up(),
                    (KeyCode::Down, _) => app.scroll_down(),
                    _ => {}
                }
            }
        } else {
            // Timeout — refresh data
            app.refresh();
        }

        if app.should_quit {
            break;
        }
    }

    // _guard drops here, restoring terminal state
    Ok(())
}
