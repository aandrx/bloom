//! Daemon integration test driven by a fake engine sidecar.
//!
//! The fake sidecar speaks the real stdio protocol, so this exercises the actual
//! child-process path: spawn, readiness, inbound event, persistence, fanout, send
//! correlation and shutdown. It does not need the engine library or the network.

use bloom_domain::{MessagePart, OutgoingBody};
use bloom_ipc::EventKind;
use bloom_secrets::SecretStoreError;
use bloom_transport::Transport;
use bloomd::{
    engine::{Engine, EngineConfig},
    ipc::{RequestHandler, SubscriptionHub},
    service::Service,
    store::{self, EncryptedStore},
    supervisor::Supervisor,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

/// A minimal in-memory secret store so tests never touch the keychain.
struct TestSecrets;
impl bloom_secrets::SecretStore for TestSecrets {
    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, SecretStoreError> {
        Ok(Some(vec![9u8; 32]))
    }
    fn set(&self, _key: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
        Ok(())
    }
}

fn fake_sidecar(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine.sh");
    let script = r#"#!/bin/sh
# Ignores its arguments; speaks the sidecar protocol on stdio.
cat <<'FRAMES'
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":false,"recipient_lists":true}}
{"version":1,"type":"inbound_message","sequence":1,"payload":{"variant":"iMessage","complete":true,"inst":{"id":"inbound-1","sender":"tel:+15550000001","conversation":{"participants":["tel:+15550000001","mailto:me@example.invalid"],"cv_name":null,"sender_guid":"iMessage;-;+15550000001","after_guid":null},"message":{"type":"normal","complete":true,"value":{"parts":[{"idx":0,"part":{"type":"text","text":"first"}}],"service":{"type":"iMessage","complete":true}}},"sent_timestamp_ms":1700000000000,"send_delivered":false,"verification_failed":false}}}
{"version":1,"type":"inbound_message","sequence":2,"payload":{"variant":"iMessage","complete":true,"inst":{"id":"group-inbound-1","sender":"mailto:one@example.invalid","conversation":{"participants":["mailto:one@example.invalid","tel:+15550000002","mailto:me@example.invalid"],"cv_name":"Two people","sender_guid":"iMessage;-;group-with-two-people","after_guid":null},"message":{"type":"normal","complete":true,"value":{"parts":[{"idx":0,"part":{"type":"text","text":"group first"}}],"service":{"type":"iMessage","complete":true}}},"sent_timestamp_ms":1700000000001,"send_delivered":false,"verification_failed":false}}}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"send"'*)
      printf '%s\n' "$line" >> "$0.send-log"
      id=$(printf '%s' "$line" | sed -n 's/.*"request_id":\([0-9]*\).*/\1/p')
      printf '{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"outbound-%s","accepted":true}\n' "$id" "$id"
      ;;
    *'"type":"shutdown"'*)
      printf '{"version":1,"type":"shutdown_ack","request_id":1}\n'
      exit 0
      ;;
  esac
done
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn malformed_stdout_terminates_engine_and_rejects_later_sends() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("malformed.sh");
    fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"ready\",\"handles\":[\"tel:+15550000000\"]}' 'not-json'\nexec sleep 30\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let engine = Engine::new(EngineConfig {
        dart_executable: script.clone(),
        script: PathBuf::new(),
        state_dir: dir.path().into(),
        library: PathBuf::new(),
        sync_chats: false,
    });
    let mut events = engine.take_events().await.unwrap();
    engine.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if matches!(
                events.recv().await,
                Some(bloom_transport::TransportEvent::Session {
                    state: bloom_domain::SessionState::Disconnected,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        engine
            .send_text(&["tel:+15550000001".into()], None, "x")
            .await,
        Err(bloom_transport::TransportError::TransportUnavailable)
    );
    engine.stop().await.unwrap();
    engine.stop().await.unwrap();
}

fn lifecycle_engine(script: PathBuf, dir: &std::path::Path) -> Arc<Engine> {
    Arc::new(Engine::new(EngineConfig {
        dart_executable: script,
        script: PathBuf::new(),
        state_dir: dir.into(),
        library: PathBuf::new(),
        sync_chats: false,
    }))
}

fn child_running(script: &std::path::Path) -> bool {
    // The fake sidecar writes its pid shortly after spawn. Poll briefly so a
    // loaded machine cannot fail this check before the file exists.
    let pid_path = script.with_extension("pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let pid = loop {
        match fs::read_to_string(&pid_path) {
            Ok(pid) => break pid,
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fake sidecar pid file is unreadable: {error}"),
        }
    };
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn stream_failure_preserves_prior_messages_fails_sync_and_reaps_child() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let template = fs::read_to_string(fake_sidecar(dir.path())).unwrap();
    let preamble = template.split_once("while IFS= read").unwrap().0;
    let rejected_frame = template
        .lines()
        .find(|line| line.contains("\"id\":\"inbound-1\""))
        .unwrap()
        .replace("inbound-1", "after-stream-failure");
    let script = dir.path().join("failure.sh");
    fs::write(&script, format!(
        "{preamble}\nprintf '%s' \"$$\" > \"${{0%.sh}}.pid\"\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"sync_conversations\"'*)\n      printf '%s\\n' 'not-json' '{rejected_frame}'\n      exec sleep 30\n      ;;\n  esac\ndone\n"
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let engine = lifecycle_engine(script.clone(), &private);
    let events = engine.take_events().await.unwrap();
    let store = Arc::new(Mutex::new(
        EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap(),
    ));
    let hub = SubscriptionHub::with_default_capacity();
    let mut subscriber = hub.subscribe();
    let service = Arc::new(Service::new(store, engine.clone(), hub));
    let persistence = tokio::spawn(service.clone().persist_events(events));
    engine.start().await.unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while received.len() < 2 {
            if let EventKind::Message {
                conversation_id,
                message,
                ..
            } = subscriber.recv().await.unwrap().event
            {
                received.push((conversation_id, message));
            }
        }
    })
    .await
    .unwrap();
    assert!(!engine.my_handles().is_empty());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), engine.sync_conversations())
            .await
            .unwrap(),
        Err(bloom_transport::TransportError::TransportUnavailable),
        "A dead reader must not leave metadata sync waiting for its 120-second deadline"
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if matches!(
                subscriber.recv().await.unwrap().event,
                EventKind::Session {
                    state: bloom_domain::SessionState::Disconnected,
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !child_running(&script),
        "Failed child must be reaped before disconnected is published"
    );
    assert!(engine.my_handles().is_empty());
    for (id, message) in received {
        let history = service.history(id, 10).await.unwrap();
        assert_eq!(
            history.len(),
            1,
            "Frames after malformed stdout must not be ingested"
        );
        assert_eq!(history[0].id(), message.id());
    }
    assert_eq!(
        engine
            .send_text(&["tel:+15550000001".into()], None, "x")
            .await,
        Err(bloom_transport::TransportError::TransportUnavailable)
    );
    assert_eq!(
        engine.start().await,
        Err(bloom_transport::TransportError::TransportUnavailable),
        "One-shot Engine cannot restart and must not spawn an orphan on another start"
    );
    assert!(!child_running(&script));
    engine.stop().await.unwrap();
    engine.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), persistence)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn graceful_shutdown_persists_the_sidecars_final_message() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let template = fs::read_to_string(fake_sidecar(dir.path())).unwrap();
    let preamble = template.split_once("while IFS= read").unwrap().0;
    let final_frame = template
        .lines()
        .find(|line| line.contains("\"id\":\"inbound-1\""))
        .unwrap()
        .replace("inbound-1", "shutdown-final");
    let script = dir.path().join("graceful.sh");
    fs::write(&script, format!(
        "{preamble}\nprintf '%s' \"$$\" > \"${{0%.sh}}.pid\"\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"shutdown\"'*)\n      sleep 0.05\n      printf '%s\\n' '{final_frame}' '{{\"version\":1,\"type\":\"shutdown_ack\",\"request_id\":1}}'\n      exit 0\n      ;;\n  esac\ndone\n"
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let engine = lifecycle_engine(script.clone(), &private);
    let events = engine.take_events().await.unwrap();
    let store = Arc::new(Mutex::new(
        EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap(),
    ));
    let hub = SubscriptionHub::with_default_capacity();
    let mut subscriber = hub.subscribe();
    let service = Arc::new(Service::new(store, engine.clone(), hub));
    let persistence = tokio::spawn(service.clone().persist_events(events));
    engine.start().await.unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while received.len() < 2 {
            if let EventKind::Message {
                conversation_id, ..
            } = subscriber.recv().await.unwrap().event
            {
                received.push(conversation_id);
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let (first, second) = tokio::join!(engine.stop(), engine.stop());
        first.unwrap();
        second.unwrap();
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), persistence)
        .await
        .unwrap()
        .unwrap();
    let history = service.history(received[0].clone(), 10).await.unwrap();
    let expected =
        bloom_domain::MessageId::new(received[0].transport().clone(), "shutdown-final").unwrap();
    assert!(
        history.iter().any(|message| message.id() == &expected),
        "A final message emitted before shutdown acknowledgment must be durably persisted"
    );
    assert!(!child_running(&script));
    engine.stop().await.unwrap();
}

async fn blocked_shutdown_engine(
    dir: &std::path::Path,
) -> (
    Arc<Engine>,
    mpsc::Receiver<bloom_transport::TransportEvent>,
    PathBuf,
) {
    let script = dir.join("blocked-shutdown.sh");
    let ready = "{\"type\":\"ready\",\"handles\":[\"tel:+15550000000\"],\"capabilities\":{\"recipient_lists\":true}}\n";
    fs::write(&script, format!(
        "#!/bin/sh\nprintf '%s' \"$$\" > \"${{0%.sh}}.pid\"\ncat <<'FRAMES'\n{}FRAMES\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"type\":\"shutdown\"'*)\n      printf 'shutdown' > \"$0.shutdown\"\n      exec sleep 30\n      ;;\n  esac\ndone\n",
        ready.repeat(bloomd::engine::EVENT_CAPACITY + 4)
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let engine = lifecycle_engine(script.clone(), dir);
    let mut events = engine.take_events().await.unwrap();
    engine.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    (engine, events, script)
}

#[tokio::test]
async fn graceful_shutdown_is_bounded_with_a_stalled_event_consumer() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, events, script) = blocked_shutdown_engine(dir.path()).await;
    tokio::time::timeout(Duration::from_secs(18), engine.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !child_running(&script),
        "An uncooperative sidecar must be reaped after the drain deadline"
    );
    assert!(engine.my_handles().is_empty());
    drop(events);
    engine.stop().await.unwrap();
}

#[tokio::test]
async fn cancelling_stop_does_not_cancel_the_readers_cleanup_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, events, script) = blocked_shutdown_engine(dir.path()).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), engine.stop())
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while child_running(&script) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Cancellation of the caller must not orphan the child");
    assert!(
        script.with_extension("sh.shutdown").exists(),
        "The fixture must have received the shutdown request"
    );
    assert!(engine.my_handles().is_empty());
    drop(events);
    engine.stop().await.unwrap();
}

