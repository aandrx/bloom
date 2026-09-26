mod app;
mod auth;
mod ui;

use anyhow::Result;
use app::App;
use bloom_core::Bloom;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
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
    run(terminal_guard.output(), _core).await
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

async fn with_shutdown<T, F, S>(work: F, shutdown: S) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
    S: std::future::Future<Output = Result<()>>,
{
    let result = work.await;
    let shutdown = shutdown.await;
    match (result, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn poll_terminal_key() -> Result<Option<KeyCode>> {
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    if event::poll(std::time::Duration::from_millis(0))? {
        if let Event::Key(key) = event::read()? {
            return Ok(Some(key.code));
        }
    }
    Ok(None)
}

async fn select_key_or_send<K, J, S>(key: K, join: J, send: S) -> KeyOrJoinOrSend
where
    K: std::future::Future<Output = Result<Option<KeyCode>>>,
    J: std::future::Future<Output = Option<(String, anyhow::Result<()>)>>,
    S: std::future::Future<Output = Option<(String, String, u64, anyhow::Result<String>)>>,
{
    tokio::select! {
        key = key => KeyOrJoinOrSend::Key(key),
        result = join => KeyOrJoinOrSend::Join(result),
        result = send => KeyOrJoinOrSend::Send(result),
    }
}

enum KeyOrJoinOrSend {
    Key(Result<Option<KeyCode>>),
    Join(Option<(String, anyhow::Result<()>)>),
    Send(Option<(String, String, u64, anyhow::Result<String>)>),
}

fn spawn_join<F>(
    id: String,
    action: F,
    tx: tokio::sync::mpsc::UnboundedSender<(String, anyhow::Result<()>)>,
) where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = action.await;
        let _ = tx.send((id, result));
    });
}

fn spawn_send<F>(
    room: String,
    body: String,
    generation: u64,
    action: F,
    tx: tokio::sync::mpsc::UnboundedSender<(String, String, u64, anyhow::Result<String>)>,
) where
    F: std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = action.await;
        let _ = tx.send((room, body, generation, result));
    });
}

#[derive(Default)]
struct RoomEpoch(u64);

impl RoomEpoch {
    fn advance(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }

    fn current(&self) -> u64 {
        self.0
    }

    fn apply<T>(&self, generation: u64, value: T) -> Option<T> {
        (generation == self.0).then_some(value)
    }
}

