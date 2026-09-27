//! Bloom TUI: a Bloom-native client for the daemon over its Unix-socket IPC.

use anyhow::{Context, Result};
use bloom_domain::{MessagePart, OutgoingBody};
use bloom_ipc::{client::BloomClient, EventKind};
use bloom_tui::app::App;
use bloom_tui::daemon::{self, ConnectionState, Snapshot};
use bloom_tui::ui;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, time::Duration};
use tokio::sync::{mpsc, watch};

const FIRST_RETRY: Duration = Duration::from_millis(250);
const MAX_RETRY: Duration = Duration::from_secs(8);

/// Restores the terminal even when the UI exits through an error path.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn socket_path() -> PathBuf {
    std::env::var_os("BLOOM_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/run/user")
                .join(unsafe { libc::geteuid() }.to_string())
                .join("bloom/bloomd.sock")
        })
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|argument| argument == "--help" || argument == "-h") {
        println!(
            "Bloom TUI\nUsage: bloom-tui [--help]\n\
             Reads BLOOM_SOCKET_PATH (or BLOOM_SOCKET) for the socket path.\n\
             Keys: up/down select, Enter compose, Enter send, Esc cancel, q quit."
        );
        return Ok(());
    }

    let client = BloomClient::new(socket_path());
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App {
        connection: ConnectionState::Connecting.label().to_owned(),
        ..Default::default()
    };

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let (visible_tx, visible_rx) = watch::channel::<Option<bloom_domain::ConversationId>>(None);

    {
        let client = client.clone();
        tokio::spawn(async move {
            let mut delay = FIRST_RETRY;
            loop {
                match daemon::pump_session(
                    &client,
                    visible_rx.clone(),
                    &event_tx,
                    &snapshot_tx,
                    &status_tx,
                )
                .await
                {
                    Ok(()) => delay = FIRST_RETRY,
                    Err(_) => {
                        let _ = status_tx.send(ConnectionState::Disconnected);
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_RETRY);
                    }
                }
            }
        });
    }

    // Read input on a dedicated thread. Reading on the main loop let a single
    // blocking read freeze rendering, so the UI showed stale state.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if key_tx.send(key).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });

    let mut last_visible = app.selected.clone();
    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            Some(snapshot) = snapshot_rx.recv() => {
                app.apply_snapshot(snapshot);
                last_visible = app.selected.clone();
                let _ = visible_tx.send(last_visible.clone());
            }
            Some(state) = status_rx.recv() => {
                app.connection = state.label().to_owned();
            }
            Some(event) = event_rx.recv() => app.apply_event(event),
            Some(result) = send_rx.recv() => {
                if let Err(error) = result {
                    app.error = Some(error);
                }
            }
            Some(key) = key_rx.recv() => {
                let quit = key.code == KeyCode::Char('q') && !app.composing
                    || key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                if quit {
                    break;
                }
                if app.composing {
                    match key.code {
                        KeyCode::Esc => app.cancel_composition(),
                        KeyCode::Enter => {
                            let intent = app.submit_intent();
                            let bloom_tui::app::CompositionIntent::SendMessage(body) = intent else {
                                app.error = Some("Start conversation request is awaiting BloomClient support".into());
                                continue;
                            };
                            if let (Some(id), false) = (app.selected.clone(), body.is_empty()) {
                                let client = client.clone();
                                let send_tx = send_tx.clone();
                                tokio::spawn(async move {
                                    let result = client
                                        .send(id, OutgoingBody {
                                            parts: vec![MessagePart::Text(body)],
                                        })
                                        .await
                                        .context("send failed")
                                        .map(|_| ())
                                        .map_err(|error| error.to_string());
                                    let _ = send_tx.send(result);
                                });
                                app.draft.clear();
                            }
                            app.composing = false;
                        }
                        KeyCode::Backspace => {
                            app.draft.pop();
                        }
                        KeyCode::Char(c) => app.draft.push(c),
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Down => app.select_step(1),
                        KeyCode::Up => app.select_step(-1),
                        KeyCode::Char('n') => app.begin_new_conversation(),
                        KeyCode::Enter => app.composing = true,
                        _ => {}
                    }
                    // A selection change is a request for a fresh read, not an event.
                    if app.selected != last_visible {
                        last_visible = app.selected.clone();
                        let _ = visible_tx.send(last_visible.clone());
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }

    Ok(())
}
