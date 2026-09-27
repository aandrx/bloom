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
      printf '{"version":1,"type":"send_confirmation","request_id":%s,"message_id":"outbound-1","accepted":true}\n' "$id"
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
