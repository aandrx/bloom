//! Bloom TUI: a Bloom-native client for the daemon over its Unix-socket IPC.

use anyhow::Result;
use bloom_domain::{MessageId, MessagePart, OutgoingBody};
use bloom_ipc::{
    client::{BloomClient, ClientError},
    EventKind, ProtocolError, ReadinessSnapshot,
};
use bloom_tui::app::{App, SendFailure, Theme, OLDER_PAGE_LIMIT, SEARCH_RESULT_LIMIT};
use bloom_tui::config::Config;
use bloom_tui::daemon::{self, ActivityToken, ConnectionState, SnapshotNotice, SnapshotRequest};
use bloom_tui::diagnostics::{self, Stage};
use bloom_tui::drafts::{DraftCoordinator, DraftOperation, DraftResult};
use bloom_tui::startup::BootstrapTicket;
use bloom_tui::ui;
use bloom_tui::RedrawScheduler;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

const FIRST_RETRY: Duration = Duration::from_millis(250);
const MAX_RETRY: Duration = Duration::from_secs(8);
/// How long a quiet inbound stream must last before catch-up is complete.
const SYNC_IDLE: Duration = Duration::from_millis(2000);
const OSC_TIMEOUT: Duration = Duration::from_millis(200);
/// How often durable draft work is considered, independent of input events.
const DRAFT_TICK: Duration = Duration::from_millis(100);
/// A quit may wait at most this long for ready draft writes to finish.
const DRAFT_QUIT_BUDGET: Duration = Duration::from_secs(1);

async fn execute_draft(client: &BloomClient, operation: DraftOperation) -> DraftResult {
    match operation {
        DraftOperation::Load { request, key } => {
            let result = client.draft_load(key.clone()).await;
            DraftResult::Load {
                request,
                key,
                result,
            }
        }
        DraftOperation::Save {
            request,
            key,
            revision,
            draft,
        } => {
            let result = client.draft_save(key.clone(), draft, revision).await;
            DraftResult::Save {
                request,
                key,
                revision,
                result,
            }
        }
        DraftOperation::Delete {
            request,
            key,
            revision,
        } => {
            let result = client.draft_delete(key.clone(), revision).await;
            DraftResult::Delete {
                request,
                key,
                revision,
                result,
            }
        }
    }
}

fn spawn_draft(
    client: BloomClient,
    results: mpsc::UnboundedSender<DraftResult>,
    operation: DraftOperation,
) {
    tokio::spawn(async move {
        let _ = results.send(execute_draft(&client, operation).await);
    });
}

fn observe_startup_draft(app: &mut App, coordinator: &DraftCoordinator) {
    if app.startup.opened() || !app.startup.snapshot_ready() {
        return;
    }
    let owner = app.current_persistable_owner();
    // A completion for an earlier owner cannot satisfy the current checklist.
    app.startup.observe_draft_owner(owner.as_ref());
    match owner {
        None => app.observe_startup_draft_ready(),
        Some(key) if coordinator.is_loaded(&key) => app.observe_startup_draft_ready(),
        Some(_) if coordinator.error().is_some() => {
            app.fail_startup_local(bloom_tui::startup::StartupFailure::Client);
        }
        Some(_) => {}
    }
}

/// Launch the one ticketed post-readiness startup read, if startup scheduled one.
/// The ticket is the only thing that lets its reply complete the local check.
fn launch_bootstrap_read(
    app: &mut App,
    client: &BloomClient,
    snapshots: &mpsc::UnboundedSender<SnapshotNotice>,
    failures: &mpsc::UnboundedSender<BootstrapTicket>,
) -> bool {
    let Some(request) = app.begin_bootstrap_snapshot_request() else {
        return false;
    };
    let ticket = request
        .bootstrap
        .expect("a bootstrap request always carries its ticket");
    let client = client.clone();
    let snapshots = snapshots.clone();
    let failures = failures.clone();
    tokio::spawn(async move {
        match daemon::snapshot(&client, request).await {
            Ok(snapshot) => {
                let _ = snapshots.send(SnapshotNotice::Completed(snapshot));
            }
            Err(_) => {
                let _ = failures.send(ticket);
            }
        }
    });
    true
}

