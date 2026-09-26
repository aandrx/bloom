use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    room::MessagesOptions,
    ruma::{events::room::message::RoomMessageEventContent, OwnedEventId, OwnedUserId},
    Client, RoomState,
};
use serde_json;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{env, fs::OpenOptions, io::Write, path::PathBuf, time::Duration};
use tokio::time::timeout;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data");
    let vars = std::fs::read_to_string(root.join("accounts.env"))?;
    let get = |key: &str| {
        vars.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_owned))
            .ok_or("account data missing")
    };
    let (au, ap, bu, bp) = (
        get("ALICE_USER")?,
        get("ALICE_PASSWORD")?,
        get("BOB_USER")?,
        get("BOB_PASSWORD")?,
    );
    let alice_id = matrix_sdk::ruma::UserId::parse(format!("@{au}:localhost"))?;
    let bob_id = matrix_sdk::ruma::UserId::parse(format!("@{bu}:localhost"))?;
    let builder = |db: PathBuf| {
        Client::builder()
            .homeserver_url("http://127.0.0.1:8008")
            .sqlite_store(db, None)
    };
    let alice = builder(root.join("alice.db")).build().await?;
    alice
        .matrix_auth()
        .login_username(&au, &ap)
        .initial_device_display_name("Bloom proof Alice")
        .send()
        .await?;
    let bob = builder(root.join("bob.db")).build().await?;
    let login = bob
        .matrix_auth()
        .login_username(&bu, &bp)
        .initial_device_display_name("Bloom proof Bob")
        .send()
        .await?;
    write_private_file(
        &root.join("bob-session.json"),
        &serde_json::to_vec(&MatrixSession::from(&login))?,
    )?;
    sync(&alice, None).await?;
    let mut bob_next_batch = sync(&bob, None).await?.next_batch;

    let mut request = matrix_sdk::ruma::api::client::room::create_room::v3::Request::new();
    let mut creation_content =
        matrix_sdk::ruma::api::client::room::create_room::v3::CreationContent::new();
    creation_content.federate = false;
    request.creation_content = Some(matrix_sdk::ruma::serde::Raw::new(&creation_content)?);
    request.preset =
        Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PrivateChat);
    request.visibility = matrix_sdk::ruma::api::client::room::Visibility::Private;
    let room = alice.create_room(request).await?;
    room.enable_encryption().await?;
    sync(&alice, None).await?;
    let rid = room.room_id().to_owned();
    if bob.join_room_by_id(&rid).await.is_ok() {
        return Err("Bob joined private room before invitation".into());
    }
    println!("PASS: Bob denied joining private room before invitation");
    if alice.user_id() != Some(&alice_id) || bob.user_id() != Some(&bob_id) {
        return Err("authenticated Matrix user ID did not match expected local ID".into());
    }
    room.invite_user_by_id(&bob_id).await?;
    let invite_response = sync(&bob, Some(bob_next_batch.clone())).await?;
    bob_next_batch = invite_response.next_batch.clone();
    if !invite_response.rooms.invited.contains_key(&rid) {
        let received: Vec<_> = invite_response
            .rooms
            .invited
            .keys()
            .map(ToString::to_string)
            .collect();
        return Err(format!(
            "Bob sync invite response missing {rid}; received room IDs: {received:?}"
        )
        .into());
    }
    let invited = bob
        .get_room(&rid)
        .ok_or("sync contained invite room ID but SDK room cache did not contain it")?;
    if invited.state() != RoomState::Invited {
        return Err("room was not invite-only for Bob".into());
    }
    println!("PASS: Alice invited Bob and invite was received");
    invited.join().await?;
    bob_next_batch = sync(&bob, Some(bob_next_batch.clone())).await?.next_batch;
    let marker = format!("bloom-proof-first-{}", std::process::id());
    let sent = room
        .send(RoomMessageEventContent::text_plain(marker.clone()))
        .await?;
    let first_id = sent.response.event_id;
    let raw_request = matrix_sdk::ruma::api::client::room::get_room_event::v3::Request::new(
        rid.clone(),
        first_id.clone(),
    );
    let raw_response = alice.send(raw_request).await?;
    let raw_event = raw_response.event.deserialize_as::<serde_json::Value>()?;
    let raw_json = raw_event.to_string();
    if raw_event.get("type").and_then(serde_json::Value::as_str) != Some("m.room.encrypted")
        || raw_json.contains(&marker)
        || raw_event
            .get("content")
            .and_then(|content| content.get("ciphertext"))
            .is_none()
    {
        return Err("homeserver event was not opaque encrypted ciphertext".into());
    }
    println!("PASS: homeserver returned opaque encrypted event without plaintext marker");
    if !receive_sync_event(&bob, &mut bob_next_batch, &rid, &au, &first_id, &marker).await? {
        return Err("Bob sync did not contain exact first event".into());
    }
    println!("PASS: Bob joined and exact event ID, sender, and body arrived via sync");
    write_private_file(&root.join("bob-sync-token"), bob_next_batch.as_bytes())?;
    drop(bob);
    let second = format!("bloom-proof-offline-{}", std::process::id());
    let second_id = room
        .send(RoomMessageEventContent::text_plain(second.clone()))
        .await?
        .response
        .event_id;
    let reopened = builder(root.join("bob.db")).build().await?;
    let session: MatrixSession =
        serde_json::from_slice(&std::fs::read(root.join("bob-session.json"))?)?;
    reopened
        .matrix_auth()
        .restore_session(session, Default::default())
        .await?;
    let mut bob_next_batch = std::fs::read_to_string(root.join("bob-sync-token"))?;
    if !receive_sync_event(
        &reopened,
        &mut bob_next_batch,
        &rid,
        &au,
        &second_id,
        &second,
    )
    .await?
    {
        return Err("Bob sync did not contain exact offline event".into());
    }
    let joined = reopened
        .get_room(&rid)
        .ok_or("room missing after restore")?;
    let history = joined.messages(MessagesOptions::backward()).await?;
    if !history
        .chunk
        .iter()
        .any(|e| e.event_id() == Some(second_id.as_ref()))
    {
        return Err("offline event missing from persisted history".into());
    }
    println!("PASS: restored session caught offline event and history persisted");
    let bad = builder(root.join("invalid.db")).build().await?;
    if bad
        .matrix_auth()
        .login_username(&au, "invalid-password")
        .send()
        .await
        .is_ok()
    {
        return Err("invalid credentials accepted".into());
    }
    println!("PASS: invalid credentials rejected");
    Ok(())
}

