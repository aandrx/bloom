//! Headless tests for the TUI's daemon session logic, driven by a fake daemon over
//! a real Unix socket. No terminal and no real daemon are involved.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::{
    client::ClientError, read_request, write_event, write_response, ConversationSyncStatus,
    DraftKey, DraftWriteOutcome, DurableDraft, Event, EventKind, ProtocolError, ReadinessOutcome,
    ReadinessPhase, ReadinessSnapshot, RequestKind, Response, ResponseKind,
};
use bloom_tui::{
    app::App,
    daemon::{
        pump_session, pump_session_with_interval, snapshot, ConnectionState, Snapshot,
        SnapshotNotice, SnapshotRequest,
    },
    startup::StartupState,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    net::UnixListener,
    sync::{broadcast, mpsc, watch, Semaphore},
};

fn transport() -> TransportId {
    TransportId::new("openbubbles").unwrap()
}

fn conversation(value: &str, last: i64) -> Conversation {
    Conversation {
        id: ConversationId::new(transport(), value).unwrap(),
        remote_id: format!("iMessage;-;{value}"),
        kind: ConversationKind::Direct,
        title: None,
        participants: vec![Participant {
            handle: Handle::new(HandleKind::Opaque, value).unwrap(),
            display_name: None,
            is_me: false,
        }],
        created_at: 1,
        last_message_at: Some(last),
        aliases: Vec::new(),
        preview: None,
    }
}

fn message(conversation: &Conversation, id: &str, at: i64, text: &str) -> Message {
    Message::new(
        MessageId::new(transport(), id).unwrap(),
        conversation.id.clone(),
        None,
        Handle::new(HandleKind::Opaque, "someone").unwrap(),
        vec![MessagePart::Text(text.to_owned())],
        at,
        None,
        DeliveryState::Delivered,
        false,
        None,
        Vec::new(),
        None,
    )
    .unwrap()
}

type StoredDraft = (DraftKey, u64, Option<DurableDraft>);

struct FakeDaemon {
    path: PathBuf,
    events: broadcast::Sender<Event>,
    conversations: Arc<Mutex<Vec<Conversation>>>,
    history_requests: Arc<Mutex<Vec<String>>>,
    first_thread_updates: Arc<Mutex<Vec<Message>>>,
    history_delay: Arc<Mutex<Duration>>,
    list_delay: Arc<Mutex<Duration>>,
    list_failure: Arc<Mutex<bool>>,
    list_blocked: Arc<AtomicBool>,
    list_gate: Arc<Semaphore>,
    drafts: Arc<Mutex<Vec<StoredDraft>>>,
    draft_failure: Arc<Mutex<bool>>,
    timeline: Arc<Mutex<Vec<String>>>,
    accept: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl FakeDaemon {
    fn start(close_after_first_event: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let path = private.join("bloomd.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let conversations = Arc::new(Mutex::new(vec![
            conversation("first@example.com", 20),
            conversation("second@example.com", 10),
        ]));
        let (events, _) = broadcast::channel::<Event>(16);
        let history_requests = Arc::new(Mutex::new(Vec::new()));
        let first_thread_updates = Arc::new(Mutex::new(Vec::new()));
        let history_delay = Arc::new(Mutex::new(Duration::ZERO));
        let list_delay = Arc::new(Mutex::new(Duration::ZERO));
        let list_failure = Arc::new(Mutex::new(false));
        let list_blocked = Arc::new(AtomicBool::new(false));
        let list_gate = Arc::new(Semaphore::new(0));
        let drafts = Arc::new(Mutex::new(Vec::new()));
        let draft_failure = Arc::new(Mutex::new(false));
        let timeline = Arc::new(Mutex::new(Vec::new()));

