use anyhow::Result;
use bloom_core::{Bloom, OsKeyring};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::{io, path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "Bloom TUI\nUsage: bloom-tui [--help]\nMatrix login support is provided by bloom-core."
        );
        return Ok(());
    }
    let secrets = Arc::new(OsKeyring);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let core = Bloom::open(
        &std::env::var("BLOOM_HOMESERVER").unwrap_or_else(|_| "".into()),
        &home.join(".local/share/bloom"),
        "default",
        secrets,
    )
    .await?;
    if !core.restore().await? {
        anyhow::bail!("No saved session. Login UI is not yet implemented.");
    }
    let _room_summaries = core.rooms().await;
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let result = run(&mut out);
    disable_raw_mode()?;
    execute!(out, LeaveAlternateScreen)?;
    result
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
                Paragraph::new("BLOOM  ·  PLACEHOLDER\n\nUI is not implemented yet\n\n[q] quit")
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
                Paragraph::new("No conversation selected  ·  E2EE")
                    .block(Block::default().borders(Borders::ALL)),
                right[0],
            );
            f.render_widget(
                Paragraph::new("Welcome to Bloom\n\nEncrypted conversations, without the noise.")
                    .block(Block::default().borders(Borders::ALL).title(" Transcript ")),
                right[1],
            );
            f.render_widget(
                Paragraph::new("Type a message…     Enter to send")
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