fn write_private_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)
}

async fn sync(
    client: &Client,
    token: Option<String>,
) -> Result<matrix_sdk::sync::SyncResponse, Box<dyn std::error::Error>> {
    let mut settings = SyncSettings::default().timeout(Duration::from_secs(5));
    if let Some(token) = token {
        settings = settings.token(token);
    }
    Ok(timeout(Duration::from_secs(10), client.sync_once(settings)).await??)
}
async fn receive_sync_event(
    client: &Client,
    next_batch: &mut String,
    room: &matrix_sdk::ruma::OwnedRoomId,
    sender: &str,
    id: &OwnedEventId,
    body: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let sender: OwnedUserId = matrix_sdk::ruma::UserId::parse(format!("@{sender}:localhost"))?;
    for _ in 0..30 {
        let response = sync(client, Some(next_batch.clone())).await?;
        *next_batch = response.next_batch.clone();
        if let Some(events) = response.rooms.joined.get(room) {
            for raw in &events.timeline.events {
                let event_id = raw.event_id();
                let raw_event = raw.raw();
                let event_sender: Option<OwnedUserId> = raw_event.get_field("sender")?;
                let content: Option<serde_json::Value> = raw_event.get_field("content")?;
                if event_id.map(ToString::to_string).as_deref() == Some(id.as_str())
                    && event_sender.as_ref() == Some(&sender)
                    && content
                        .as_ref()
                        .and_then(|value| value.get("body"))
                        .and_then(serde_json::Value::as_str)
                        == Some(body)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