        let accept = {
            let conversations = conversations.clone();
            let events = events.clone();
            let history_requests = history_requests.clone();
            let first_thread_updates = first_thread_updates.clone();
            let history_delay = history_delay.clone();
            let list_delay = list_delay.clone();
            let list_failure = list_failure.clone();
            let list_blocked = list_blocked.clone();
            let list_gate = list_gate.clone();
            let drafts = drafts.clone();
            let draft_failure = draft_failure.clone();
            let timeline = timeline.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let events = events.clone();
                    let history_requests = history_requests.clone();
                    let first_thread_updates = first_thread_updates.clone();
                    let history_delay = history_delay.clone();
                    let list_delay = list_delay.clone();
                    let list_failure = list_failure.clone();
                    let list_blocked = list_blocked.clone();
                    let list_gate = list_gate.clone();
                    let drafts = drafts.clone();
                    let draft_failure = draft_failure.clone();
                    let timeline = timeline.clone();
                    let conversations = conversations.clone();
                    tokio::spawn(async move {
                        while let Ok(request) = read_request(&mut stream).await {
                            let request_id = request.request_id;
                            match request.request {
                                RequestKind::ListConversations => {
                                    timeline.lock().unwrap().push("list:start".to_owned());
                                    if list_blocked.load(Ordering::SeqCst) {
                                        let _ = list_gate.acquire().await;
                                    }
                                    if *list_failure.lock().unwrap() {
                                        let _ = write_response(
                                            &mut stream,
                                            &Response {
                                                request_id,
                                                response: ResponseKind::Error {
                                                    error: ProtocolError::Internal,
                                                },
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                    let delay = *list_delay.lock().unwrap();
                                    tokio::time::sleep(delay).await;
                                    timeline.lock().unwrap().push("list:end".to_owned());
                                    let conversations = conversations.lock().unwrap().clone();
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Conversations { conversations },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::History {
                                    conversation_id,
                                    limit,
                                    before: _,
                                } => {
                                    history_requests
                                        .lock()
                                        .unwrap()
                                        .push(conversation_id.value().to_owned());
                                    timeline
                                        .lock()
                                        .unwrap()
                                        .push(format!("history:{}", conversation_id.value()));
                                    let delay = *history_delay.lock().unwrap();
                                    tokio::time::sleep(delay).await;
                                    let conversations = conversations.lock().unwrap().clone();
                                    let target = conversations
                                        .iter()
                                        .find(|c| c.id == conversation_id)
                                        .cloned();
                                    let messages = match target {
                                        Some(conversation) => {
                                            let mut messages = vec![message(
                                                &conversation,
                                                "history-1",
                                                5,
                                                "from history",
                                            )];
                                            if conversations
                                                .first()
                                                .is_some_and(|first| conversation.id == first.id)
                                            {
                                                messages.extend(
                                                    first_thread_updates.lock().unwrap().clone(),
                                                );
                                            }
                                            messages
                                        }
                                        None => Vec::new(),
                                    };
                                    let _ = limit;
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::History { messages },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::Readiness => {
                                    let snapshot = ReadinessSnapshot {
                                        generation: 1,
                                        operation: 1,
                                        revision: 1,
                                        phase: ReadinessPhase::Ready,
                                        outcome: Some(ReadinessOutcome::Complete),
                                        observed_records: conversations.lock().unwrap().len()
                                            as u32,
                                        can_send: true,
                                        engine_ready: true,
                                        bounded_error: None,
                                    };
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Readiness { snapshot },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::SyncConversations => {
                                    let count = conversations.lock().unwrap().len() as u32;
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::ConversationSync {
                                                status: ConversationSyncStatus::Complete,
                                                conversations: count,
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::StartConversation { .. } => unreachable!(),
                                RequestKind::Send { .. } => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Send {
                                                message_id: MessageId::new(transport(), "sent-1")
                                                    .unwrap(),
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::SearchMessages { .. } => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::SearchResults { hits: vec![] },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::MessageContext { .. } => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Error {
                                                error: ProtocolError::Unsupported,
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::DraftSave {
                                    key,
                                    draft,
                                    revision,
                                } => {
                                    let response = if *draft_failure.lock().unwrap() {
                                        ResponseKind::Error {
                                            error: ProtocolError::Internal,
                                        }
                                    } else {
                                        let mut stored = drafts.lock().unwrap();
                                        let existing = stored
                                            .iter()
                                            .position(|(candidate, _, _)| candidate == &key);
                                        if existing.is_some_and(|index| revision <= stored[index].1)
                                        {
                                            ResponseKind::DraftSave {
                                                outcome: DraftWriteOutcome::Stale,
                                            }
                                        } else {
                                            if let Some(index) = existing {
                                                stored[index] = (key, revision, Some(draft));
                                            } else {
                                                stored.push((key, revision, Some(draft)));
                                            }
                                            ResponseKind::DraftSave {
                                                outcome: DraftWriteOutcome::Applied,
                                            }
                                        }
                                    };
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response,
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::DraftLoad { key } => {
                                    let (draft, revision_floor) = {
                                        let stored = drafts.lock().unwrap();
                                        match stored
                                            .iter()
                                            .find(|(candidate, _, _)| candidate == &key)
                                        {
                                            Some((_, revision, draft)) => {
                                                (draft.clone(), *revision)
                                            }
                                            None => (None, 0),
                                        }
                                    };
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::DraftLoad {
                                                draft,
                                                revision_floor,
                                                corrupt_deleted: false,
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::DraftDelete { key, revision } => {
                                    let response = if *draft_failure.lock().unwrap() {
                                        ResponseKind::Error {
                                            error: ProtocolError::Internal,
                                        }
                                    } else {
                                        let mut stored = drafts.lock().unwrap();
                                        let existing = stored
                                            .iter()
                                            .position(|(candidate, _, _)| candidate == &key);
                                        if existing.is_some_and(|index| revision <= stored[index].1)
                                        {
                                            ResponseKind::DraftDelete {
                                                outcome: DraftWriteOutcome::Stale,
                                            }
                                        } else {
                                            if let Some(index) = existing {
                                                stored[index] = (key, revision, None);
                                            } else {
                                                stored.push((key, revision, None));
                                            }
                                            ResponseKind::DraftDelete {
                                                outcome: DraftWriteOutcome::Applied,
                                            }
                                        }
                                    };
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response,
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::SendAttachment { .. } => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Error {
                                                error: ProtocolError::Unsupported,
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::FetchAttachment { .. } => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Error {
                                                error: ProtocolError::Unsupported,
                                            },
                                        },
                                    )
                                    .await;
                                }
                                RequestKind::Subscribe => {
                                    // Register before acknowledging, so an event
                                    // published after the client sees Subscribed
                                    // cannot be missed.
                                    let mut receiver = events.subscribe();
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Subscribed,
                                        },
                                    )
                                    .await;
                                    while let Ok(event) = receiver.recv().await {
                                        if write_event(&mut stream, &event).await.is_err() {
                                            return;
                                        }
                                        if close_after_first_event {
                                            // Dropping the stream is the disconnect the
                                            // client must recover from.
                                            return;
                                        }
                                    }
                                    return;
                                }
                            }
                        }
                    });
                }
            })
        };

        Self {
            path,
            events,
            conversations,
            history_requests,
            first_thread_updates,
            history_delay,
            list_delay,
            list_failure,
            list_blocked,
            list_gate,
            drafts,
            draft_failure,
            timeline,
            accept,
            _dir: dir,
        }
    }

    fn socket(&self) -> &Path {
        &self.path
    }

    fn publish(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn history_requests(&self) -> Vec<String> {
        self.history_requests.lock().unwrap().clone()
    }

    fn delay_history(&self, duration: Duration) {
        *self.history_delay.lock().unwrap() = duration;
    }

    fn delay_list(&self, duration: Duration) {
        *self.list_delay.lock().unwrap() = duration;
    }

    fn fail_list(&self) {
        *self.list_failure.lock().unwrap() = true;
    }

    fn block_list(&self) {
        self.list_blocked.store(true, Ordering::SeqCst);
    }

    fn release_list(&self) {
        self.list_gate.add_permits(1);
    }

    fn unblock_list(&self) {
        self.list_blocked.store(false, Ordering::SeqCst);
    }

    fn set_conversations(&self, conversations: Vec<Conversation>) {
        *self.conversations.lock().unwrap() = conversations;
    }

    fn timeline(&self) -> Vec<String> {
        self.timeline.lock().unwrap().clone()
    }

    fn seed_draft(&self, key: DraftKey, draft: DurableDraft, revision: u64) {
        self.drafts
            .lock()
            .unwrap()
            .push((key, revision, Some(draft)));
    }

    fn draft(&self, key: &DraftKey) -> Option<DurableDraft> {
        self.drafts
            .lock()
            .unwrap()
            .iter()
            .find(|(candidate, _, _)| candidate == key)
            .and_then(|(_, _, draft)| draft.clone())
    }

    fn draft_revision(&self, key: &DraftKey) -> Option<u64> {
        self.drafts
            .lock()
            .unwrap()
            .iter()
            .find(|(candidate, _, _)| candidate == key)
            .map(|(_, revision, _)| *revision)
    }

    fn fail_drafts(&self) {
        *self.draft_failure.lock().unwrap() = true;
    }

    fn receive_without_event(&self, conversation: &Conversation) {
        self.first_thread_updates.lock().unwrap().push(message(
            conversation,
            "arrived-while-subscriber-missed-event",
            50,
            "now stored, but event missed",
        ));
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn wait_for_status(
    receiver: &mut mpsc::UnboundedReceiver<ConnectionState>,
    expected: ConnectionState,
) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await {
            Ok(Some(state)) if state == expected => return,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("status channel closed before reaching {expected:?}"),
            Err(_) => panic!("status {expected:?} was never reported"),
        }
    }
}

async fn next_notice(receiver: &mut mpsc::UnboundedReceiver<SnapshotNotice>) -> SnapshotNotice {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("a snapshot notice arrives")
        .expect("snapshot channel open")
}

async fn next_snapshot(receiver: &mut mpsc::UnboundedReceiver<SnapshotNotice>) -> Snapshot {
    loop {
        if let SnapshotNotice::Completed(snapshot) = next_notice(receiver).await {
            return snapshot;
        }
    }
}

async fn next_started(receiver: &mut mpsc::UnboundedReceiver<SnapshotNotice>) -> SnapshotRequest {
    loop {
        if let SnapshotNotice::Started(request) = next_notice(receiver).await {
            return request;
        }
    }
}

async fn next_completed(receiver: &mut mpsc::UnboundedReceiver<SnapshotNotice>) -> Snapshot {
    loop {
        if let SnapshotNotice::Completed(snapshot) = next_notice(receiver).await {
            return snapshot;
        }
    }
}

fn readiness(
    phase: ReadinessPhase,
    outcome: Option<ReadinessOutcome>,
    revision: u64,
) -> ReadinessSnapshot {
    ReadinessSnapshot {
        generation: 1,
        operation: 1,
        revision,
        phase,
        outcome,
        observed_records: 2,
        can_send: phase == ReadinessPhase::Ready && outcome == Some(ReadinessOutcome::Complete),
        engine_ready: true,
        bounded_error: None,
    }
}

fn loading_app() -> App {
    let mut app = App {
        startup: StartupState::loading(),
        ..Default::default()
    };
    app.observe_startup_connection(ConnectionState::Connected);
    app.observe_startup_readiness(readiness(
        ReadinessPhase::Ready,
        Some(ReadinessOutcome::Complete),
        1,
    ));
    app
}

async fn bootstrap_snapshot(client: &bloom_ipc::client::BloomClient, app: &mut App) -> Snapshot {
    let request = app
        .begin_bootstrap_snapshot_request()
        .expect("a bootstrap read is scheduled");
    snapshot(client, request)
        .await
        .expect("the bootstrap read succeeds")
}

fn list_starts(daemon: &FakeDaemon) -> usize {
    daemon
        .timeline()
        .iter()
        .filter(|entry| entry.as_str() == "list:start")
        .count()
}

async fn wait_for_list_starts(daemon: &FakeDaemon, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if list_starts(daemon) >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the list request starts");
}

#[tokio::test]
async fn explicit_selection_reads_history_before_the_slow_list_returns() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    daemon.delay_list(Duration::from_millis(300));
    let requested = conversation("second@example.com", 10).id;

    let snap = snapshot(&client, Some(requested.clone()).into())
        .await
        .expect("an explicit selection snapshots");

    let timeline = daemon.timeline();
    let history_at = timeline
        .iter()
        .position(|entry| entry == "history:second@example.com")
        .unwrap_or_else(|| {
            panic!("history for the requested thread reached the daemon: {timeline:?}")
        });
    let list_end_at = timeline
        .iter()
        .position(|entry| entry == "list:end")
        .expect("the list response completed");
    assert!(
        history_at < list_end_at,
        "explicit history must start before the delayed list response completes: {timeline:?}"
    );
    assert_eq!(snap.visible.as_ref(), Some(&requested));
    assert_eq!(snap.history.len(), 1);
    assert_eq!(
        snap.history[0].parts(),
        &[MessagePart::Text("from history".into())]
    );
    assert_eq!(
        daemon.history_requests(),
        vec!["second@example.com".to_owned()],
        "the speculative history result is used, not fetched twice"
    );
}

#[tokio::test]
async fn list_failure_is_not_held_up_by_a_concurrent_history_read() {
    // This guards the speculative design, not a pre-existing defect: the previous
    // serial order returned the list error immediately only because it never issued
    // history first. Once history is speculative, a slow read must not delay it.
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    daemon.fail_list();
    daemon.delay_history(Duration::from_millis(600));
    let requested = conversation("second@example.com", 10).id;

    let started = std::time::Instant::now();
    let result = snapshot(&client, Some(requested).into()).await;
    let elapsed = started.elapsed();

    assert_eq!(result, Err(ClientError::Server(ProtocolError::Internal)));
    // Keep a wide margin below the 600 ms delay so a loaded machine cannot turn
    // promptness into a flake.
    assert!(
        elapsed < Duration::from_millis(500),
        "a list error must not wait for the speculative history read: {elapsed:?}"
    );
}

#[tokio::test]
async fn missing_requested_conversation_falls_back_to_the_first_thread_history() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let missing = conversation("missing@example.com", 1).id;

    let snap = snapshot(&client, Some(missing.clone()).into())
        .await
        .expect("a missing selection still snapshots the first thread");

    assert_eq!(
        snap.visible.as_ref().map(|id| id.value().to_owned()),
        Some("first@example.com".to_owned()),
        "a deleted selection falls back to the first conversation"
    );
    assert_eq!(snap.history.len(), 1);
    assert_eq!(
        snap.history[0].conversation_id(),
        &conversation("first@example.com", 20).id,
        "only the visible thread's history is published"
    );
    assert!(
        snap.history
            .iter()
            .all(|entry| entry.conversation_id() != &missing),
        "the vanished selection's history is never published"
    );
    // Outcome, not an exact request sequence: whether the speculative miss reaches
    // the daemon before the list resolves is a scheduling detail, not a contract.
    assert!(
        daemon
            .history_requests()
            .iter()
            .any(|requested| requested == "first@example.com"),
        "the fallback thread's history is read"
    );
}

#[tokio::test]
async fn selected_thread_catches_up_from_store_even_without_an_inbound_event() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = tokio::spawn(async move {
        pump_session_with_interval(
            &client,
            visible_rx,
            &event_tx,
            &snapshot_tx,
            &status_tx,
            Duration::from_millis(25),
        )
        .await
    });
    let initial = next_snapshot(&mut snapshot_rx).await;
    let selected = initial.conversations[0].clone();
    visible_tx.send(Some(selected.id.clone()).into()).unwrap();
    let selected_snapshot = next_snapshot(&mut snapshot_rx).await;
    assert_eq!(selected_snapshot.visible.as_ref(), Some(&selected.id));
    assert_eq!(initial.visible.as_ref(), Some(&selected.id));
    assert_eq!(initial.history.len(), 1);
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;

    // The daemon store changes as it would after receipt on the phone, but the
    // subscription never delivers Message. A visible transcript must catch up.
    daemon.receive_without_event(&selected);
    let refreshed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = next_snapshot(&mut snapshot_rx).await;
            if snapshot
                .history
                .iter()
                .any(|entry| entry.id().value() == "arrived-while-subscriber-missed-event")
            {
                break snapshot;
            }
        }
    })
    .await
    .expect("selected transcript catches up without an inbound event");
    assert_eq!(refreshed.visible.as_ref(), Some(&selected.id));
    assert_eq!(refreshed.history.len(), 2);
    assert!(
        refreshed.token.is_none(),
        "periodic reconciliation must be untracked"
    );
    assert!(event_rx.try_recv().is_err(), "no live event was delivered");
    assert!(
        daemon.history_requests().len() >= 2,
        "the selected history was re-read"
    );
    session.abort();
}

#[tokio::test]
async fn live_events_are_not_blocked_by_a_slow_history_reconciliation() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = tokio::spawn(async move {
        pump_session_with_interval(
            &client,
            visible_rx,
            &event_tx,
            &snapshot_tx,
            &status_tx,
            Duration::from_millis(25),
        )
        .await
    });
    let initial = next_snapshot(&mut snapshot_rx).await;
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;
    let selected = initial.conversations[0].clone();
    visible_tx.send(Some(selected.id.clone()).into()).unwrap();
    let selected_snapshot = next_snapshot(&mut snapshot_rx).await;
    assert_eq!(selected_snapshot.visible.as_ref(), Some(&selected.id));
    let previous_reads = daemon.history_requests().len();
    daemon.delay_history(Duration::from_millis(500));

