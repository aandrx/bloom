//! Headless tests for the TUI's daemon session logic, driven by a fake daemon over
//! a real Unix socket. No terminal and no real daemon are involved.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::{
    read_request, write_event, write_response, Event, EventKind, RequestKind, Response,
    ResponseKind,
};
use bloom_tui::daemon::{pump_session, ConnectionState, Snapshot};
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

        let accept = {
            let events = events.clone();
            let history_requests = history_requests.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let events = events.clone();
                    let history_requests = history_requests.clone();
                    let conversations = conversations.clone();
                    tokio::spawn(async move {
                        while let Ok(request) = read_request(&mut stream).await {
                            let request_id = request.request_id;
                            match request.request {
                                RequestKind::ListConversations => {
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
                                } => {
                                    history_requests
                                        .lock()
                                        .unwrap()
                                        .push(conversation_id.value().to_owned());
                                    let target = conversations
                                        .iter()
                                        .find(|c| c.id == conversation_id)
                                        .cloned();
                                    let messages = match target {
                                        Some(conversation) => vec![message(
                                            &conversation,
                                            "history-1",
                                            5,
                                            "from history",
                                        )],
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
