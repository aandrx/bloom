//! Headless tests for the TUI's daemon session logic, driven by a fake daemon over
//! a real Unix socket. No terminal and no real daemon are involved.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::{
    client::ClientError, read_request, write_event, write_response, ConversationSyncStatus, Event,
    EventKind, ProtocolError, RequestKind, Response, ResponseKind,
};
use bloom_tui::daemon::{
    pump_session, pump_session_with_interval, snapshot, ConnectionState, Snapshot,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::UnixListener,
    sync::{broadcast, mpsc, watch},
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

struct FakeDaemon {
    path: PathBuf,
    events: broadcast::Sender<Event>,
    history_requests: Arc<Mutex<Vec<String>>>,
    first_thread_updates: Arc<Mutex<Vec<Message>>>,
    history_delay: Arc<Mutex<Duration>>,
    list_delay: Arc<Mutex<Duration>>,
    list_failure: Arc<Mutex<bool>>,
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

        let conversations = vec![
            conversation("first@example.com", 20),
            conversation("second@example.com", 10),
        ];
        let (events, _) = broadcast::channel::<Event>(16);
        let history_requests = Arc::new(Mutex::new(Vec::new()));
        let first_thread_updates = Arc::new(Mutex::new(Vec::new()));
        let history_delay = Arc::new(Mutex::new(Duration::ZERO));
        let list_delay = Arc::new(Mutex::new(Duration::ZERO));
        let list_failure = Arc::new(Mutex::new(false));
        let timeline = Arc::new(Mutex::new(Vec::new()));

        let accept = {
            let events = events.clone();
            let history_requests = history_requests.clone();
            let first_thread_updates = first_thread_updates.clone();
            let history_delay = history_delay.clone();
            let list_delay = list_delay.clone();
            let list_failure = list_failure.clone();
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
                    let timeline = timeline.clone();
                    let conversations = conversations.clone();
                    tokio::spawn(async move {
                        while let Ok(request) = read_request(&mut stream).await {
                            let request_id = request.request_id;
                            match request.request {
                                RequestKind::ListConversations => {
                                    timeline.lock().unwrap().push("list:start".to_owned());
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
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::Conversations {
                                                conversations: conversations.clone(),
                                            },
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
                                            if conversation.id == conversations[0].id {
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
                                RequestKind::SyncConversations => {
                                    let _ = write_response(
                                        &mut stream,
                                        &Response {
                                            request_id,
                                            response: ResponseKind::ConversationSync {
                                                status: ConversationSyncStatus::Complete,
                                                conversations: conversations.len() as u32,
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
            history_requests,
            first_thread_updates,
            history_delay,
            list_delay,
            list_failure,
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

    fn timeline(&self) -> Vec<String> {
        self.timeline.lock().unwrap().clone()
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

async fn next_snapshot(receiver: &mut mpsc::UnboundedReceiver<Snapshot>) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("a snapshot arrives")
        .expect("snapshot channel open")
}

#[tokio::test]
async fn explicit_selection_reads_history_before_the_slow_list_returns() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());
    daemon.delay_list(Duration::from_millis(300));
    let requested = conversation("second@example.com", 10).id;

    let snap = snapshot(&client, Some(requested.clone()))
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
    let result = snapshot(&client, Some(requested)).await;
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

    let snap = snapshot(&client, Some(missing.clone()))
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
    let (visible_tx, visible_rx) = watch::channel::<Option<ConversationId>>(None);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
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
    visible_tx.send(Some(selected.id.clone())).unwrap();
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
    let (visible_tx, visible_rx) = watch::channel::<Option<ConversationId>>(None);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
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
    visible_tx.send(Some(selected.id.clone())).unwrap();
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
async fn session_snapshots_events_and_resyncs_the_selected_thread() {
    let daemon = FakeDaemon::start(false);
    let client = bloom_ipc::client::BloomClient::new(daemon.socket());

    let (visible_tx, visible_rx) = watch::channel::<Option<ConversationId>>(None);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
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
    let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("an event arrives")
        .expect("event channel open");
    match event {
        EventKind::Message { message, .. } => {
            assert_eq!(message.parts(), &[MessagePart::Text("live".into())])
        }
        _ => panic!("expected a message event"),
    }

    // Selecting another conversation triggers a fresh read of that thread.
    let second = first.conversations[1].id.clone();
    visible_tx.send(Some(second.clone())).unwrap();
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

    let (_visible_tx, visible_rx) = watch::channel::<Option<ConversationId>>(None);
    let (event_tx, _event_rx) = mpsc::unbounded_channel::<EventKind>();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<Snapshot>();
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
