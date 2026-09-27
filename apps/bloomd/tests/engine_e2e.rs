//! End-to-end test against the real engine sidecar and the real daemon binary.
//!
//! Ignored by default because it needs the installed engine library and the user's
//! registration state. Run it explicitly:
//!
//! ```text
//! BLOOM_E2E_ENGINE_STATE=<state dir> \
//! BLOOM_E2E_ENGINE_LIBRARY=<engine .so> \
//! BLOOM_E2E_SCRIPT=<sidecar script> \
//! cargo test -p bloomd --test engine_e2e -- --ignored --nocapture
//! ```
//!
//! Optional: `BLOOM_E2E_SEND_TO` sends one message, `BLOOM_E2E_SECONDS` bounds the
//! inbound wait (default 25), `BLOOM_E2E_REQUIRE_INBOUND=1` fails when nothing
//! arrives, `BLOOM_E2E_DART` overrides the Dart executable.
//!
//! The test never prints message content or handles.

use bloom_ipc::{client::BloomClient, EventKind};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for this test"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn wait_for_socket(path: &Path) -> bool {
    for _ in 0..120 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

#[tokio::test]
#[ignore = "requires the installed engine library and the user's state directory"]
async fn daemon_end_to_end_with_the_real_engine() {
    let state = required("BLOOM_E2E_ENGINE_STATE");
    let library = required("BLOOM_E2E_ENGINE_LIBRARY");
    let script = required("BLOOM_E2E_SCRIPT");
    let seconds: u64 = optional("BLOOM_E2E_SECONDS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(25);

    // A private sandbox keeps this test away from the user's Bloom store.
    let sandbox = tempfile::tempdir().expect("temp dir");
    let private: PathBuf = sandbox.path().join("private");
    fs::create_dir(&private).expect("private dir");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("restrict dir");
    let socket = private.join("bloomd.sock");
    let store = private.join("store.db");

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bloomd"));
    command
        .env("BLOOM_SOCKET_PATH", &socket)
        .env("BLOOM_STORE_PATH", &store)
        .env("BLOOM_ENGINE_STATE", state)
        .env("BLOOM_ENGINE_LIBRARY", library)
        .env("BLOOM_ENGINE_SCRIPT", script)
        .env(
            "BLOOM_DART_EXECUTABLE",
            optional("BLOOM_E2E_DART").unwrap_or_else(|| "dart".to_owned()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut daemon = command.spawn().expect("daemon starts");

    assert!(wait_for_socket(&socket), "daemon never created its socket");

    let client = BloomClient::new(&socket);
    let conversations = client
        .list_conversations()
        .await
        .expect("daemon answers list_conversations");
    println!("conversations visible: {}", conversations.len());

    let mut stream = client.subscribe().await.expect("subscription accepted");

    let mut inbound = 0usize;
    let mut session_active = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            Ok(Ok(event)) => match event.event {
                EventKind::Message {
                    conversation_id, ..
                } => {
                    inbound += 1;
                    // Reading history proves the event was persisted, not just fanned out.
                    let history = client
                        .history(conversation_id, 10)
                        .await
                        .expect("history readable for a received message");
                    assert!(
                        !history.is_empty(),
                        "an event must be persisted before fanout"
                    );
                }
                EventKind::Session { .. } => session_active = true,
                _ => {}
            },
            Ok(Err(error)) => panic!("event stream failed: {error}"),
            Err(_) => {}
        }
        if inbound > 0 && session_active {
            break;
        }
    }

    if let Some(recipient) = optional("BLOOM_E2E_SEND_TO") {
        let target = conversations
            .iter()
            .find(|conversation| {
                conversation.participants.iter().any(|participant| {
                    !participant.is_me && participant.handle.value() == recipient
                })
            })
            .map(|conversation| conversation.id.clone())
            .unwrap_or_else(|| panic!("no stored conversation matches the requested recipient"));
        let body = bloom_domain::OutgoingBody {
            parts: vec![bloom_domain::MessagePart::Text(
                "bloom end to end test".to_owned(),
            )],
        };
        let message_id = client.send(target, body).await.expect("send accepted");
        println!("sent message id length: {}", message_id.value().len());
    }

    println!("session observed active: {session_active}");
    println!("inbound messages observed: {inbound}");
    assert!(
        session_active,
        "the engine never reported an active session"
    );
    if optional("BLOOM_E2E_REQUIRE_INBOUND").is_some() {
        assert!(inbound > 0, "no inbound message arrived within the window");
    }

    let _ = daemon.start_kill();
    let _ = daemon.wait().await;
}