    tokio::time::timeout(Duration::from_secs(1), async {
        while daemon.history_requests().len() <= previous_reads {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("periodic history request starts");
    daemon.publish(Event {
        event: EventKind::Message {
            conversation_id: selected.id.clone(),
            message: message(&selected, "during-history-read", 55, "live stays immediate"),
            push_sequence: None,
        },
    });
    let delivered = tokio::time::timeout(Duration::from_millis(200), event_rx.recv())
        .await
        .expect("history read must not block live subscription")
        .expect("event channel open");
    assert!(matches!(delivered, EventKind::Message { message, .. }
        if message.parts() == [MessagePart::Text("live stays immediate".into())]));
    session.abort();
}

#[tokio::test]
async fn watch_coalescing_never_starts_a_discarded_selection() {
    let daemon = FakeDaemon::start(false);
    let first = conversation("first@example.com", 20);
    let second = conversation("second@example.com", 10);
    let third = conversation("third@example.com", 5);
    daemon.set_conversations(vec![first.clone(), second.clone(), third.clone()]);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = tokio::spawn(async move {
        pump_session_with_interval(
            &client,
            visible_rx,
            &event_tx,
            &snapshot_tx,
            &status_tx,
            Duration::from_secs(60),
        )
        .await
    });

    let initial = next_snapshot(&mut snapshot_rx).await;
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;
    let mut app = App::default();
    app.set_conversations(initial.conversations.clone());

    // A is consumed and blocked in the daemon. B is published and then replaced
    // by C before the session can read it, so B must never start.
    daemon.block_list();
    app.select(Some(first.id.clone()));
    let request_a = app.prepare_snapshot_request();
    visible_tx.send(request_a.clone()).unwrap();
    let started_a = next_started(&mut snapshot_rx).await;
    assert_eq!(started_a, request_a);
    assert!(app.start_snapshot_activity(&started_a));
    assert!(app.has_local_activity(&first.id));
    wait_for_list_starts(&daemon, 2).await;

    app.select(Some(second.id.clone()));
    let request_b = app.prepare_snapshot_request();
    visible_tx.send(request_b.clone()).unwrap();
    app.select(Some(third.id.clone()));
    let request_c = app.prepare_snapshot_request();
    visible_tx.send(request_c.clone()).unwrap();

    daemon.release_list();
    let completed_a = next_completed(&mut snapshot_rx).await;
    app.apply_snapshot(completed_a);
    assert!(!app.has_local_activity(&first.id));

    let started_c = next_started(&mut snapshot_rx).await;
    assert_eq!(
        started_c, request_c,
        "the coalesced-away B request must never start"
    );
    assert_ne!(started_c.token, request_b.token);
    assert!(app.start_snapshot_activity(&started_c));
    assert!(app.has_local_activity(&third.id));
    assert!(!app.has_local_activity(&second.id));

    daemon.release_list();
    let completed_c = next_completed(&mut snapshot_rx).await;
    app.apply_snapshot(completed_c);
    assert!(!app.has_local_activity(&third.id));
    assert!(app.activity.is_empty(), "no residual activity remains");

    let history = daemon.history_requests();
    assert!(history.contains(&first.id.value().to_owned()));
    assert!(history.contains(&third.id.value().to_owned()));
    assert!(
        !history.contains(&second.id.value().to_owned()),
        "the discarded selection never reached history"
    );
    session.abort();
}

#[tokio::test]
async fn watch_coalescing_a_b_a_uses_distinct_tokens() {
    let daemon = FakeDaemon::start(false);
    let first = conversation("first@example.com", 20);
    let second = conversation("second@example.com", 10);
    daemon.set_conversations(vec![first.clone(), second.clone()]);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = tokio::spawn(async move {
        pump_session_with_interval(
            &client,
            visible_rx,
            &event_tx,
            &snapshot_tx,
            &status_tx,
            Duration::from_secs(60),
        )
        .await
    });

    let initial = next_snapshot(&mut snapshot_rx).await;
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;
    let mut app = App::default();
    app.set_conversations(initial.conversations.clone());

    daemon.block_list();
    app.select(Some(first.id.clone()));
    let request_a1 = app.prepare_snapshot_request();
    visible_tx.send(request_a1.clone()).unwrap();
    let started_a1 = next_started(&mut snapshot_rx).await;
    assert_eq!(started_a1, request_a1);
    assert!(app.start_snapshot_activity(&started_a1));
    wait_for_list_starts(&daemon, 2).await;

    app.select(Some(second.id.clone()));
    let request_b = app.prepare_snapshot_request();
    visible_tx.send(request_b.clone()).unwrap();
    app.select(Some(first.id.clone()));
    let request_a2 = app.prepare_snapshot_request();
    assert_ne!(request_a1.token, request_a2.token);
    visible_tx.send(request_a2.clone()).unwrap();

    daemon.release_list();
    let completed_a1 = next_completed(&mut snapshot_rx).await;
    app.apply_snapshot(completed_a1);
    assert!(!app.has_local_activity(&first.id));

    let started_a2 = next_started(&mut snapshot_rx).await;
    assert_eq!(started_a2, request_a2);
    assert_ne!(started_a2.token, request_a1.token);
    assert_ne!(started_a2.token, request_b.token);
    assert!(app.start_snapshot_activity(&started_a2));
    assert!(app.has_local_activity(&first.id));

    daemon.release_list();
    let completed_a2 = next_completed(&mut snapshot_rx).await;
    app.apply_snapshot(completed_a2);
    assert!(!app.has_local_activity(&first.id));
    assert!(app.activity.is_empty());

    let history = daemon.history_requests();
    assert!(
        !history.contains(&second.id.value().to_owned()),
        "the coalesced-away B request never reached history"
    );
    session.abort();
}

#[tokio::test]
async fn started_selection_failure_is_cleaned_up_by_disconnect() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = tokio::spawn(async move {
        pump_session_with_interval(
            &client,
            visible_rx,
            &event_tx,
            &snapshot_tx,
            &status_tx,
            Duration::from_secs(60),
        )
        .await
    });

    let initial = next_snapshot(&mut snapshot_rx).await;
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;
    let mut app = App::default();
    app.set_conversations(initial.conversations.clone());
    daemon.fail_list();
    app.select(Some(initial.conversations[0].id.clone()));
    let request = app.prepare_snapshot_request();
    visible_tx.send(request.clone()).unwrap();

    let started = next_started(&mut snapshot_rx).await;
    assert_eq!(started, request);
    assert!(app.start_snapshot_activity(&started));
    assert!(app.has_local_activity(&initial.conversations[0].id));

    let result = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("a failed selection read ends the session")
        .expect("session task joined");
    assert!(result.is_err());

    app.observe_connection_state(ConnectionState::Disconnected);
    assert!(app.activity.is_empty());
    assert!(!app.start_snapshot_activity(&started));
}

#[tokio::test]
async fn a_second_tui_completes_its_own_readiness_and_snapshot() {
    let daemon = FakeDaemon::start(false);
    let client_a = bloom_ipc::client::BloomClient::new(daemon.socket());
    let client_b = bloom_ipc::client::BloomClient::new(daemon.socket());
    let (_visible_a, visible_a) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (_visible_b, visible_b) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (events_a, mut event_a) = mpsc::unbounded_channel::<EventKind>();
    let (events_b, mut event_b) = mpsc::unbounded_channel::<EventKind>();
    let (snapshots_a, mut snapshot_a) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (snapshots_b, mut snapshot_b) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx_a, mut status_a) = mpsc::unbounded_channel::<ConnectionState>();
    let (status_tx_b, mut status_b) = mpsc::unbounded_channel::<ConnectionState>();
    let session_a = tokio::spawn(async move {
        pump_session(&client_a, visible_a, &events_a, &snapshots_a, &status_tx_a).await
    });
    let session_b = tokio::spawn(async move {
        pump_session(&client_b, visible_b, &events_b, &snapshots_b, &status_tx_b).await
    });

    let readiness_a = loop {
        if let EventKind::Readiness { snapshot } = event_a.recv().await.unwrap() {
            break snapshot;
        }
    };
    let readiness_b = loop {
        if let EventKind::Readiness { snapshot } = event_b.recv().await.unwrap() {
            break snapshot;
        }
    };
    let first = next_snapshot(&mut snapshot_a).await;
    let second = next_snapshot(&mut snapshot_b).await;
    wait_for_status(&mut status_a, ConnectionState::Connected).await;
    wait_for_status(&mut status_b, ConnectionState::Connected).await;

    let bootstrap_client_a = bloom_ipc::client::BloomClient::new(daemon.socket());
    let bootstrap_client_b = bloom_ipc::client::BloomClient::new(daemon.socket());
    let mut app = App {
        startup: StartupState::loading(),
        ..Default::default()
    };
    app.observe_startup_connection(ConnectionState::Connected);
    app.observe_startup_readiness(readiness_a);
    // The session's initial read is untracked and must not complete startup.
    app.apply_snapshot(first);
    assert!(!app.startup.snapshot_ready());
    let first_bootstrap = bootstrap_snapshot(&bootstrap_client_a, &mut app).await;
    app.apply_snapshot(first_bootstrap);
    app.observe_startup_draft_ready();
    assert!(app.startup.can_confirm());
    let mut second_app = App {
        startup: StartupState::loading(),
        ..Default::default()
    };
    second_app.observe_startup_connection(ConnectionState::Connected);
    second_app.observe_startup_readiness(readiness_b);
    second_app.apply_snapshot(second);
    assert!(!second_app.startup.snapshot_ready());
    let second_bootstrap = bootstrap_snapshot(&bootstrap_client_b, &mut second_app).await;
    second_app.apply_snapshot(second_bootstrap);
    second_app.observe_startup_draft_ready();
    assert!(
        second_app.startup.can_confirm(),
        "second TUI must finish its own ticketed snapshot"
    );
    let lists = daemon
        .timeline()
        .iter()
        .filter(|entry| entry.as_str() == "list:start")
        .count();
    assert!(
        lists >= 2,
        "each TUI reads its own local list: {:?}",
        daemon.timeline()
    );
    assert!(
        !daemon.timeline().iter().any(|entry| entry.contains("sync")),
        "the TUI must not trigger an engine listing"
    );
    session_a.abort();
    session_b.abort();
}

#[tokio::test]
async fn startup_readiness_is_reported_before_the_client_snapshot() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());

    let (_visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();
    let session = {
        let client = client.clone();
        tokio::spawn(async move {
            pump_session(&client, visible_rx, &event_tx, &snapshot_tx, &status_tx).await
        })
    };

    let readiness = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("readiness event arrives")
        .expect("event channel open");
    let EventKind::Readiness {
        snapshot: readiness,
    } = readiness
    else {
        panic!("expected readiness before the first snapshot");
    };
    let initial = next_snapshot(&mut snapshot_rx).await;
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;

    let mut app = App {
        startup: StartupState::loading(),
        ..Default::default()
    };
    app.observe_startup_connection(ConnectionState::Connected);
    app.observe_startup_readiness(readiness);
    app.apply_snapshot(initial);
    assert!(
        !app.startup.snapshot_ready(),
        "the session's initial read is not the bootstrap read"
    );
    let bootstrap = bootstrap_snapshot(&client, &mut app).await;
    app.apply_snapshot(bootstrap);
    app.observe_startup_draft_ready();
    assert!(app.startup.can_confirm());
    assert!(app.open_bloom());
    assert!(app.startup.opened());
    session.abort();
}