async fn run(out: &mut io::Stdout, core: Bloom) -> Result<()> {
    let core = std::sync::Arc::new(core);
    let (mut events, sync) = core.start_sync(128).await?;
    let mut app = App::default();
    let (room_tx, mut room_rx) = tokio::sync::mpsc::unbounded_channel();
    let (history_tx, mut history_rx) = tokio::sync::mpsc::unbounded_channel();
    let (join_tx, mut join_rx) = tokio::sync::mpsc::unbounded_channel();
    let (send_tx, mut send_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut room_epoch = RoomEpoch::default();
    let initial_generation = room_epoch.current();
    let initial_core = core.clone();
    let initial_room_tx = room_tx.clone();
    tokio::spawn(async move {
        let _ = initial_room_tx.send((initial_generation, initial_core.rooms().await));
    });
    let result = async {
        let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
        loop {
            terminal.draw(|frame| ui::render(frame, &app))?;
            tokio::select! {
                event = events.recv() => if let Ok(event) = event {
                    if let bloom_core::CoreEvent::RoomsChanged(_) = &event {
                        room_epoch.advance();
                    }
                    for id in app.handle(event) { spawn_history(core.clone(), id, history_tx.clone()); }
                    for id in app.take_history_requests() { spawn_history(core.clone(), id, history_tx.clone()); }
                },
                Some((generation, rooms)) = room_rx.recv() => {
                    if let Some(rooms) = room_epoch.apply(generation, rooms) {
                        for id in app.set_rooms(rooms) {
                            spawn_history(core.clone(), id, history_tx.clone());
                        }
                    }
                },
                Some((id, result)) = history_rx.recv() => app.history_result(id, result),
                selected = select_key_or_send(poll_terminal_key(), join_rx.recv(), send_rx.recv()) => match selected {
                KeyOrJoinOrSend::Send(Some((id, body, generation, result))) => app.send_result(id, body, generation, result),
                KeyOrJoinOrSend::Send(None) => {},
                KeyOrJoinOrSend::Join(Some((id, result))) => { app.join_result(id, result); let generation = room_epoch.advance(); let core = core.clone(); let tx = room_tx.clone(); tokio::spawn(async move { let _ = tx.send((generation, core.rooms().await)); }); },
                KeyOrJoinOrSend::Join(None) => {},
                KeyOrJoinOrSend::Key(Ok(key)) => {
                            if app.composer_open {
                                match key {
                                    Some(KeyCode::Esc) => app.cancel_composer(),
                                    Some(KeyCode::Enter) => if let Some((id, body, generation)) = app.begin_send() {
                                        let core = core.clone(); let tx = send_tx.clone();
                                        let submitted_id = id.clone(); let submitted_body = body.clone();
                                        spawn_send(id, body, generation, async move {
                                            match submitted_id.parse() {
                                                Ok(room) => core.send(&room, &submitted_body).await,
                                                Err(error) => Err(anyhow::anyhow!(error)),
                                            }
                                        }, tx);
                                    },
                                    Some(KeyCode::Backspace) => app.backspace(),
                                    Some(KeyCode::Char(c)) => app.edit_char(c),
                                    _ => {}
                                }
                            } else { match key {
                                Some(KeyCode::Char('m')) => app.open_composer(),
                                Some(KeyCode::Down) => app.select_step(1),
                                Some(KeyCode::Up) => app.select_step(-1),
                                Some(KeyCode::Char('j')) => if let Some(id) = app.request_join() {
                                    let core = core.clone(); let tx = join_tx.clone();
                                    spawn_join(id.clone(), async move { let parsed = id.parse(); match parsed { Ok(room) => core.join(&room).await, Err(error) => Err(anyhow::anyhow!(error)) } }, tx);
                                },
                                Some(KeyCode::Char('q')) => break,
                                _ => {}
                            }}
                }
                KeyOrJoinOrSend::Key(Err(error)) => return Err(error),
                },
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    with_shutdown(result, sync.shutdown()).await
}

fn spawn_history(
    core: std::sync::Arc<Bloom>,
    id: String,
    tx: tokio::sync::mpsc::UnboundedSender<(String, anyhow::Result<Vec<bloom_core::Message>>)>,
) {
    tokio::spawn(async move {
        let result = match id.parse() {
            Ok(room) => core.history(&room, 100).await,
            Err(error) => Err(anyhow::anyhow!(error)),
        };
        let _ = tx.send((id, result));
    });
}

#[cfg(test)]
mod tests {
    use super::{
        select_key_or_send, spawn_join, spawn_send, with_shutdown, KeyOrJoinOrSend, RoomEpoch,
        TerminalGuard,
    };
    use anyhow::anyhow;
    use crossterm::event::KeyCode;
    use std::io;

    #[tokio::test]
    async fn shutdown_runs_when_work_fails() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let result = with_shutdown(
            async { Err::<(), _>(anyhow!("terminal failed")) },
            async move {
                let _ = tx.send(());
                Ok(())
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(rx.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn key_is_selected_before_delayed_join() {
        let (join_tx, mut join_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        spawn_join(
            "room".to_string(),
            async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(())
            },
            join_tx,
        );
        started_rx.await.unwrap();
        let (_send_tx, mut send_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, String, u64, anyhow::Result<String>)>();
        let selected = select_key_or_send(
            async { Ok(Some(KeyCode::Down)) },
            join_rx.recv(),
            send_rx.recv(),
        )
        .await;
        assert!(matches!(
            selected,
            KeyOrJoinOrSend::Key(Ok(Some(KeyCode::Down)))
        ));
        assert!(join_rx.try_recv().is_err());
        assert!(send_rx.try_recv().is_err());
        assert!(release_tx.send(()).is_ok());
        let selected =
            select_key_or_send(std::future::pending(), join_rx.recv(), send_rx.recv()).await;
        assert!(matches!(selected, KeyOrJoinOrSend::Join(Some((id, Ok(())))) if id == "room"));
    }

    #[tokio::test]
    async fn key_remains_responsive_while_send_is_pending() {
        let (send_tx, mut send_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        spawn_send(
            "room".into(),
            "text".into(),
            1,
            async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok("$event".into())
            },
            send_tx,
        );
        started_rx.await.unwrap();
        let selected = select_key_or_send(
            async { Ok(Some(KeyCode::Char('x'))) },
            std::future::pending(),
            send_rx.recv(),
        )
        .await;
        assert!(matches!(
            selected,
            KeyOrJoinOrSend::Key(Ok(Some(KeyCode::Char('x'))))
        ));
        assert!(send_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        let selected = select_key_or_send(
            std::future::pending(),
            std::future::pending(),
            send_rx.recv(),
        )
        .await;
        assert!(
            matches!(selected, KeyOrJoinOrSend::Send(Some((room, body, 1, Ok(event)))) if room == "room" && body == "text" && event == "$event")
        );
    }

    #[test]
    fn stale_room_snapshots_are_rejected_after_newer_generation() {
        let mut epoch = RoomEpoch::default();
        let initial = epoch.current();
        let sync = epoch.advance();
        assert!(epoch.apply(initial, vec!["old"]).is_none());
        let refresh = epoch.advance();
        assert!(epoch.apply(sync, vec!["sync-old"]).is_none());
        assert_eq!(epoch.apply(refresh, vec!["new"]), Some(vec!["new"]));
    }

    #[test]
    fn setup_failure_leaves_cleanup_flag_set() {
        let mut cleanup_flag = false;
        let result =
            TerminalGuard::setup_phase(&mut cleanup_flag, || Err(io::Error::other("setup failed")));

        assert!(result.is_err());
        assert!(cleanup_flag);
    }
}
