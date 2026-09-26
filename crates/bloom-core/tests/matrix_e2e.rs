use anyhow::{Context, Result};
use bloom_core::{Bloom, CoreEvent, EncryptionStatus, MembershipState, SecretStore};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::timeout;

#[derive(Default)]
struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);
impl SecretStore for FakeSecrets {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }
}

fn account(name: &str) -> Result<String> {
    let _ = env::var("BLOOM_E2E_ACCOUNTS")
        .context("missing BLOOM_E2E_ACCOUNTS; run scripts/test-local-e2e.sh")?;
    env::var(name).with_context(|| format!("missing {name}; run scripts/test-local-e2e.sh"))
}

async fn next_message(
    rx: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
    room: &matrix_sdk::ruma::OwnedRoomId,
    body: &str,
) -> Result<bloom_core::Message> {
    timeout(Duration::from_secs(90), async {
        loop {
            match rx.recv().await {
                Ok(CoreEvent::Message { room_id, message })
                    if &room_id == room && message.body.as_deref() == Some(body) =>
                {
                    return Ok(message)
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    })
    .await
    .context("timed out waiting for message")?
}

async fn wait_summary(
    bloom: &Bloom,
    room: &matrix_sdk::ruma::OwnedRoomId,
    state: MembershipState,
    encryption: EncryptionStatus,
) -> Result<()> {
    timeout(Duration::from_secs(90), async {
        loop {
            if bloom
                .rooms()
                .await
                .iter()
                .any(|s| s.room_id == *room && s.membership == state && s.encryption == encryption)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("timed out waiting for room summary")?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Synapse; run scripts/test-local-e2e.sh"]
async fn local_synapse_login_invite_encrypted_messages_and_offline_restore() -> Result<()> {
    let alice_user = format!("@{}:localhost", account("ALICE_USER")?);
    let alice_password = account("ALICE_PASSWORD")?;
    let bob_user = format!("@{}:localhost", account("BOB_USER")?);
    let bob_password = account("BOB_PASSWORD")?;
    let root = tempfile::tempdir()?;
    let alice_dir: PathBuf = root.path().join("alice");
    let bob_dir: PathBuf = root.path().join("bob");
    let alice_secrets = Arc::new(FakeSecrets::default());
    let bob_secrets = Arc::new(FakeSecrets::default());
    let alice = Bloom::open(
        "http://127.0.0.1:8008",
        &alice_dir,
        &alice_user,
        alice_secrets.clone(),
    )
    .await?;
    alice.login(&alice_user, &alice_password).await?;
    assert!(
        !alice_secrets
            .0
            .lock()
            .unwrap()
            .values()
            .any(|v| v == alice_password.as_bytes()),
        "Alice password persisted"
    );
    let (_alice_rx, _alice_sync) = alice.start_sync(128).await?;
    let room = alice.direct(&bob_user).await?;
    assert!(
        alice.rooms().await.iter().any(|s| s.room_id == room
            && s.membership == MembershipState::Joined
            && s.encryption == EncryptionStatus::Encrypted),
        "direct room must be private, joinable and encrypted"
    );

    let bob = Bloom::open(
        "http://127.0.0.1:8008",
        &bob_dir,
        &bob_user,
        bob_secrets.clone(),
    )
    .await?;
    bob.login(&bob_user, &bob_password).await?;
    assert!(
        !bob_secrets
            .0
            .lock()
            .unwrap()
            .values()
            .any(|v| v == bob_password.as_bytes()),
        "Bob password persisted"
    );
    let (mut bob_rx, bob_sync) = bob.start_sync(128).await?;
    wait_summary(
        &bob,
        &room,
        MembershipState::Invited,
        EncryptionStatus::Unknown,
    )
    .await?;
    let invited = bob
        .rooms()
        .await
        .into_iter()
        .find(|summary| summary.room_id == room)
        .context("invited room summary disappeared")?;
    assert_eq!(invited.encryption, EncryptionStatus::Unknown);
    // Matrix SDK 0.19.1 receives stripped encryption state on invite but cannot
    // verify full state until joining, so sending stays blocked until then.
    let prejoin = format!("bloom-prejoin-{}", uuidless_marker());
    let error = bob.send(&room, &prejoin).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("refusing to send unless room encryption is confirmed"));

    bob.join(&room).await?;
    wait_summary(
        &bob,
        &room,
        MembershipState::Joined,
        EncryptionStatus::Encrypted,
    )
    .await?;

    let first = format!("bloom-online-{}", uuidless_marker());
    let sent_event_id = alice.send(&room, &first).await?;
    let received = next_message(&mut bob_rx, &room, &first).await?;
    assert_eq!(received.sender, alice_user_id(&alice_user));
    assert_eq!(received.event_id, sent_event_id);
    bob_sync.shutdown().await?;

    let second = format!("bloom-offline-{}", uuidless_marker());
    alice.send(&room, &second).await?;
    drop(bob);
    let reopened = Bloom::open("http://127.0.0.1:8008", &bob_dir, &bob_user, bob_secrets).await?;
    assert!(reopened.restore().await?);
    let (mut restored_rx, _restored_sync) = reopened.start_sync(128).await?;
    let offline = next_message(&mut restored_rx, &room, &second).await?;
    assert_eq!(offline.sender, alice_user_id(&alice_user));
    assert!(!offline.event_id.is_empty());
    let history = timeout(Duration::from_secs(60), async {
        loop {
            let messages = reopened.history(&room, 50).await?;
            if messages.iter().any(|m| m.body.as_deref() == Some(&first))
                && messages.iter().any(|m| m.body.as_deref() == Some(&second))
            {
                return Ok::<_, anyhow::Error>(messages);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .context("timed out waiting for history")??;
    for marker in [&first, &second] {
        let message = history
            .iter()
            .find(|m| m.body.as_deref() == Some(marker.as_str()))
            .unwrap();
        assert_eq!(message.sender, alice_user_id(&alice_user));
        assert!(!message.event_id.is_empty());
    }
    Ok(())
}

fn uuidless_marker() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
fn alice_user_id(user: &str) -> String {
    user.to_owned()
}
