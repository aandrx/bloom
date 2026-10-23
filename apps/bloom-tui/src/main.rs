//! Bloom TUI: a Bloom-native client for the daemon over its Unix-socket IPC.

use anyhow::Result;
use bloom_domain::{MessageId, MessagePart, OutgoingBody};
use bloom_ipc::{
    client::{BloomClient, ClientError},
    EventKind, ProtocolError,
};
use bloom_tui::app::{App, SendFailure, Theme, OLDER_PAGE_LIMIT, SEARCH_RESULT_LIMIT};
use bloom_tui::config::Config;
use bloom_tui::daemon::{self, ConnectionState, Snapshot};
use bloom_tui::diagnostics::{self, Stage};
use bloom_tui::ui;
use bloom_tui::RedrawScheduler;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::{io, path::PathBuf, time::Duration};
use tokio::sync::{mpsc, watch};

const FIRST_RETRY: Duration = Duration::from_millis(250);
const MAX_RETRY: Duration = Duration::from_secs(8);
/// How long a quiet inbound stream must last before catch-up is complete.
const SYNC_IDLE: Duration = Duration::from_millis(2000);
const OSC_TIMEOUT: Duration = Duration::from_millis(200);

fn parse_osc11_reply(reply: Option<&[u8]>) -> Option<(u8, u8, u8)> {
    let bytes = reply?;
    if let Some(index) = bytes.windows(1).position(|window| window == b"#") {
        let hex = std::str::from_utf8(bytes.get(index + 1..index + 7)?).ok()?;
        if bytes
            .get(index + 7)
            .is_some_and(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        return Some((
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ));
    }
    let start = bytes.windows(4).position(|window| window == b"rgb:")? + 4;
    let payload = std::str::from_utf8(&bytes[start..])
        .ok()?
        .trim_end_matches(['\u{7}', '\u{1b}', '\\']);
    let mut channels = payload.split('/');
    let parse = |value: &str| -> Option<u8> {
        if value.is_empty() || value.len() > 4 {
            return None;
        }
        let raw = u16::from_str_radix(value, 16).ok()?;
        Some((raw as u32 * 255 / ((1u32 << (value.len() * 4)) - 1)) as u8)
    };
    let rgb = (
        parse(channels.next()?)?,
        parse(channels.next()?)?,
        parse(channels.next()?)?,
    );
    if channels.next().is_some() {
        return None;
    }
    Some(rgb)
}

fn sync_failure_message(error: &ClientError) -> String {
    match error {
        ClientError::Connect => "Could not connect to Bloom. Conversation sync did not start.".into(),
        ClientError::Timeout => {
            "Conversation sync timed out. It may finish in the background; refresh the list shortly."
                .into()
        }
        ClientError::Server(ProtocolError::Unsupported) => {
            "This engine does not support conversation-list sync.".into()
        }
        _ => "Conversation-list sync failed. Check the Bloom connection and try again.".into(),
    }
}

fn theme_for_rgb((r, g, b): (u8, u8, u8)) -> bloom_tui::app::Theme {
    if (r as u16 + g as u16 + b as u16) / 3 >= 128 {
        bloom_tui::app::Theme::Light
    } else {
        bloom_tui::app::Theme::Dark
    }
}

fn query_background_theme() -> Option<bloom_tui::app::Theme> {
    if !std::io::IsTerminal::is_terminal(&io::stdin())
        || !std::io::IsTerminal::is_terminal(&io::stdout())
    {
        return None;
    }
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    tty.write_all(b"\x1b]11;?\x07").ok()?;
    tty.flush().ok()?;
    let start = std::time::Instant::now();
    let mut reply = Vec::new();
    while start.elapsed() < OSC_TIMEOUT && reply.len() < 128 {
        let remaining = OSC_TIMEOUT.saturating_sub(start.elapsed());
        let fd = std::os::fd::AsRawFd::as_raw_fd(&tty);
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut poll_fd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready <= 0 {
            break;
        }
        let mut byte = [0];
        if tty.read(&mut byte).ok()? == 0 {
            break;
        }
        reply.push(byte[0]);
        if byte[0] == 7 || reply.ends_with(b"\\") {
            break;
        }
    }
    parse_osc11_reply(Some(&reply)).map(theme_for_rgb)
}

/// Restores the terminal even when the UI exits through an error path.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

/// The same resolution the daemon uses, so an override (including one from .env)
/// cannot put the UI on a different socket than the daemon.
fn socket_path() -> PathBuf {
    std::env::var_os("BLOOM_SOCKET_PATH")
        .or_else(|| std::env::var_os("BLOOM_SOCKET"))
        .map(PathBuf::from)
        .unwrap_or_else(bloom_ipc::default_socket_path)
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().any(|argument| argument == "--help" || argument == "-h") {
        println!(
            "Bloom TUI\nUsage: bloom-tui [--help]\n\
             Reads BLOOM_SOCKET_PATH (or BLOOM_SOCKET) for the socket path.\n\
             Keys: up/down scroll or select, Enter send/choose, Esc dismisses, Ctrl+C clears input then quits when empty.\n\
             Herdr-style: Ctrl+B then n/p or h/l changes chat; Ctrl+P goes previous; j/k scroll. Use /new to start a chat."
        );
        return Ok(());
    }

    let client = BloomClient::new(socket_path());
    let config_path = Config::path();
    let (mut config, config_warning) = Config::load(&config_path);
    let _guard = TerminalGuard::enter()?;
    let system_theme = query_background_theme().unwrap_or_default();
    let theme_override = match config.theme.as_deref() {
        Some("dark") => Some(Theme::Dark),
        Some("light") => Some(Theme::Light),
        _ => None,
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let read_positions = config.read_positions.clone();
    let mut app = App {
        theme: theme_override.unwrap_or(system_theme),
        system_theme,
        theme_override,
        compact_sidebar: config.compact_sidebar,
        wrap_composer: config.wrap_composer,
        nicknames: config.nicknames.clone(),
        titles: config.titles.clone(),
        hidden: config.hidden.iter().cloned().collect(),
        error: config_warning,
        connection: ConnectionState::Connecting.label().to_owned(),
        connection_state: ConnectionState::Connecting,
        ..Default::default()
    };
    app.apply_read_positions(&read_positions);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<(
        u64,
        Option<bloom_domain::ConversationId>,
        Result<MessageId, SendFailure>,
    )>();
    let (conversation_tx, mut conversation_rx) =
        mpsc::unbounded_channel::<Vec<bloom_domain::Conversation>>();
    let (sync_tx, mut sync_rx) = mpsc::unbounded_channel();
    let (older_tx, mut older_rx) = mpsc::unbounded_channel::<(
        bloom_domain::ConversationId,
        Result<Vec<bloom_domain::Message>, ClientError>,
    )>();
    let (search_tx, mut search_rx) =
        mpsc::unbounded_channel::<(u64, Result<Vec<bloom_ipc::MessageSearchHit>, ClientError>)>();
    let (context_tx, mut context_rx) = mpsc::unbounded_channel::<(
        u64,
        bloom_domain::ConversationId,
        MessageId,
        Result<Vec<bloom_domain::Message>, ClientError>,
    )>();
    let (visible_tx, visible_rx) = watch::channel::<Option<bloom_domain::ConversationId>>(None);

    {
        let client = client.clone();
        let snapshot_tx = snapshot_tx.clone();
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
                        diagnostics::record("connection state=disconnected");
                        let _ = status_tx.send(ConnectionState::Disconnected);
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_RETRY);
                    }
                }
            }
        });
    }

    // Read input on a dedicated thread. Reading on the main loop let a single
    // blocking read freeze rendering, so the UI showed stale state. Only key
    // presses and repeats are forwarded: a terminal that reports key releases (the
    // enhanced keyboard protocol) must not fire every binding twice.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if input_tx.send(Event::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(event @ (Event::Mouse(_) | Event::Paste(_) | Event::Resize(_, _))) => {
                    if input_tx.send(event).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });

    let mut last_visible = app.selected.clone();
    let mut redraw = RedrawScheduler::default();
    let mut hits = ui::HitMap::default();
    let mut render_cache = ui::RenderCache::default();
    let mut inbound_redraws = VecDeque::<(u64, std::time::Instant)>::new();
    // The daemon has no explicit catch-up completion frame, so treat a burst of
    // inbound events as syncing until the stream has been quiet for a moment.
    let mut last_sync_activity: Option<std::time::Instant> = None;
    redraw.request();
    let mut next_animation = tokio::time::Instant::now();
    loop {
        if redraw.take_due(std::time::Instant::now()) {
            app.set_terminal_width(terminal.size()?.width);
            // Rendering returns the hitboxes it drew, so mouse input is resolved
            // against the frame the user is actually looking at.
            terminal.draw(|frame| {
                hits = ui::render_with_cache(frame, &app, &mut render_cache);
            })?;
            if app.record_transcript(hits.transcript_view.take()) {
                // The just-painted badges predate the newly observed read marker.
                // Coalesce one follow-up frame instead of waiting for more input.
                redraw.request();
            }
            let drawn_at = std::time::Instant::now();
            while let Some((sequence, received_at)) = inbound_redraws.pop_front() {
                diagnostics::inbound_timing(
                    "event_to_redraw",
                    sequence,
                    drawn_at.duration_since(received_at),
                );
            }
        }

        // Read positions can advance after a successful draw or an asynchronous
        // refresh, not just during input handling. Persist the completed frame's
        // state before waiting for the next event.
        let updated_config = Config {
            version: 1,
            theme: app.theme_override.map(|theme| match theme {
                Theme::Dark => "dark".to_owned(),
                Theme::Light => "light".to_owned(),
            }),
            compact_sidebar: app.compact_sidebar,
            wrap_composer: app.wrap_composer,
            nicknames: app.nicknames.clone(),
            titles: app.titles.clone(),
            hidden: {
                let mut hidden: Vec<String> = app.hidden.iter().cloned().collect();
                hidden.sort();
                hidden
            },
            read_positions: app.read_positions_for_config(),
        };
        if updated_config != config {
            match updated_config.save(&config_path) {
                Ok(()) => config = updated_config,
                Err(error) => {
                    app.error = Some(format!("Could not save local settings: {error}"));
                }
            }
        }

        let redraw_delay = redraw.wake_after(std::time::Instant::now());
        tokio::select! {
            _ = async {
                if let Some(delay) = redraw_delay {
                    tokio::time::sleep(delay).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
            Some(snapshot) = snapshot_rx.recv() => {
                app.thread_loading = false;
                app.apply_snapshot(snapshot);
                app.advance_search_jump();
                redraw.request();
                if app.selected != last_visible {
                    last_visible = app.selected.clone();
                    let _ = visible_tx.send(last_visible.clone());
                }
            }
            Some(conversations) = conversation_rx.recv() => {
                app.set_conversations(conversations);
                redraw.request();
            },
            Some(state) = status_rx.recv() => {
                app.connection = state.label().to_owned();
                app.connection_state = state;
                redraw.request();
                if app.is_animating() { next_animation = tokio::time::Instant::now() + Duration::from_millis(33); }
            }
            Some(event) = event_rx.recv() => {
                let received_at = std::time::Instant::now();
                let push_sequence = match &event {
                    EventKind::Message { push_sequence, .. } => *push_sequence,
                    _ => None,
                };
                let apply_started = std::time::Instant::now();
                let is_inbound = matches!(
                    event,
                    EventKind::Message { .. }
                        | EventKind::Update { .. }
                        | EventKind::Delivery { .. }
                        | EventKind::ConversationsChanged
                );
                app.apply_event(event);
                if is_inbound {
                    last_sync_activity = Some(received_at);
                    app.set_syncing(true);
                }
                if let Some(sequence) = push_sequence {
                    diagnostics::inbound_timing("event_apply", sequence, apply_started.elapsed());
                    inbound_redraws.push_back((sequence, received_at));
                }
                redraw.request();
                if app.resync_conversations {
                    app.resync_conversations = false;
                    let client = client.clone();
                    let tx = conversation_tx.clone();
                    let snapshots = snapshot_tx.clone();
                    let selected = app.selected.clone();
                    tokio::spawn(async move {
                        match daemon::snapshot(&client, selected).await {
                            Ok(snapshot) => { let _ = snapshots.send(snapshot); }
                            Err(_) => {
                                if let Ok(conversations) = daemon::conversations(&client).await {
                                    let _ = tx.send(conversations);
                                }
                            }
                        }
                    });
                }
            }
            Some((attempt, conversation, result)) = send_rx.recv() => {
                app.finish_send_receipt(attempt, conversation, result);
                redraw.request();
            },
            Some((conversation, result)) = older_rx.recv() => {
                match result {
                    Ok(page) => {
                        app.finish_load_older(conversation.clone(), page, OLDER_PAGE_LIMIT);
                        if app
                            .search_jump
                            .as_ref()
                            .is_some_and(|(id, _)| id == &conversation)
                        {
                            app.advance_search_jump();
                        }
                    }
                    Err(_) => {
                        let jump_here = app
                            .search_jump
                            .as_ref()
                            .is_some_and(|(id, _)| id == &conversation);
                        app.fail_load_older(
                            conversation,
                            "Older messages could not be loaded.".into(),
                        );
                        if jump_here {
                            app.abandon_search_jump();
                            app.notice =
                                Some("Older messages could not be loaded.".into());
                        }
                    }
                }
                redraw.request();
            },
            Some((request, result)) = search_rx.recv() => {
                match result {
                    Ok(hits) => app.finish_search(request, hits),
                    Err(_) => app.fail_search(request, "Message search failed.".into()),
                }
                redraw.request();
            },
            Some((generation, conversation, target, result)) = context_rx.recv() => {
                match result {
                    Ok(messages) => {
                        app.finish_message_context(generation, conversation, target, messages);
                    }
                    Err(_) => {
                        app.fail_message_context(generation, conversation, target);
                    }
                }
                redraw.request();
            },
            Some(result) = sync_rx.recv() => {
                app.finish_sync(result);
                // `/sync` refreshes metadata, not remote message history. Re-read
                // the selected thread from Bloom's store so already persisted
                // messages cannot leave the transcript stale.
                let client = client.clone();
                let snapshots = snapshot_tx.clone();
                let selected = app.selected.clone();
                tokio::spawn(async move {
                    if let Ok(snapshot) = daemon::snapshot(&client, selected).await {
                        let _ = snapshots.send(snapshot);
                    }
                });
                redraw.request();
            },
            Some(input) = input_rx.recv() => {
                // Keep vertical cursor motion and mouse placement aligned with the
                // responsive composer width currently on screen.
                let size = terminal.size()?;
                app.set_terminal_width(size.width);
                app.composer_columns = ui::composer_columns(size.width, app.compact_sidebar);
                app.composer_prefix_columns =
                    ui::composer_prefix_columns(&app, app.composer_columns);
                // All input semantics live in `App::handle_key`/`handle_mouse` so the
                // real path is testable; this loop only performs the async work an
                // action asks for.
                let action = match input {
                    Event::Key(key) => app.handle_key(key),
                    Event::Paste(text) => { app.paste(&text); bloom_tui::app::Action::None }
                    Event::Resize(_, _) => bloom_tui::app::Action::None,
                    Event::Mouse(mouse) => app.handle_mouse(mouse.kind, mouse.column, mouse.row, &hits),
                    _ => bloom_tui::app::Action::None,
                };
                match action {
                    bloom_tui::app::Action::Quit => break,
                    bloom_tui::app::Action::SendMessage { attempt, conversation, body } => {
                        let target = Some(conversation.clone());
                        app.begin_send(attempt, target.clone());
                        let client = client.clone();
                        let send_tx = send_tx.clone();
                        tokio::spawn(async move {
                            let started = std::time::Instant::now();
                            diagnostics::record(&format!("request begin attempt={attempt} stage=send"));
                            let result = client.send(conversation, OutgoingBody {
                                parts: vec![MessagePart::Text(body)],
                            }).await;
                            diagnostics::request_result(Stage::Send, attempt, started.elapsed(), result.as_ref().err());
                            let result = result.map_err(SendFailure::from);
                            let _ = send_tx.send((attempt, target, result));
                        });
                    }
                    bloom_tui::app::Action::StartConversation { attempt, address, body } => {
                        let target = None;
                        app.begin_send(attempt, target.clone());
                        let client = client.clone();
                        let send_tx = send_tx.clone();
                        tokio::spawn(async move {
                            let started = std::time::Instant::now();
                            diagnostics::record(&format!("request begin attempt={attempt} stage=start_conversation"));
                            let result = client.start_conversation(address, OutgoingBody {
                                parts: vec![MessagePart::Text(body)],
                            }).await;
                            diagnostics::request_result(Stage::StartConversation, attempt, started.elapsed(), result.as_ref().err());
                            let result = result.map_err(SendFailure::from);
                            let _ = send_tx.send((attempt, target, result));
                        });
                    }
                    bloom_tui::app::Action::Search { request, query, participant } => {
                        let client = client.clone();
                        let tx = search_tx.clone();
                        tokio::spawn(async move {
                            let result = client.search_messages_with_participant(&query, SEARCH_RESULT_LIMIT, participant).await;
                            let _ = tx.send((request, result));
                        });
                    }
                    bloom_tui::app::Action::SyncConversations => {
                        app.sync_in_flight = true;
                        app.notice = Some("Syncing conversation-list metadata...".into());
                        app.error = None;
                        redraw.request();
                        let client = client.clone();
                        let tx = sync_tx.clone();
                        tokio::spawn(async move {
                            let result = client
                                .sync_conversations()
                                .await
                                .map_err(|error| sync_failure_message(&error));
                            let _ = tx.send(result);
                        });
                    }
                    bloom_tui::app::Action::None => {}
                }
                // A selection change is a request for a fresh read, not an event.
                if app.selected != last_visible {
                    last_visible = app.selected.clone();
                    app.thread_loading = last_visible.is_some();
                    let _ = visible_tx.send(last_visible.clone());
                }
                redraw.request();
                if app.is_animating() { next_animation = tokio::time::Instant::now() + Duration::from_millis(33); }
            }
            _ = async {
                if app.is_animating() {
                    tokio::time::sleep_until(next_animation).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                app.advance();
                app.set_syncing(
                    last_sync_activity.is_some_and(|at| at.elapsed() < SYNC_IDLE),
                );
                redraw.request();
                next_animation = tokio::time::Instant::now() + Duration::from_millis(33);
            }
        }
        // Async acknowledgments, live events and metadata can also select a thread.
        if app.selected != last_visible {
            last_visible = app.selected.clone();
            app.thread_loading = last_visible.is_some();
            let _ = visible_tx.send(last_visible.clone());
        }
        if let Some((generation, conversation, target)) = app.begin_search_context_request() {
            let client = client.clone();
            let tx = context_tx.clone();
            tokio::spawn(async move {
                let result = client
                    .message_context(
                        conversation.clone(),
                        target.clone(),
                        bloom_ipc::MAX_MESSAGE_CONTEXT_LIMIT,
                    )
                    .await;
                let _ = tx.send((generation, conversation, target, result));
            });
            redraw.request();
        }
        if app.older_requested {
            // Keep the request pending while another page is in flight, so a
            // gesture is not silently dropped by the single-request rule.
            if let Some((conversation, cursor)) = app.begin_load_older() {
                app.older_requested = false;
                let client = client.clone();
                let tx = older_tx.clone();
                tokio::spawn(async move {
                    let result = client
                        .history_before(conversation.clone(), OLDER_PAGE_LIMIT, cursor)
                        .await;
                    let _ = tx.send((conversation, result));
                });
                redraw.request();
            } else if !app.can_load_older() {
                // Nothing is loading and no page can be fetched, so the request
                // is fully satisfied; clearing it stops a later input from
                // firing a page the user did not ask for.
                app.older_requested = false;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod accessibility_tests {
    use super::*;

    #[test]
    fn parses_osc11_rgb_and_hex_replies() {
        assert_eq!(
            parse_osc11_reply(Some(b"\x1b]11;rgb:ffff/ffff/ffff\x07")),
            Some((255, 255, 255))
        );
        assert_eq!(
            parse_osc11_reply(Some(b"\x1b]11;#102030\x07")),
            Some((16, 32, 48))
        );
        assert_eq!(parse_osc11_reply(Some(b"\x1b]11;garbage\x07")), None);
        assert_eq!(parse_osc11_reply(None), None);
    }

    #[test]
    fn initial_theme_uses_background_luminance() {
        assert_eq!(theme_for_rgb((250, 250, 250)), bloom_tui::app::Theme::Light);
        assert_eq!(theme_for_rgb((10, 10, 10)), bloom_tui::app::Theme::Dark);
    }
}
