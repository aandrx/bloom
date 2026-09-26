mod auth;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::io;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "Bloom TUI\nUsage: bloom-tui [--help]\nMatrix login support is provided by bloom-core."
        );
        return Ok(());
    }
    let _core = auth::authenticate().await?;
    let mut terminal_guard = TerminalGuard::enter()?;
    run(terminal_guard.output())
}
struct TerminalGuard {
    output: io::Stdout,
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalGuard {
    fn setup_phase<F, E>(cleanup_flag: &mut bool, setup: F) -> std::result::Result<(), E>
    where
        F: FnOnce() -> std::result::Result<(), E>,
    {
        *cleanup_flag = true;
        setup()
    }

    fn enter() -> Result<Self> {
        let mut guard = Self {
            output: io::stdout(),
            raw_mode: false,
            alternate_screen: false,
        };
        Self::setup_phase(&mut guard.raw_mode, enable_raw_mode)?;
        Self::setup_phase(&mut guard.alternate_screen, || {
            execute!(guard.output, EnterAlternateScreen)
        })?;
        Ok(guard)
    }

    fn output(&mut self) -> &mut io::Stdout {
        &mut self.output
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.alternate_screen {
            let _ = execute!(self.output, LeaveAlternateScreen);
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

fn run(out: &mut io::Stdout) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(28), Constraint::Min(30)])
                .split(area);
            f.render_widget(
                Paragraph::new(
                    "BLOOM  ·  PLACEHOLDER\n\nMessaging UI is not implemented yet\n\n[q] quit",
                )
                .block(Block::default().borders(Borders::ALL).title(" Bloom ")),
                chunks[0],
            );
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(chunks[1]);
            f.render_widget(
                Paragraph::new("No conversation selected")
                    .block(Block::default().borders(Borders::ALL)),
                right[0],
            );
            f.render_widget(
                Paragraph::new("Welcome to Bloom\n\nEncrypted conversations, without the noise.")
                    .block(Block::default().borders(Borders::ALL).title(" Transcript ")),
                right[1],
            );
            f.render_widget(
                Paragraph::new("Messaging is not available yet")
                    .block(Block::default().borders(Borders::ALL).title(" Composer ")),
                right[2],
            );
        })?;
        if let Event::Key(key) = event::read()? {
            if key.code == KeyCode::Char('q') {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TerminalGuard;
    use std::io;

    #[test]
    fn setup_failure_leaves_cleanup_flag_set() {
        let mut cleanup_flag = false;
        let result =
            TerminalGuard::setup_phase(&mut cleanup_flag, || Err(io::Error::other("setup failed")));

        assert!(result.is_err());
        assert!(cleanup_flag);
    }
}