#[tokio::test]
async fn snapshot_started_before_readiness_never_completes_startup() {
    for release_before_readiness in [true, false] {
        let daemon = FakeDaemon::start(false);
        daemon.block_list();
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let mut app = App {
            startup: StartupState::loading(),
            ..Default::default()
        };
        app.observe_startup_connection(ConnectionState::Connected);
        app.observe_startup_readiness(readiness(ReadinessPhase::Refreshing, None, 1));

        let pending = tokio::spawn({
            let client = client.clone();
            async move { snapshot(&client, None.into()).await }
        });
        wait_for_list_starts(&daemon, 1).await;

        let ready = readiness(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete), 2);
        if release_before_readiness {
            daemon.release_list();
            let early = pending.await.unwrap().unwrap();
            app.apply_snapshot(early);
            app.observe_startup_readiness(ready);
        } else {
            app.observe_startup_readiness(ready);
            daemon.release_list();
            let late = pending.await.unwrap().unwrap();
            app.apply_snapshot(late);
        }
        daemon.unblock_list();

        assert!(
            !app.startup.snapshot_ready(),
            "an untracked read started before readiness cannot complete startup              (release_before={release_before_readiness})"
        );
        assert!(!app.startup.can_confirm());
        assert!(!app.outbound_allowed());
        assert!(!app.startup.checks()[4]);

        let bootstrap = bootstrap_snapshot(&client, &mut app).await;
        app.apply_snapshot(bootstrap);
        app.observe_startup_draft_ready();
        assert!(
            app.startup.can_confirm(),
            "a ticketed post-readiness read plus a matching draft confirms              (release_before={release_before_readiness})"
        );
        assert!(app.open_bloom());
        assert!(app.outbound_allowed());
    }
}