fn observe_drafts(app: &App, coordinator: &mut DraftCoordinator) {
    coordinator.observe(
        app.current_persistable_owner(),
        app.desired_drafts(),
        Instant::now(),
    );
}

fn surface_draft_error(app: &mut App, coordinator: &mut DraftCoordinator) -> bool {
    match coordinator.take_error() {
        Some(error) => {
            app.error = Some(error);
            true
        }
        None => false,
    }
}

/// Drain ready draft work for at most one second. The debounce is forced so a
/// just-typed draft is not lost, while any in-flight response is awaited within
/// the same budget. No automatic send or retry happens here.
async fn drain_drafts(
    client: &BloomClient,
    app: &mut App,
    coordinator: &mut DraftCoordinator,
    in_flight: &mut bool,
    results: &mut mpsc::UnboundedReceiver<DraftResult>,
) {
    let deadline = Instant::now() + DRAFT_QUIT_BUDGET;
    loop {
        while let Ok(result) = results.try_recv() {
            *in_flight = false;
            if let Some(candidate) = coordinator.apply_result(result) {
                app.apply_loaded_draft(candidate.key, candidate.draft);
            }
        }
        observe_drafts(app, coordinator);
        let _ = surface_draft_error(app, coordinator);
        let forced_now = Instant::now() + bloom_tui::drafts::DEBOUNCE;
        if let Some(operation) = coordinator.take_ready(forced_now) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(result) =
                tokio::time::timeout(remaining, execute_draft(client, operation)).await
            else {
                break;
            };
            if let Some(candidate) = coordinator.apply_result(result) {
                app.apply_loaded_draft(candidate.key, candidate.draft);
            }
            if Instant::now() >= deadline {
                break;
            }
            continue;
        }
        if Instant::now() >= deadline {
            break;
        }
        if *in_flight {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, results.recv()).await {
                Ok(Some(result)) => {
                    *in_flight = false;
                    if let Some(candidate) = coordinator.apply_result(result) {
                        app.apply_loaded_draft(candidate.key, candidate.draft);
                    }
                    continue;
                }
                _ => break,
            }
        }
        break;
    }
}

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

/// Tracks a failed settings save so the loop neither retries an unchanged
/// config forever nor hides the error until an unrelated redraw.
#[derive(Default)]
struct SettingsSaveState {
    failed: Option<(Config, String)>,
}

impl SettingsSaveState {
    /// A config is attempted unless the same value already failed.
    fn should_attempt(&self, desired: &Config) -> bool {
        self.failed
            .as_ref()
            .is_none_or(|(failed, _)| failed != desired)
    }

    /// Remembers the value that failed and its exact message so the same value
    /// is not attempted again until the desired config changes.
    fn record_failure(&mut self, desired: &Config, message: String) {
        self.failed = Some((desired.clone(), message));
    }

    /// Reports whether the guard's message is now stale because the desired
    /// config matches the saved one. A pending change keeps both the guard and
    /// its error, so a failed change is not cleared and retried on the next
    /// iteration. Returns false unless `error` is exactly the message this guard
    /// set, so an unrelated newer error is never cleared.
    fn settled_message(
        &mut self,
        desired: &Config,
        saved: &Config,
        error: &Option<String>,
    ) -> bool {
        if desired != saved {
            return false;
        }
        self.take_own_message(error)
    }

    /// Forgets the guard after a successful save and reports whether the visible
    /// error is the one this guard set.
    fn take_own_message(&mut self, error: &Option<String>) -> bool {
        let message = self.failed.take().map(|(_, message)| message);
        message.is_some_and(|message| error.as_deref() == Some(message.as_str()))
    }
}