#[tokio::test]
async fn dropping_event_consumer_reaps_child_even_with_backpressure() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("backpressure.sh");
    let ready = "{\"type\":\"ready\",\"handles\":[\"tel:+15550000000\"],\"capabilities\":{\"recipient_lists\":true}}\n";
    fs::write(&script, format!(
        "#!/bin/sh\nprintf '%s' \"$$\" > \"${{0%.sh}}.pid\"\ncat <<'FRAMES'\n{}FRAMES\nexec sleep 30\n",
        ready.repeat(bloomd::engine::EVENT_CAPACITY + 2)
    )).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let engine = lifecycle_engine(script.clone(), dir.path());
    let mut events = engine.take_events().await.unwrap();
    engine.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(child_running(&script));
    drop(events);
    tokio::time::timeout(Duration::from_secs(4), async {
        while child_running(&script) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(engine.my_handles().is_empty());
    assert_eq!(
        engine
            .send_text(&["tel:+15550000001".into()], None, "x")
            .await,
        Err(bloom_transport::TransportError::TransportUnavailable)
    );
    engine.stop().await.unwrap();
}

#[tokio::test]
async fn exact_direct_identity_reconciles_split_histories_at_the_service_boundary() {
    use bloom_domain::{
        Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
        MessageId, Participant, TransportId,
    };
    use bloom_transport::TransportEvent;

    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let handle = Handle::new(HandleKind::Email, "person@example.test").unwrap();
    let transport = TransportId::new("openbubbles").unwrap();
    let local = Conversation {
        id: ConversationId::new(transport.clone(), "iMessage;-;mailto:person@example.test")
            .unwrap(),
        remote_id: "iMessage;-;mailto:person@example.test".into(),
        kind: ConversationKind::Direct,
        title: None,
        participants: vec![Participant {
            handle: handle.clone(),
            display_name: None,
            is_me: false,
        }],
        created_at: 1,
        last_message_at: Some(10),
        aliases: Vec::new(),
        preview: None,
    };
    let engine = Conversation {
        id: ConversationId::new(transport.clone(), "iMessage;-;person@example.test").unwrap(),
        remote_id: "iMessage;-;person@example.test".into(),
        kind: ConversationKind::Direct,
        title: None,
        participants: local.participants.clone(),
        created_at: 2,
        last_message_at: Some(20),
        aliases: Vec::new(),
        preview: None,
    };
    let make_message = |conversation: &Conversation, id: &str, at: i64, text: &str| {
        Message::new(
            MessageId::new(transport.clone(), id).unwrap(),
            conversation.id.clone(),
            None,
            handle.clone(),
            vec![MessagePart::Text(text.into())],
            at,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap()
    };
    let outgoing_thread = make_message(&local, "outgoing", 10, "outgoing-only history");
    let incoming_thread = make_message(&engine, "incoming", 20, "incoming-only history");
    {
        let mut guard = store.lock().unwrap();
        store::ingest_inbound(&mut guard, &local, &outgoing_thread).unwrap();
        store::ingest_inbound(&mut guard, &engine, &incoming_thread).unwrap();
        assert_eq!(store::list_conversations(&guard).unwrap().len(), 2);
    }

    let daemon_engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: PathBuf::from("unused"),
        script: PathBuf::from("unused"),
        state_dir: private.clone(),
        library: PathBuf::from("unused"),
    }));
    let hub = SubscriptionHub::with_default_capacity();
    let service = Service::new(store.clone(), daemon_engine, hub);
    let live = make_message(&engine, "live", 30, "next message");
    assert!(service
        .persist(&TransportEvent::Message {
            conversation: engine.clone(),
            message: live,
            part_ids: Vec::new(),
            push_sequence: None,
        })
        .unwrap());

    let conversations = service.list_conversations().await.unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].id, local.id);
    assert_eq!(conversations[0].remote_id, "iMessage;-;person@example.test");
    let history = service.history(local.id.clone(), 10).await.unwrap();
    assert_eq!(history.len(), 3);
    assert!(history
        .iter()
        .all(|message| message.conversation_id() == &local.id));
    assert_eq!(
        history[0].parts(),
        &[MessagePart::Text("outgoing-only history".into())]
    );
    assert_eq!(
        history[1].parts(),
        &[MessagePart::Text("incoming-only history".into())]
    );
    assert_eq!(
        history[2].parts(),
        &[MessagePart::Text("next message".into())]
    );

    let unrelated_id = ConversationId::new(transport.clone(), "unrelated-thread").unwrap();
    let mismatched = Message::new(
        MessageId::new(transport.clone(), "mismatched-message").unwrap(),
        unrelated_id,
        None,
        handle.clone(),
        vec![MessagePart::Text("must not be ingested".into())],
        40,
        None,
        DeliveryState::Delivered,
        false,
        None,
        Vec::new(),
        None,
    )
    .unwrap();
    assert!(matches!(
        service.persist(&TransportEvent::Message {
            conversation: engine,
            message: mismatched,
            part_ids: Vec::new(),
            push_sequence: None,
        }),
        Err(store::StoreError::MessageConversationMismatch)
    ));
    assert_eq!(service.history(local.id, 10).await.unwrap().len(), 3);
}