#[tokio::test]
async fn empty_conversation_list_completes_without_an_activity_token() {
    let daemon = FakeDaemon::start(false);
    daemon.set_conversations(Vec::new());
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let mut app = loading_app();

    let request = app
        .begin_bootstrap_snapshot_request()
        .expect("ready readiness schedules a read");
    assert!(
        request.token.is_none(),
        "bootstrap reads carry no activity token"
    );
    let snapshot = snapshot(&client, request)
        .await
        .expect("empty list snapshots");
    assert!(snapshot.conversations.is_empty());
    assert!(snapshot.token.is_none());
    assert!(snapshot.bootstrap.is_some());

    app.apply_snapshot(snapshot);
    assert!(app.startup.snapshot_ready());
    app.observe_startup_draft_ready();
    assert!(app.startup.can_confirm());
    assert!(app.open_bloom());
    assert!(app.outbound_allowed());
}

#[tokio::test]
async fn duplicate_ready_does_not_launch_a_second_bootstrap_read() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let mut app = loading_app();
    let request = app
        .begin_bootstrap_snapshot_request()
        .expect("ready readiness schedules a read");
    let before = list_starts(&daemon);

    app.observe_startup_readiness(readiness(
        ReadinessPhase::Ready,
        Some(ReadinessOutcome::Complete),
        1,
    ));
    assert!(app.begin_bootstrap_snapshot_request().is_none());

    let snapshot = snapshot(&client, request)
        .await
        .expect("the first bootstrap read succeeds");
    app.apply_snapshot(snapshot);
    app.observe_startup_readiness(readiness(
        ReadinessPhase::Ready,
        Some(ReadinessOutcome::Complete),
        1,
    ));
    assert!(app.begin_bootstrap_snapshot_request().is_none());
    assert_eq!(
        list_starts(&daemon),
        before + 1,
        "a replayed Ready must not launch another read"
    );
}

