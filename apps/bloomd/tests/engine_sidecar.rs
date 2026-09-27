//! Daemon integration test driven by a fake engine sidecar.
//!
//! The fake sidecar speaks the real stdio protocol, so this exercises the actual
//! child-process path: spawn, readiness, inbound event, persistence, fanout, send
//! correlation and shutdown. It does not need the engine library or the network.

use bloom_domain::{MessagePart, OutgoingBody};
use bloom_ipc::EventKind;
use bloom_secrets::SecretStoreError;
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
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":false}}
{"version":1,"type":"inbound_message","sequence":1,"payload":{"variant":"iMessage","complete":true,"inst":{"id":"inbound-1","sender":"tel:+15550000001","conversation":{"participants":["tel:+15550000001","mailto:me@example.invalid"],"cv_name":null,"sender_guid":"iMessage;-;+15550000001","after_guid":null},"message":{"type":"normal","complete":true,"value":{"parts":[{"idx":0,"part":{"type":"text","text":"first"}}],"service":{"type":"iMessage","complete":true}}},"sent_timestamp_ms":1700000000000,"send_delivered":false,"verification_failed":false}}}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"send"'*)
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
async fn daemon_persists_inbound_and_sends_outbound_through_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar(dir.path());

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

/// Like `fake_sidecar`, but reports readiness without pushing an inbound message,
/// so outbound tests are not polluted by an inbound conversation.
fn fake_sidecar_quiet(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake_engine_quiet.sh");
    let script = r#"#!/bin/sh
cat <<'FRAMES'
{"version":1,"type":"ready","engine_version":"fake","handles":["mailto:me@example.invalid"],"capabilities":{"send":true,"history":false}}
FRAMES
while IFS= read -r line; do
  case "$line" in
    *'"type":"send"'*)
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
async fn starting_a_conversation_creates_then_reuses_it_and_normalises_addresses() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let fake = fake_sidecar_quiet(dir.path());

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
        1,
        "exactly one conversation is created"
    );
    assert_eq!(conversations[0].participants.len(), 2);
    assert!(
        conversations[0].participants.iter().any(|p| p.is_me),
        "the local participant must be recorded so the UI can label our side"
    );
    let conversation_id = conversations[0].id.clone();

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
            1,
            "re-sending to the same address reuses the conversation"
        );
        assert_eq!(
            store::history(&guard, &conversation_id, 50).unwrap().len(),
            2,
            "both outbound messages are stored"
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
        assert_eq!(conversations.len(), 2);
        let phone = conversations
            .iter()
            .find(|c| c.id != conversation_id)
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
            frame.contains("\"recipient\":\"mailto:person@example.test\""),
            "recipient must be engine form with one prefix: {frame}"
        );
        assert!(
            !frame.contains("mailto:mailto:"),
            "recipient must not be double prefixed: {frame}"
        );
    }
}