#[tokio::test]
async fn daemon_persists_inbound_and_sends_outbound_through_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar(dir.path());
    let send_log = std::path::PathBuf::from(format!("{}.send-log", fake.display()));

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();
    let mut subscriber = hub.subscribe();

    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));

    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine.clone(), hub.clone()));
    let service_for_persist = service.clone();
    let persistence = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let _ = service_for_persist.persist(&event);
        }
    });

    supervisor.start().await.expect("engine starts");

    // The first published event is the session becoming active; then the inbound
    // message, which must be persisted before it is published.
    let (conversation_id, message) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = subscriber.recv().await.expect("channel open");
            if let EventKind::Message {
                conversation_id,
                message,
                ..
            } = event.event
            {
                return (conversation_id, message);
            }
        }
    })
    .await
    .expect("an inbound message event arrives");
    assert_eq!(conversation_id.value(), "iMessage;-;+15550000001");
    assert_eq!(message.parts(), &[MessagePart::Text("first".to_owned())]);

    {
        let guard = store.lock().unwrap();
        let stored = store::get_message(&guard, message.id())
            .expect("store readable")
            .expect("message persisted before fanout");
        assert_eq!(stored.parts(), &[MessagePart::Text("first".to_owned())]);
        let conversation = store::get_conversation(&guard, &conversation_id)
            .expect("store readable")
            .expect("conversation created by ingest");
        assert_eq!(conversation.participants.len(), 2);
        assert!(conversation
            .participants
            .iter()
            .any(|participant| participant.is_me));
    }

    // Outbound: the daemon resolves the recipient and correlates the confirmation.
    let message_id = service
        .send(
            conversation_id.clone(),
            OutgoingBody {
                parts: vec![MessagePart::Text("reply".to_owned())],
            },
        )
        .await
        .expect("send succeeds");
    assert_eq!(message_id.value(), "outbound-1");

    {
        let guard = store.lock().unwrap();
        let stored = store::get_message(&guard, &message_id)
            .expect("store readable")
            .expect("outbound message persisted");
        assert!(stored.from_me);
        assert_eq!(stored.parts(), &[MessagePart::Text("reply".to_owned())]);
    }

    // Receive the group via the real sidecar path before sending through it.
    let (group_id, group_message) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = subscriber.recv().await.expect("channel open");
            if let EventKind::Message {
                conversation_id,
                message,
                ..
            } = event.event
            {
                if conversation_id.value() == "iMessage;-;group-with-two-people" {
                    return (conversation_id, message);
                }
            }
        }
    })
    .await
    .expect("an inbound group message event arrives");
    assert_eq!(group_id.value(), "iMessage;-;group-with-two-people");
    assert_eq!(
        group_message.parts(),
        &[MessagePart::Text("group first".to_owned())]
    );

    // A group send must carry every remote participant, not just the first one.
    let group = bloom_domain::Conversation {
        id: bloom_domain::ConversationId::new(
            conversation_id.transport().clone(),
            "iMessage;-;group-with-two-people",
        )
        .unwrap(),
        remote_id: "iMessage;-;group-with-two-people".into(),
        kind: bloom_domain::ConversationKind::Group,
        title: Some("Two people".into()),
        participants: vec![
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "one@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: false,
            },
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(bloom_domain::HandleKind::Tel, "+15550000002")
                    .unwrap(),
                display_name: None,
                is_me: false,
            },
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "me@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: true,
            },
        ],
        created_at: 1,
        last_message_at: None,
        aliases: Vec::new(),
        preview: None,
    };
    assert_eq!(group.id, group_id);
    {
        let guard = store.lock().unwrap();
        let received = store::get_conversation(&guard, &group_id)
            .expect("store readable")
            .expect("inbound group conversation persisted");
        assert_eq!(received.kind, bloom_domain::ConversationKind::Group);
        assert_eq!(received.remote_id, group_id.value());
        assert_eq!(received.participants.len(), 3);
        assert!(received
            .participants
            .iter()
            .any(|p| p.handle.value() == "one@example.invalid" && !p.is_me));
        assert!(received
            .participants
            .iter()
            .any(|p| p.handle.value() == "+15550000002" && !p.is_me));
        assert!(received
            .participants
            .iter()
            .any(|p| p.handle.value() == "me@example.invalid" && p.is_me));
        let history = store::history(&guard, &group_id, 10).expect("history readable");
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].parts(),
            &[MessagePart::Text("group first".to_owned())]
        );
        assert_eq!(history[0].conversation_id(), &group_id);
    }
    assert_eq!(
        service
            .send(
                group.id.clone(),
                OutgoingBody {
                    parts: vec![MessagePart::Text("group hello".into())],
                },
            )
            .await
            .expect("group send succeeds")
            .value(),
        "outbound-2"
    );
    let frames = fs::read_to_string(send_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        frames[0]["recipients"],
        serde_json::json!(["tel:+15550000001"])
    );
    assert_eq!(frames[0]["recipient"], "tel:+15550000001");
    assert_eq!(frames[0]["chat_guid"], "iMessage;-;+15550000001");
    assert_eq!(
        frames[1]["recipients"],
        serde_json::json!(["mailto:one@example.invalid", "tel:+15550000002"])
    );
    assert_eq!(frames[1]["chat_guid"], "iMessage;-;group-with-two-people");
    assert!(frames[1].get("recipient").is_none());
    let group_history = store::history(&store.lock().unwrap(), &group.id, 10).unwrap();
    assert_eq!(group_history.len(), 2);
    assert_eq!(
        group_history[0].parts(),
        &[MessagePart::Text("group first".to_owned())]
    );
    assert_eq!(
        group_history[1].parts(),
        &[MessagePart::Text("group hello".into())]
    );
    assert_eq!(group_history[0].conversation_id(), &group.id);
    assert_eq!(group_history[1].conversation_id(), &group.id);

    // The listing carries a preview of the newest stored message for the sidebar.
    let listed = service
        .list_conversations()
        .await
        .expect("listing succeeds")
        .into_iter()
        .find(|conversation| conversation.id == conversation_id)
        .expect("conversation listed");
    assert_eq!(listed.preview.as_deref(), Some("You: reply"));

    // A duplicate delivery must not create a second row.
    let outcome = {
        let mut guard = store.lock().unwrap();
        store::upsert_message(&mut guard, &message).expect("duplicate upsert succeeds")
    };
    assert_eq!(
        outcome,
        store::MessageUpsertOutcome::Unchanged,
        "re-delivered messages must be idempotent"
    );

    let report = supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    assert!(!report.drain_incomplete);
    persistence.abort();

    // History is served from Bloom's own store, not from the engine.
    let guard = store.lock().unwrap();
    let history = store::history(&guard, &conversation_id, 10).expect("history readable");
    assert_eq!(history.len(), 2);
}