/// The same resolution the daemon uses, so an override (including one from .env)
/// cannot put the UI on a different socket than the daemon.
fn socket_path() -> std::path::PathBuf {
    bloom_ipc::socket_path_from_env()
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
        startup: bloom_tui::startup::StartupState::loading(),
        ..Default::default()
    };
    app.apply_read_positions(&read_positions);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (snapshot_fail_tx, mut snapshot_fail_rx) = mpsc::unbounded_channel::<ActivityToken>();
    let (bootstrap_fail_tx, mut bootstrap_fail_rx) = mpsc::unbounded_channel::<BootstrapTicket>();
    // A retry's readiness fetch is the only thing that can schedule its bootstrap
    // read, so its result must reach the loop even when it fails.
    let (retry_readiness_tx, mut retry_readiness_rx) =
        mpsc::unbounded_channel::<Result<ReadinessSnapshot, ClientError>>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<(
        u64,
        Option<bloom_domain::ConversationId>,
        Result<MessageId, SendFailure>,
    )>();
    let (draft_tx, mut draft_rx) = mpsc::unbounded_channel::<DraftResult>();
    let (conversation_tx, mut conversation_rx) =
        mpsc::unbounded_channel::<Vec<bloom_domain::Conversation>>();
    let (sync_tx, mut sync_rx) = mpsc::unbounded_channel();
    let (older_tx, mut older_rx) = mpsc::unbounded_channel::<(
        bloom_domain::ConversationId,
        ActivityToken,
        Result<Vec<bloom_domain::Message>, ClientError>,
    )>();
    let (search_tx, mut search_rx) =
        mpsc::unbounded_channel::<(u64, Result<Vec<bloom_ipc::MessageSearchHit>, ClientError>)>();
    let (context_tx, mut context_rx) = mpsc::unbounded_channel::<(
        u64,
        bloom_domain::ConversationId,
        MessageId,
        ActivityToken,
        Result<Vec<bloom_domain::Message>, ClientError>,
    )>();
    let (attachment_tx, mut attachment_rx) =
        mpsc::unbounded_channel::<(String, Result<(String, u64), ClientError>)>();
    let (attachment_send_tx, mut attachment_send_rx) =
        mpsc::unbounded_channel::<Result<MessageId, SendFailure>>();
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());

    {
        let client = client.clone();
        let snapshot_tx = snapshot_tx.clone();
        let event_tx = event_tx.clone();
        let status_tx = status_tx.clone();
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
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<(Instant, Event)>();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            let read_at = Instant::now();
            match event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if input_tx.send((read_at, Event::Key(key))).is_err() {
                        return;
                    }
                }
                Ok(event @ (Event::Mouse(_) | Event::Paste(_) | Event::Resize(_, _))) => {
                    if input_tx.send((read_at, event)).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });

    let mut last_visible = app.selected.clone();
    let mut resnapshot_on_connect = false;
    let mut redraw = RedrawScheduler::default();
    let mut settings_save = SettingsSaveState::default();
    let mut hits = ui::HitMap::default();
    let mut render_cache = ui::RenderCache::default();
    let mut inbound_redraws = VecDeque::<(u64, std::time::Instant)>::new();
    // The daemon has no explicit catch-up completion frame, so treat a burst of
    // inbound events as syncing until the stream has been quiet for a moment.
    let mut last_sync_activity: Option<std::time::Instant> = None;
    let mut drafts = DraftCoordinator::new();
    let mut draft_in_flight = false;
    let mut draft_tick_due = false;
    redraw.request();
    let mut next_animation = tokio::time::Instant::now();
    let mut next_draft_tick = tokio::time::Instant::now();
    let mut startup_input_cutoff: Option<Instant> = None;
    let mut startup_enter_armed = true;
    loop {
        // Owner and modal changes can arrive through any branch, so capture them
        // before painting rather than only after the next key press.
        observe_drafts(&app, &mut drafts);
        observe_startup_draft(&mut app, &drafts);
        if surface_draft_error(&mut app, &mut drafts) {
            // A failed background save is reported in the normal UI, not as a
            // terminal startup failure.
            redraw.request();
        }
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
            if hits.sidebar.width > 0 {
                app.record_sidebar_view(
                    hits.sidebar_offset,
                    hits.sidebar_visible_entries,
                    hits.sidebar_max_offset,
                );
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
        let settings_pending = updated_config != config;
        if settings_pending && settings_save.should_attempt(&updated_config) {
            match updated_config.save(&config_path) {
                Ok(()) => {
                    config = updated_config;
                    if settings_save.take_own_message(&app.error) {
                        app.error = None;
                        redraw.request();
                    }
                }
                Err(error) => {
                    let message = format!("Could not save local settings: {error}");
                    app.error = Some(message.clone());
                    settings_save.record_failure(&updated_config, message);
                    // The error was set after this frame was drawn, so paint it
                    // without waiting for an unrelated event.
                    redraw.request();
                }
            }
        } else if settings_save.settled_message(&updated_config, &config, &app.error) {
            // Nothing is pending, so an earlier failure no longer describes
            // outstanding work. A still-pending failed change keeps its error.
            app.error = None;
            redraw.request();
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
            _ = tokio::time::sleep_until(next_draft_tick) => {
                next_draft_tick = tokio::time::Instant::now() + DRAFT_TICK;
                draft_tick_due = true;
            }
            Some(result) = draft_rx.recv() => {
                draft_in_flight = false;
                if let Some(candidate) = drafts.apply_result(result) {
                    if app.apply_loaded_draft(candidate.key, candidate.draft) {
                        redraw.request();
                    }
                }
                observe_startup_draft(&mut app, &drafts);
                if surface_draft_error(&mut app, &mut drafts) {
                    // A failed save must not make the startup checklist look
                    // terminal; draft loads still drive the local check above.
                    redraw.request();
                }
            }
            Some(notice) = snapshot_rx.recv() => {
                match notice {
                    SnapshotNotice::Started(request) => {
                        // The session consumed this watch request, so only now does
                        // it own local activity. A coalesced-away request never
                        // reaches this branch and cannot leave a spinner behind.
                        if app.start_snapshot_activity(&request) {
                            redraw.request();
                        }
                    }
                    SnapshotNotice::Completed(snapshot) => {
                        app.apply_snapshot(snapshot);
                        launch_bootstrap_read(
                            &mut app,
                            &client,
                            &snapshot_tx,
                            &bootstrap_fail_tx,
                        );
                        app.advance_search_jump();
                        redraw.request();
                        if app.selected != last_visible {
                            last_visible = app.selected.clone();
                            let request = app.prepare_snapshot_request();
                            let _ = visible_tx.send(request);
                        }
                    }
                }
            }
            Some(token) = snapshot_fail_rx.recv() => {
                app.fail_snapshot_activity(token);
                redraw.request();
            }
            Some(ticket) = bootstrap_fail_rx.recv() => {
                app.fail_startup_bootstrap(ticket);
                redraw.request();
            }
            Some(result) = retry_readiness_rx.recv() => {
                // A failed fetch becomes a bounded startup failure and drops any
                // ticket, so the overlay offers Retry instead of stalling.
                if app.startup.observe_retry_readiness(result) {
                    // The retry's readiness fetch is what schedules its bootstrap
                    // read, so launch it here: the event and snapshot paths may
                    // already have run before this result arrived, and with no
                    // selected conversation the poller would never read.
                    launch_bootstrap_read(
                        &mut app,
                        &client,
                        &snapshot_tx,
                        &bootstrap_fail_tx,
                    );
                }
                redraw.request();
            }
            Some(conversations) = conversation_rx.recv() => {
                app.set_conversations(conversations);
                redraw.request();
            },
            Some(state) = status_rx.recv() => {
                app.connection = state.label().to_owned();
                app.observe_startup_connection(state);
                if state == ConnectionState::Disconnected {
                    resnapshot_on_connect = true;
                } else if state == ConnectionState::Connected && resnapshot_on_connect {
                    // The reconnect's initial snapshot carries the old epoch's
                    // token, so ask for one fresh local read under the new epoch.
                    resnapshot_on_connect = false;
                    let request = app.prepare_snapshot_request();
                    let _ = visible_tx.send(request);
                }
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
                launch_bootstrap_read(
                    &mut app,
                    &client,
                    &snapshot_tx,
                    &bootstrap_fail_tx,
                );
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
                    let failures = snapshot_fail_tx.clone();
                    let request = app.begin_snapshot_request();
                    let token = request.token;
                    tokio::spawn(async move {
                        match daemon::snapshot(&client, request).await {
                            Ok(snapshot) => {
                                let _ = snapshots.send(SnapshotNotice::Completed(snapshot));
                            }
                            Err(_) => {
                                if let Some(token) = token {
                                    let _ = failures.send(token);
                                }
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
            Some((conversation, token, result)) = older_rx.recv() => {
                match result {
                    Ok(page) => {
                        app.finish_load_older(
                            conversation.clone(),
                            token,
                            page,
                            OLDER_PAGE_LIMIT,
                        );
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
                            token,
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
            Some(result) = attachment_send_rx.recv() => {
                app.finish_attachment_send(result);
                redraw.request();
            },
            Some((token, result)) = attachment_rx.recv() => {
                let result = result.map_err(|error| match error {
                    ClientError::Server(ProtocolError::InvalidRequest)
                    | ClientError::Server(ProtocolError::Unsupported) => {
                        "Attachment is no longer available. Ask the sender to resend it.".to_owned()
                    }
                    _ => "Attachment could not be saved.".to_owned(),
                });
                if let Some(path) = app.finish_attachment_fetch(&token, result) {
                    match bloom_tui::open::open_external(&path) {
                        Ok(()) => app.finish_attachment_open(&path),
                        Err(error) => app.fail_attachment_open(error.message()),
                    }
                }
                redraw.request();
            },
            Some((generation, conversation, target, token, result)) = context_rx.recv() => {
                match result {
                    Ok(messages) => {
                        app.finish_message_context(
                            generation,
                            conversation,
                            target,
                            token,
                            messages,
                        );
                    }
                    Err(_) => {
                        app.fail_message_context(generation, conversation, target, token);
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
                let failures = snapshot_fail_tx.clone();
                let request = app.begin_snapshot_request();
                let token = request.token;
                tokio::spawn(async move {
                    match daemon::snapshot(&client, request).await {
                        Ok(snapshot) => {
                            let _ = snapshots.send(SnapshotNotice::Completed(snapshot));
                        }
                        Err(_) => {
                            if let Some(token) = token {
                                let _ = failures.send(token);
                            }
                        }
                    }
                });
                redraw.request();
            },
            Some((read_at, input)) = input_rx.recv() => {
                if let Some(cutoff) = startup_input_cutoff {
                    if read_at <= cutoff {
                        redraw.request();
                        continue;
                    }
                    startup_input_cutoff = None;
                }
                if !startup_enter_armed && !app.startup.overlay_visible() {
                    match &input {
                        Event::Key(key)
                            if key.code == KeyCode::Enter
                                || key.kind == KeyEventKind::Repeat =>
                        {
                            redraw.request();
                            continue;
                        }
                        Event::Key(_) | Event::Paste(_) | Event::Mouse(_) => {
                            // Require one deliberate non-Enter event after Open
                            // Bloom before Enter can submit a restored draft.
                            startup_enter_armed = true;
                        }
                        _ => {}
                    }
                }
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
                    bloom_tui::app::Action::Quit => {
                        // Give an in-flight send a bounded chance to settle before
                        // flushing drafts, so restart cannot resend a delivered body.
                        if app.pending_send_attempt.is_some() {
                            let settle = tokio::time::timeout(
                                Duration::from_secs(1),
                                send_rx.recv(),
                            )
                            .await;
                            if let Ok(Some((attempt, conversation, result))) = settle {
                                app.finish_send_receipt(attempt, conversation, result);
                            } else if app.pending_send_attempt.is_some() {
                                app.mark_pending_send_uncertain();
                            }
                        }
                        drain_drafts(
                            &client,
                            &mut app,
                            &mut drafts,
                            &mut draft_in_flight,
                            &mut draft_rx,
                        )
                        .await;
                        break;
                    }
                    bloom_tui::app::Action::SendMessage { .. } if !app.outbound_allowed() => {
                        app.refuse_outbound();
                    }
                    bloom_tui::app::Action::StartConversation { .. } if !app.outbound_allowed() => {
                        app.refuse_outbound();
                    }
                    bloom_tui::app::Action::SendAttachment { .. } if !app.outbound_allowed() => {
                        app.refuse_outbound();
                    }
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
                    bloom_tui::app::Action::SendAttachment { conversation, path } => {
                        let client = client.clone();
                        let tx = attachment_send_tx.clone();
                        tokio::spawn(async move {
                            let started = std::time::Instant::now();
                            diagnostics::record("request begin stage=send_attachment");
                            let result = client.send_attachment(conversation, path).await;
                            diagnostics::attachment_send_result(
                                started.elapsed(),
                                result.as_ref().err(),
                            );
                            let _ = tx.send(result.map_err(SendFailure::from));
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
                    bloom_tui::app::Action::FetchAttachment { token } => {
                        let client = client.clone();
                        let tx = attachment_tx.clone();
                        tokio::spawn(async move {
                            let result = client.fetch_attachment(token.clone()).await;
                            let _ = tx.send((token, result));
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
                    bloom_tui::app::Action::OpenBloom => {
                        if app.open_bloom() {
                            // Events already read from the terminal before this
                            // confirmation must not become composer commands. Stamp
                            // a read-time cutoff instead of draining the channel:
                            // a drain would also eat keys the user pressed immediately
                            // after the screen changed.
                            startup_input_cutoff = Some(Instant::now());
                            startup_enter_armed = false;
                        }
                    }
                    bloom_tui::app::Action::RetryStartup => {
                        app.begin_startup_retry();
                        let client = client.clone();
                        let snapshots = snapshot_tx.clone();
                        let failures = snapshot_fail_tx.clone();
                        let readiness = retry_readiness_tx.clone();
                        let sync_tx = sync_tx.clone();
                        let request = app.begin_snapshot_request();
                        let token = request.token;
                        tokio::spawn(async move {
                            let sync_result = client
                                .sync_conversations()
                                .await
                                .map_err(|error| sync_failure_message(&error));
                            let _ = sync_tx.send(sync_result);
                            // Forward the fetch result either way: a silent error
                            // would leave no readiness identity and therefore no
                            // bootstrap ticket, wedging the checklist.
                            let _ = readiness.send(client.readiness().await);
                            match daemon::snapshot(&client, request).await {
                                Ok(snapshot) => {
                                    let _ = snapshots.send(SnapshotNotice::Completed(snapshot));
                                }
                                Err(_) => {
                                    if let Some(token) = token {
                                        let _ = failures.send(token);
                                    }
                                }
                            }
                        });
                    }
                    bloom_tui::app::Action::None => {}
                }
                // A selection change is a request for a fresh read, not an event.
                if app.selected != last_visible {
                    last_visible = app.selected.clone();
                    let request = app.prepare_snapshot_request();
                    let _ = visible_tx.send(request);
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
        if draft_tick_due {
            draft_tick_due = false;
            observe_drafts(&app, &mut drafts);
            if let Some(operation) = drafts.take_ready(Instant::now()) {
                draft_in_flight = true;
                spawn_draft(client.clone(), draft_tx.clone(), operation);
            }
            if surface_draft_error(&mut app, &mut drafts) {
                redraw.request();
            }
        }
        // Async acknowledgments, live events and metadata can also select a thread.
        if app.selected != last_visible {
            last_visible = app.selected.clone();
            let request = app.prepare_snapshot_request();
            let _ = visible_tx.send(request);
        }
        if let Some((generation, conversation, target, token)) =
            app.begin_connected_search_context_request()
        {
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
                let _ = tx.send((generation, conversation, target, token, result));
            });
            redraw.request();
        }
        if app.older_requested {
            // Keep the request pending while another page is in flight, so a
            // gesture is not silently dropped by the single-request rule.
            if let Some((conversation, cursor, token)) = app.begin_load_older() {
                app.older_requested = false;
                let client = client.clone();
                let tx = older_tx.clone();
                tokio::spawn(async move {
                    let result = client
                        .history_before(conversation.clone(), OLDER_PAGE_LIMIT, cursor)
                        .await;
                    let _ = tx.send((conversation, token, result));
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

#[cfg(test)]
mod settings_save_tests {
    use super::*;

    fn desired(theme: Option<&str>) -> Config {
        Config {
            version: 1,
            theme: theme.map(str::to_owned),
            ..Config::default()
        }
    }

    #[test]
    fn repeated_identical_config_attempts_once_then_is_suppressed() {
        let mut state = SettingsSaveState::default();
        let config = desired(Some("dark"));
        assert!(state.should_attempt(&config));
        state.record_failure(&config, "Could not save local settings: denied".into());
        assert!(!state.should_attempt(&config));
    }

    #[test]
    fn changed_config_after_failure_is_attempted_again() {
        let mut state = SettingsSaveState::default();
        let failed = desired(Some("dark"));
        state.record_failure(&failed, "Could not save local settings: denied".into());
        assert!(state.should_attempt(&desired(Some("light"))));
    }

    #[test]
    fn success_adopts_config_and_clears_its_own_message() {
        let mut state = SettingsSaveState::default();
        let config = desired(Some("dark"));
        let message = "Could not save local settings: denied".to_owned();
        state.record_failure(&config, message.clone());
        let mut error = Some(message);
        if state.take_own_message(&error) {
            error = None;
        }
        assert_eq!(error, None);
        assert!(state.should_attempt(&desired(Some("light"))));
    }

    #[test]
    fn success_leaves_a_different_error_untouched() {
        let mut state = SettingsSaveState::default();
        let config = desired(Some("dark"));
        state.record_failure(&config, "Could not save local settings: denied".into());
        let error = Some("Conversation-list sync failed.".to_owned());
        let saved = desired(None);
        assert!(!state.settled_message(&saved, &saved, &error));
        assert_eq!(error.as_deref(), Some("Conversation-list sync failed."));
    }

    #[test]
    fn reverting_to_the_saved_config_clears_the_stale_message() {
        let mut state = SettingsSaveState::default();
        let failed = desired(Some("dark"));
        let message = "Could not save local settings: denied".to_owned();
        state.record_failure(&failed, message.clone());
        // Reverting makes the desired config equal the saved one, so nothing is
        // pending and the guard's stale message is cleared.
        let saved = desired(None);
        let mut error = Some(message);
        if state.settled_message(&saved, &saved, &error) {
            error = None;
        }
        assert_eq!(error, None);
        assert!(state.failed.is_none());
    }

    #[test]
    fn pending_failed_change_keeps_its_error_and_stays_suppressed() {
        let mut state = SettingsSaveState::default();
        let failed = desired(Some("dark"));
        let message = "Could not save local settings: denied".to_owned();
        state.record_failure(&failed, message.clone());
        let error = Some(message);
        // The change is still pending, so the message is not stale.
        assert!(!state.settled_message(&failed, &desired(None), &error));
        assert_eq!(
            error.as_deref(),
            Some("Could not save local settings: denied")
        );
        // The guard must survive, otherwise the next iteration retries the write.
        assert!(!state.should_attempt(&failed));
    }

    #[test]
    fn failure_is_recorded_so_the_same_value_is_not_retried() {
        let mut state = SettingsSaveState::default();
        let config = desired(Some("dark"));
        state.record_failure(&config, "Could not save local settings: denied".into());
        assert_eq!(
            state.failed.as_ref().map(|(failed, _)| failed),
            Some(&config)
        );
        assert!(!state.should_attempt(&config));
    }
}