#[tokio::test]
async fn stale_ticketed_revision_gets_one_replacement() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    let mut app = loading_app();
    let request = app
        .begin_bootstrap_snapshot_request()
        .expect("ready readiness schedules a read");
    let ticket = request.bootstrap.expect("bootstrap ticket");

    // A newer untracked read wins the revision race.
    let newer = snapshot(&client, None.into())
        .await
        .expect("newer snapshot succeeds");
    app.apply_snapshot(newer);
    app.apply_snapshot(Snapshot {
        token: None,
        bootstrap: Some(ticket),
        revision: 1,
        visible: None,
        conversations: Vec::new(),
        history: Vec::new(),
    });
    assert!(
        !app.startup.snapshot_ready(),
        "a rejected ticketed read cannot complete startup"
    );

    let replacement = app
        .begin_bootstrap_snapshot_request()
        .expect("one replacement is scheduled");
    let fresh = snapshot(&client, replacement)
        .await
        .expect("replacement snapshot succeeds");
    app.apply_snapshot(fresh);
    app.observe_startup_draft_ready();
    assert!(app.startup.can_confirm());
}

#[tokio::test]
async fn session_snapshots_events_and_resyncs_the_selected_thread() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());

    let (visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();

    let session = {
        let client = client.clone();
        tokio::spawn(async move {
            pump_session(&client, visible_rx, &event_tx, &snapshot_tx, &status_tx).await
        })
    };

    wait_for_status(&mut status_rx, ConnectionState::Connecting).await;

    let first = next_snapshot(&mut snapshot_rx).await;
    assert_eq!(first.conversations.len(), 2);
    assert_eq!(
        first.visible.as_ref().unwrap().value(),
        "first@example.com",
        "the first conversation is snapshotted when nothing is selected"
    );
    assert_eq!(first.history.len(), 1);
    assert_eq!(
        first.history[0].parts(),
        &[MessagePart::Text("from history".into())]
    );
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;

    // A live event is forwarded to the UI.
    let target = first.conversations[0].clone();
    daemon.publish(Event {
        event: EventKind::Message {
            conversation_id: target.id.clone(),
            message: message(&target, "live-1", 40, "live"),
            push_sequence: None,
        },
    });
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = event_rx.recv().await.expect("event channel open");
            if !matches!(event, EventKind::Readiness { .. }) {
                break event;
            }
        }
    })
    .await
    .expect("an event arrives");
    match event {
        EventKind::Message { message, .. } => {
            assert_eq!(message.parts(), &[MessagePart::Text("live".into())])
        }
        _ => panic!("expected a message event"),
    }

    // Selecting another conversation triggers a fresh read of that thread.
    let second = first.conversations[1].id.clone();
    visible_tx.send(Some(second.clone()).into()).unwrap();
    let resynced = next_snapshot(&mut snapshot_rx).await;
    assert_eq!(resynced.visible.as_ref().unwrap(), &second);
    assert_eq!(resynced.history.len(), 1);
    assert!(
        daemon
            .history_requests()
            .contains(&second.value().to_owned()),
        "history must be requested for the newly selected conversation"
    );

    session.abort();
}