/// Push an engine-GUID inbound message before confirming the first send. This
/// recreates the identity race a user can hit when starting a fresh direct chat.
fn fake_sidecar_identity_race(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_identity_race.sh");
    let script = r#"#!/bin/sh
cat <<'FRAMES'
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":false}}
FRAMES
sent=0
while IFS= read -r line; do
  case "$line" in
    *'"type":"send"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"request_id":\([0-9]*\).*/\1/p')
      if [ "$sent" -eq 0 ]; then
        printf '%s\n' '{"version":1,"type":"inbound_message","sequence":1,"payload":{"variant":"iMessage","complete":true,"inst":{"id":"inbound-race","sender":"mailto:someone@example.com","conversation":{"participants":["mailto:someone@example.com","mailto:me@example.invalid"],"cv_name":null,"sender_guid":"iMessage;-;someone@example.com","after_guid":null},"message":{"type":"normal","complete":true,"value":{"parts":[{"idx":0,"part":{"type":"text","text":"echo"}}],"service":{"type":"iMessage","complete":true}}},"sent_timestamp_ms":1700000000000,"send_delivered":false,"verification_failed":false}}}'
        sent=1
        sleep 0.2
      fi
      printf '{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"outbound-%s","accepted":true}\n' "$id" "$id"
      ;;
    *'"type":"shutdown"'*)
      printf '{"version":1,"type":"shutdown_ack","request_id":1}\n'
      exit 0
      ;;
  esac
