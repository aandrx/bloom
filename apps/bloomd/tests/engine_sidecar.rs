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

    for invalid in ["", "   ", "not-an-address", "mailto:@bad"] {
        let result = service.start_conversation(invalid.to_owned(), body()).await;
        assert!(
            matches!(result, Err(bloom_ipc::ProtocolError::InvalidRequest)),
            "address {invalid:?} must be rejected"
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