#[tokio::test]
async fn a_lost_subscription_surfaces_as_an_error_so_the_caller_can_reconnect() {
    let daemon = FakeDaemon::start(true);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());

    let (_visible_tx, visible_rx) = watch::channel::<SnapshotRequest>(SnapshotRequest::default());
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<SnapshotNotice>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<ConnectionState>();

    let session = {
        let client = client.clone();
        tokio::spawn(async move {
            pump_session(&client, visible_rx, &event_tx, &snapshot_tx, &status_tx).await
        })
    };

    let first = next_snapshot(&mut snapshot_rx).await;
    assert_eq!(first.conversations.len(), 2);
    wait_for_status(&mut status_rx, ConnectionState::Connected).await;

    let target = first.conversations[0].clone();
    daemon.publish(Event {
        event: EventKind::Message {
            conversation_id: target.id.clone(),
            message: message(&target, "live-1", 40, "live"),
            push_sequence: None,
        },
    });

    let result = tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("the session ends after the daemon drops it")
        .expect("session task joined");
    assert!(
        result.is_err(),
        "a dropped subscription must be reported so the caller resyncs"
    );
}

mod durable_drafts {
    use super::*;
    use bloom_tui::app::{Action, App};
    use bloom_tui::drafts::{
        DraftCoordinator, DraftOperation, DraftResult, RestoreCandidate, SYNC_ERROR,
    };
    use bloom_tui::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;

    fn draft_key(conversation: &Conversation) -> DraftKey {
        DraftKey::Conversation {
            owner: conversation.id.clone(),
        }
    }

    fn durable(body: &str) -> DurableDraft {
        DurableDraft {
            body: body.into(),
            cursor: body.chars().count(),
            pasted_body: false,
            uncertain: false,
        }
    }

    async fn execute(
        client: &bloom_ipc::client::BloomClient,
        operation: DraftOperation,
    ) -> DraftResult {
        match operation {
            DraftOperation::Load { request, key } => DraftResult::Load {
                request,
                key: key.clone(),
                result: client.draft_load(key).await,
            },
            DraftOperation::Save {
                request,
                key,
                revision,
                draft,
            } => DraftResult::Save {
                request,
                key: key.clone(),
                revision,
                result: client.draft_save(key, draft, revision).await,
            },
            DraftOperation::Delete {
                request,
                key,
                revision,
            } => DraftResult::Delete {
                request,
                key: key.clone(),
                revision,
                result: client.draft_delete(key, revision).await,
            },
        }
    }

    async fn apply_operation(
        app: &mut App,
        coordinator: &mut DraftCoordinator,
        client: &bloom_ipc::client::BloomClient,
        operation: DraftOperation,
    ) -> Option<RestoreCandidate> {
        let candidate = coordinator.apply_result(execute(client, operation).await);
        if let Some(candidate) = &candidate {
            app.apply_loaded_draft(candidate.key.clone(), candidate.draft.clone());
        }
        if let Some(error) = coordinator.take_error() {
            app.error = Some(error);
        }
        candidate
    }

    async fn step(
        app: &mut App,
        coordinator: &mut DraftCoordinator,
        client: &bloom_ipc::client::BloomClient,
        now: Instant,
    ) -> Option<RestoreCandidate> {
        coordinator.observe(app.current_persistable_owner(), app.desired_drafts(), now);
        let operation = coordinator.take_ready(now)?;
        apply_operation(app, coordinator, client, operation).await
    }

    #[tokio::test]
    async fn load_restore_debounced_save_and_clear_delete_round_trip() {
        let daemon = FakeDaemon::start(false);
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let chat = conversation("first@example.com", 20);
        let key = draft_key(&chat);
        daemon.seed_draft(key.clone(), durable("stored body"), 4);

        let mut app = App::default();
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let mut coordinator = DraftCoordinator::new();
        let start = Instant::now();

        assert!(step(&mut app, &mut coordinator, &client, start)
            .await
            .is_some());
        assert_eq!(app.draft, "stored body");
        assert_eq!(coordinator.known_floor(&key), Some(4));

        app.draft = "debounced local body".into();
        app.draft_cursor = app.draft.chars().count();
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(10),
        );
        assert!(coordinator
            .take_ready(start + Duration::from_millis(309))
            .is_none());
        let save = coordinator
            .take_ready(start + Duration::from_millis(310))
            .expect("save after debounce");
        apply_operation(&mut app, &mut coordinator, &client, save).await;
        assert_eq!(
            daemon.draft(&key).expect("stored save").body,
            "debounced local body"
        );
        assert_eq!(daemon.draft_revision(&key), Some(5));