done
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn starting_a_conversation_creates_then_reuses_it_and_normalises_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar_identity_race(dir.path());

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();

    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));

    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine.clone(), hub.clone()));
    let persistence = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let _ = service.persist(&event);
            }
        })
    };
    supervisor.start().await.expect("engine starts");

    // Wait for readiness so the engine has reported this account's own handles.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while engine.my_handles().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !engine.my_handles().is_empty(),
        "engine must report our handles"
    );

    let body = || OutgoingBody {
        parts: vec![MessagePart::Text("hello".to_owned())],
    };
    let foreign = bloom_domain::Conversation {
        id: bloom_domain::ConversationId::new(
            bloom_domain::TransportId::new("other-transport").unwrap(),
            "someone@example.com-thread",
        )
        .unwrap(),
        remote_id: "foreign-route".into(),
        kind: bloom_domain::ConversationKind::Direct,
        title: None,
        participants: vec![bloom_domain::Participant {
            handle: bloom_domain::Handle::new(
                bloom_domain::HandleKind::Email,
                "someone@example.com",
            )
            .unwrap(),
            display_name: None,
            is_me: false,
        }],
        created_at: 1,
        last_message_at: None,
        aliases: Vec::new(),
        preview: None,
    };
    store::upsert_conversation(&mut store.lock().unwrap(), &foreign).unwrap();

    // A bare email address starts a conversation.
    let first = service
        .start_conversation("someone@example.com".to_owned(), body())
        .await
        .expect("first send succeeds");
    assert_eq!(first.value(), "outbound-1");

    let conversations = {
        let guard = store.lock().unwrap();
        store::list_conversations(&guard).expect("store readable")
    };
    assert_eq!(
        conversations.len(),
        2,
        "the other transport's thread stays separate"
    );
    let engine_thread = conversations
        .iter()
        .find(|conversation| conversation.id.transport().as_str() == "openbubbles")
        .expect("the direct thread is stored on the engine transport");
    assert_eq!(engine_thread.participants.len(), 2);
    assert_eq!(engine_thread.remote_id, "iMessage;-;someone@example.com");
    assert_eq!(engine_thread.aliases.len(), 1);
    assert!(
        engine_thread
            .participants
            .iter()
            .any(|participant| participant.is_me),
        "the local participant must be recorded so the UI can label our side"
    );
    let conversation_id = engine_thread.id.clone();

    // A duplicate message must not be created by re-sending the same address.
    let second = service
        .start_conversation("mailto:someone@example.com".to_owned(), body())
        .await
        .expect("second send succeeds");
    assert_eq!(second.value(), "outbound-2");
    {
        let guard = store.lock().unwrap();
        assert_eq!(
            store::list_conversations(&guard).unwrap().len(),
            2,
            "re-sending to the same address reuses the engine conversation"
        );
        assert_eq!(
            store::history(&guard, &conversation_id, 50).unwrap().len(),
            3,
            "the inbound race and both outbound messages are stored in one thread"
        );
    }

    // A phone number normalises and creates a separate conversation.
    service
        .start_conversation("+1 (555) 000-0009".to_owned(), body())
        .await
        .expect("phone send succeeds");
    {
        let guard = store.lock().unwrap();
        let conversations = store::list_conversations(&guard).unwrap();
        assert_eq!(conversations.len(), 3);
        let phone = conversations
            .iter()
            .find(|conversation| {
                conversation
                    .participants
                    .iter()
                    .any(|participant| participant.handle.value() == "+15550000009")
            })
            .expect("the phone conversation exists");
        assert!(
            phone.participants.iter().any(|p| !p.is_me
                && p.handle.kind() == &bloom_domain::HandleKind::Tel
                && p.handle.value() == "+15550000009"),
            "the phone number is stored normalised"
        );
    }

    // A group containing the requested recipient must not capture a direct message.
    // The recipient is one we have never messaged directly, so a reuse check that
    // matched any participant would send this into the group instead of a new thread.
    let group = bloom_domain::Conversation {
        id: bloom_domain::ConversationId::new(
            bloom_domain::TransportId::new("iMessage").unwrap(),
            "group-fixture",
        )
        .unwrap(),
        remote_id: "group-fixture".to_owned(),
        kind: bloom_domain::ConversationKind::Group,
        title: Some("Synthetic group".to_owned()),
        participants: vec![
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "groupmember@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: false,
            },
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "other@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: false,
            },
        ],
        created_at: 1,
        last_message_at: None,
        aliases: Vec::new(),
        preview: None,
    };
    store::upsert_conversation(&mut store.lock().unwrap(), &group).unwrap();
    let before = store::list_conversations(&store.lock().unwrap())
        .unwrap()
        .len();
    service
        .start_conversation("groupmember@example.invalid".to_owned(), body())
        .await
        .expect("sending to a group member succeeds");
    {
        let guard = store.lock().unwrap();
        let conversations = store::list_conversations(&guard).unwrap();
        assert_eq!(
            conversations.len(),
            before + 1,
            "a new direct conversation is created rather than reusing the group"
        );
        let direct = conversations
            .iter()
            .find(|c| {
                c.id != group.id
                    && c.kind == bloom_domain::ConversationKind::Direct
                    && c.participants
                        .iter()
                        .any(|p| !p.is_me && p.handle.value() == "groupmember@example.invalid")
            })
            .expect("the new direct conversation exists");
        assert_eq!(direct.kind, bloom_domain::ConversationKind::Direct);
        assert_eq!(
            store::history(&guard, &group.id, 50).unwrap().len(),
            0,
            "the message must not be recorded in the group"
        );
    }

    let report = supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    assert!(!report.drain_incomplete);
    persistence.abort();
}

/// A sidecar that reports readiness and then lists conversations, ending with a
/// completion frame, which is the shape the engine's chat sync produces.
fn fake_sidecar_listing(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_listing.sh");
    let script = r#"#!/bin/sh
cat <<'FRAMES'
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":true}}
{"version":1,"type":"conversation","sequence":1,"conversation":{"guid":"iMessage;-;a@example.invalid","title":"First","participants":["mailto:a@example.invalid","mailto:me@example.invalid"],"is_group":false}}
{"version":1,"type":"conversation","sequence":2,"conversation":{"guid":"iMessage;-;b@example.invalid","title":null,"participants":["mailto:b@example.invalid","mailto:me@example.invalid"],"is_group":false}}
{"version":1,"type":"conversation","sequence":3,"conversation":{"guid":"iMessage;-;a@example.invalid","title":"First","participants":["mailto:a@example.invalid","mailto:me@example.invalid"],"is_group":false}}
{"version":1,"type":"conversation","sequence":4,"conversation":{"guid":"   ","participants":[]}}
{"version":1,"type":"history_complete","conversations":3,"messages":0,"limited":false}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"shutdown"'*) printf '{"version":1,"type":"shutdown_ack","request_id":1}\n'; exit 0 ;;
  esac
done
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn fake_sidecar_sync(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_sync.sh");
    let script = r#"#!/bin/sh
cat <<'FRAMES'
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":false}}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"sync_conversations"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"request_id":\([0-9]*\).*/\1/p')
      printf '{"version":1,"type":"conversation","sequence":1,"conversation":{"guid":"iMessage;-;sync@example.invalid","title":"Synced","participants":["mailto:sync@example.invalid","mailto:me@example.invalid"],"is_group":false}}\n'
      printf '{"version":1,"type":"sync_result","request_id":%s,"status":"complete","conversations":1}\n' "$id"
      ;;
    *'"type":"shutdown"'*)
      printf '{"version":1,"type":"shutdown_ack","request_id":1}\n'
      exit 0
      ;;
  esac
done
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn sync_waits_for_persistence_and_publishes_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar_sync(dir.path());

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();
    let mut subscriber = hub.subscribe();
    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));
    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine, hub));
    let service_for_persist = service.clone();
    let persistence = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            service_for_persist.persist(&event).unwrap();
        }
    });
    supervisor.start().await.expect("engine starts");

    assert_eq!(
        service.sync_conversations().await.unwrap(),
        (bloom_ipc::ConversationSyncStatus::Complete, 1)
    );
    assert_eq!(
        store::list_conversations(&store.lock().unwrap())
            .unwrap()
            .len(),
        1,
        "sync response must wait until the listing row is committed"
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
            .await
            .expect("sync publishes a refresh")
            .unwrap();
        if matches!(event.event, EventKind::ConversationsChanged) {
            break;
        }
    }

    let report = supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    assert!(!report.drain_incomplete);
    persistence.abort();
}