        app.draft.clear();
        app.draft_cursor = 0;
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(320),
        );
        let delete = coordinator
            .take_ready(start + Duration::from_millis(620))
            .expect("delete after debounce");
        assert!(matches!(delete, DraftOperation::Delete { .. }));
        apply_operation(&mut app, &mut coordinator, &client, delete).await;
        assert_eq!(daemon.draft(&key), None);
    }

    #[tokio::test]
    async fn owner_switches_keep_drafts_isolated_and_ignore_a_late_load() {
        let daemon = FakeDaemon::start(false);
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let first = conversation("first@example.com", 20);
        let second = conversation("second@example.com", 10);
        let first_key = draft_key(&first);
        let second_key = draft_key(&second);
        daemon.seed_draft(first_key.clone(), durable("first stored"), 2);
        let mut app = App::default();
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.select(Some(first.id.clone()));
        let mut coordinator = DraftCoordinator::new();
        let start = Instant::now();

        coordinator.observe(app.current_persistable_owner(), app.desired_drafts(), start);
        let late = coordinator.take_ready(start).expect("first load");
        app.select(Some(second.id.clone()));
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(1),
        );
        let result = execute(&client, late).await;
        assert!(coordinator.apply_result(result).is_none());
        assert!(
            app.draft.is_empty(),
            "late first-chat load must not restore"
        );
        assert!(step(
            &mut app,
            &mut coordinator,
            &client,
            start + Duration::from_millis(2)
        )
        .await
        .is_none());

        app.draft = "second local".into();
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(3),
        );
        let operation = coordinator
            .take_ready(start + Duration::from_millis(303))
            .expect("second save");
        apply_operation(&mut app, &mut coordinator, &client, operation).await;
        assert_eq!(
            daemon.draft(&second_key).expect("second saved").body,
            "second local"
        );
        assert_eq!(
            daemon.draft(&first_key).expect("first untouched").body,
            "first stored"
        );

        app.select(Some(first.id.clone()));
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(303),
        );
        assert!(app.desired_drafts().contains_key(&second_key));
        assert!(
            !app.desired_drafts().contains_key(&first_key),
            "the first chat's empty composer overrides no other owner"
        );
        assert!(step(
            &mut app,
            &mut coordinator,
            &client,
            start + Duration::from_millis(304)
        )
        .await
        .is_some());
        assert_eq!(app.draft, "first stored");
        assert_ne!(first_key, second_key);
    }

    #[tokio::test]
    async fn send_success_deletes_the_parked_draft() {
        let daemon = FakeDaemon::start(false);
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let chat = conversation("first@example.com", 20);
        let key = draft_key(&chat);
        let mut app = App::default();
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let mut coordinator = DraftCoordinator::new();
        let start = Instant::now();
        step(&mut app, &mut coordinator, &client, start).await;

        app.draft = "send body".into();
        app.draft_cursor = app.draft.chars().count();
        let Action::SendMessage {
            attempt,
            conversation: target,
            ..
        } = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("send action expected");
        };
        app.begin_send(attempt, Some(target.clone()));
        step(
            &mut app,
            &mut coordinator,
            &client,
            start + Duration::from_millis(10),
        )
        .await;
        let save = coordinator
            .take_ready(start + Duration::from_millis(310))
            .expect("parked save");
        apply_operation(&mut app, &mut coordinator, &client, save).await;
        assert_eq!(daemon.draft(&key).expect("parked stored").body, "send body");

        app.finish_send(attempt, Some(target), Ok(()));
        assert!(app.parked_send.is_none());
        coordinator.observe(
            app.current_persistable_owner(),
            app.desired_drafts(),
            start + Duration::from_millis(320),
        );
        let delete = coordinator
            .take_ready(start + Duration::from_millis(620))
            .expect("send success delete");
        assert!(matches!(delete, DraftOperation::Delete { .. }));
        apply_operation(&mut app, &mut coordinator, &client, delete).await;
        assert_eq!(daemon.draft(&key), None);
    }

    #[tokio::test]
    async fn send_failure_clears_parked_copy_and_restores_recovery_text() {
        let daemon = FakeDaemon::start(false);
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let chat = conversation("first@example.com", 20);
        let key = draft_key(&chat);
        let mut app = App::default();
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let mut coordinator = DraftCoordinator::new();
        let start = Instant::now();
        step(&mut app, &mut coordinator, &client, start).await;

        app.draft = "failed send body".into();
        app.draft_cursor = app.draft.chars().count();
        let Action::SendMessage {
            attempt,
            conversation: target,
            ..
        } = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("send action expected");
        };
        app.begin_send(attempt, Some(target.clone()));
        step(
            &mut app,
            &mut coordinator,
            &client,
            start + Duration::from_millis(10),
        )
        .await;
        let save = coordinator
            .take_ready(start + Duration::from_millis(310))
            .expect("parked save");
        apply_operation(&mut app, &mut coordinator, &client, save).await;
        assert!(daemon.draft(&key).is_some());

        app.finish_send(attempt, Some(target), Err("rejected".into()));
        assert!(app.parked_send.is_none());
        assert_eq!(app.draft, "failed send body");
        assert!(app.desired_drafts().contains_key(&key));
        assert_eq!(
            app.desired_drafts()
                .get(&key)
                .expect("recovery desired")
                .body,
            "failed send body"
        );
    }

    #[tokio::test]
    async fn daemon_failure_keeps_text_and_surfaces_the_generic_error() {
        let daemon = FakeDaemon::start(false);
        let client = bloom_ipc::client::BloomClient::new(daemon.socket());
        let chat = conversation("first@example.com", 20);
        let mut app = App::default();
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let mut coordinator = DraftCoordinator::new();
        let start = Instant::now();
        step(&mut app, &mut coordinator, &client, start).await;

        app.draft = "kept despite failure".into();
        app.draft_cursor = app.draft.chars().count();
        daemon.fail_drafts();
        step(
            &mut app,
            &mut coordinator,
            &client,
            start + Duration::from_millis(10),
        )
        .await;
        let save = coordinator
            .take_ready(start + Duration::from_millis(310))
            .expect("failed save");
        apply_operation(&mut app, &mut coordinator, &client, save).await;
        assert_eq!(app.draft, "kept despite failure");
        assert_eq!(app.error.as_deref(), Some(SYNC_ERROR));
    }
}