#[tokio::test]
async fn listing_frames_upsert_conversations_and_publish_one_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar_listing(dir.path());

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();
    let mut subscriber = hub.subscribe();

    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));

    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine.clone(), hub.clone()));
    let persistence = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let _ = service.persist(&event);
            }
        })
    };
    supervisor.start().await.expect("engine starts");

    // The first published event is the session becoming active.
    let mut refreshes = 0usize;
    let mut saw_conversations_changed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !saw_conversations_changed {
        match tokio::time::timeout(Duration::from_secs(2), subscriber.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.event, EventKind::ConversationsChanged) {
                    refreshes += 1;
                    saw_conversations_changed = true;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }
    assert!(
        saw_conversations_changed,
        "finishing a listing must tell clients to refresh"
    );

    let conversations = {
        let guard = store.lock().unwrap();
        store::list_conversations(&guard).expect("store readable")
    };
    assert_eq!(
        conversations.len(),
        2,
        "a repeated guid must not create a duplicate, and an unusable frame is skipped"
    );
    assert!(conversations
        .iter()
        .any(|conversation| conversation.title.as_deref() == Some("First")));
    assert!(conversations
        .iter()
        .any(|conversation| conversation.title.is_none()));
    for conversation in &conversations {
        assert!(
            conversation.participants.iter().any(|p| p.is_me),
            "our own participant must be marked"
        );
    }

    // No further refresh should arrive for the same listing.
    let extra = tokio::time::timeout(Duration::from_millis(400), subscriber.recv()).await;
    assert!(
        extra.is_err()
            || !matches!(
                extra.unwrap().map(|e| e.event),
                Ok(EventKind::ConversationsChanged)
            ),
        "a listing must publish exactly one refresh"
    );
    assert_eq!(refreshes, 1);

    let report = supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    assert!(!report.drain_incomplete);
    persistence.abort();
}

/// A fake sidecar that records every frame the daemon writes to its stdin. The
/// engine form of the recipient is a contract the real sidecar depends on: a
/// double-prefixed handle (`mailto:mailto:...`) resolves to no IDS target and
/// fails every send.
fn fake_sidecar_recording(dir: &std::path::Path, log: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_recording.sh");
    let script = format!(
        r#"#!/bin/sh
cat <<'FRAMES'
{{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{{"send":true,"history":false}}}}
FRAMES
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{log}'
  case "$line" in
    *'"type":"send"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"request_id":\([0-9]*\).*/\1/p')
      printf '{{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"outbound-%s","accepted":true}}\n' "$id" "$id"
      ;;
    *'"type":"shutdown"'*)
      printf '{{"version":1,"type":"shutdown_ack","request_id":1}}\n'
      exit 0
      ;;
  esac
done
"#,
        log = log.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// Holds the first synthetic send until the test observes the fixed timeout,
/// then emits its late confirmation before immediately confirming a second send.
fn fake_sidecar_delayed_confirmation(dir: &std::path::Path, marker: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_delayed.sh");
    let script = format!(
        r#"#!/bin/sh
cat <<'FRAMES'
{{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{{"send":true,"history":false}}}}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"send"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"request_id":\([0-9]*\).*/\1/p')
      if [ ! -e '{marker}.received' ]; then
        printf 'received\n' > '{marker}.received'
        while [ ! -e '{marker}.release' ]; do sleep 0.05; done
        printf '{{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"late-outbound","accepted":true}}\n' "$id"
        printf 'confirmed\n' > '{marker}.confirmed'
      else
        printf '{{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"second-outbound","accepted":true}}\n' "$id"
      fi
      ;;
    *'"type":"shutdown"'*)
      printf '{{"version":1,"type":"shutdown_ack","request_id":1}}\n'
      exit 0
      ;;
  esac
done
"#,
        marker = marker.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn late_confirmation_after_send_timeout_is_not_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let marker = dir.path().join("send-marker");
    let fake = fake_sidecar_delayed_confirmation(dir.path(), &marker);

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();
    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));
    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine.clone(), hub));
    let persistence = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let _ = service.persist(&event);
            }
        })
    };
    supervisor.start().await.expect("engine starts");

    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while engine.my_handles().is_empty() && tokio::time::Instant::now() < ready_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !engine.my_handles().is_empty(),
        "sidecar readiness observed"
    );

    let conversation_id =
        bloom_domain::ConversationId::new(bloomd::engine::transport_id(), "synthetic-chat")
            .unwrap();
    let conversation = bloom_domain::Conversation {
        id: conversation_id.clone(),
        remote_id: "synthetic-chat".to_owned(),
        kind: bloom_domain::ConversationKind::Direct,
        title: None,
        participants: vec![
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "person@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: false,
            },
            bloom_domain::Participant {
                handle: bloom_domain::Handle::new(
                    bloom_domain::HandleKind::Email,
                    "me@example.invalid",
                )
                .unwrap(),
                display_name: None,
                is_me: true,
            },
        ],
        created_at: 1,
        last_message_at: None,
        aliases: Vec::new(),
        preview: None,
    };
    store::upsert_conversation(&mut store.lock().unwrap(), &conversation).unwrap();

    let send_service = service.clone();
    let send_started = std::time::Instant::now();
    let send = tokio::spawn(async move {
        send_service
            .send(
                conversation_id,
                OutgoingBody {
                    parts: vec![MessagePart::Text(
                        "synthetic timeout reproduction".to_owned(),
                    )],
                },
            )
            .await
    });
    let received = tokio::time::Instant::now() + Duration::from_secs(5);
    while !marker.with_extension("received").exists() && tokio::time::Instant::now() < received {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        marker.with_extension("received").exists(),
        "fake sidecar received send"
    );
    let result = tokio::time::timeout(Duration::from_secs(35), send)
        .await
        .expect("send returns within timeout bound")
        .expect("send task completes");
    assert_eq!(result, Err(bloom_ipc::ProtocolError::Protocol));
    assert!(
        send_started.elapsed() >= Duration::from_secs(30),
        "waited for the configured send timeout"
    );
    fs::write(marker.with_extension("release"), "release").unwrap();
    assert!(
        store::history(&store.lock().unwrap(), &conversation.id, 10)
            .unwrap()
            .is_empty(),
        "failed send was not persisted"
    );

    let confirmed = tokio::time::Instant::now() + Duration::from_secs(5);
    while !marker.with_extension("confirmed").exists() && tokio::time::Instant::now() < confirmed {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        marker.with_extension("confirmed").exists(),
        "valid late confirmation emitted"
    );

    // A subsequent valid response proves the reader consumed the late frame and
    // remained synchronized, rather than closing on malformed protocol output.
    let second = service
        .send(
            conversation.id.clone(),
            OutgoingBody {
                parts: vec![MessagePart::Text("second synthetic send".to_owned())],
            },
        )
        .await
        .expect("second send succeeds after late confirmation");
    assert_eq!(second.value(), "second-outbound");
    let history = store::history(&store.lock().unwrap(), &conversation.id, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id().value(), "second-outbound");

    supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    persistence.abort();
}

#[tokio::test]
async fn the_daemon_sends_engine_form_recipients_exactly_once_prefixed() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let log = dir.path().join("frames.log");
    let fake = fake_sidecar_recording(dir.path(), &log);

    let store = EncryptedStore::open(private.join("store.db"), &TestSecrets).unwrap();
    let store = Arc::new(Mutex::new(store));
    let hub = SubscriptionHub::with_default_capacity();

    let engine = Arc::new(Engine::new(EngineConfig {
        sync_chats: false,
        dart_executable: fake,
        script: PathBuf::from("ignored"),
        state_dir: private.clone(),
        library: PathBuf::from("ignored"),
    }));

    let (sink, mut events) = mpsc::channel(64);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    let service = Arc::new(Service::new(store.clone(), engine.clone(), hub.clone()));
    let persistence = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let _ = service.persist(&event);
            }
        })
    };
    supervisor.start().await.expect("engine starts");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while engine.my_handles().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Start a conversation, then reply to it. Both paths must hand the sidecar a
    // single-prefix handle.
    service
        .start_conversation(
            "person@example.test".to_owned(),
            OutgoingBody {
                parts: vec![MessagePart::Text("first".to_owned())],
            },
        )
        .await
        .expect("start conversation sends");

    let conversation_id = {
        let guard = store.lock().unwrap();
        store::list_conversations(&guard)
            .expect("store readable")
            .into_iter()
            .find(|conversation| {
                conversation
                    .participants
                    .iter()
                    .any(|participant| participant.handle.value() == "person@example.test")
            })
            .expect("conversation created")
            .id
    };
    service
        .send(
            conversation_id,
            OutgoingBody {
                parts: vec![MessagePart::Text("reply".to_owned())],
            },
        )
        .await
        .expect("reply sends");

    supervisor
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown completes");
    persistence.abort();

    let frames = fs::read_to_string(&log).expect("frames recorded");
    let sends: Vec<&str> = frames
        .lines()
        .filter(|line| line.contains("\"type\":\"send\""))
        .collect();
    assert_eq!(sends.len(), 2, "both sends reached the sidecar: {frames}");
    for frame in sends {
        assert!(
            frame.contains("\"recipients\":[\"mailto:person@example.test\"]"),
            "recipients must be engine form with one prefix: {frame}"
        );
        assert!(
            !frame.contains("mailto:mailto:"),
            "recipients must not be double prefixed: {frame}"
        );
    }
}
