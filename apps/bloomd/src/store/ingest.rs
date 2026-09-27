use super::{EncryptedStore, StoreError};
use bloom_domain::{Conversation, ConversationId, DeliveryState, Message, MessageId};
use rusqlite::{params, OptionalExtension};

pub use super::messages::MessageUpsertOutcome;

fn conversation_id(id: &ConversationId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}
fn message_id(id: &MessageId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}

/// Insert or update one message row inside an existing transaction.
fn upsert_message_row(
    tx: &rusqlite::Transaction<'_>,
    message_key: &str,
    conversation_key: &str,
    created_at: i64,
    data: &[u8],
    aliases: &[String],
    conversation: &ConversationId,
) -> Result<MessageUpsertOutcome, StoreError> {
    let previous: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT conversation_id, data FROM messages WHERE id=?1",
            [message_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((owner, old_data)) = previous.as_ref() {
        if owner != conversation_key {
            if !aliases.contains(owner) {
                return Err(StoreError::MessageConversationMismatch);
            }
            let old_message: Message =
                serde_json::from_slice(old_data).map_err(|_| StoreError::Schema)?;
            let remapped = super::messages::remap_message(&old_message, conversation)?;
            let new_message: Message =
                serde_json::from_slice(data).map_err(|_| StoreError::Schema)?;
            let outcome = if remapped == new_message {
                MessageUpsertOutcome::Unchanged
            } else {
                MessageUpsertOutcome::Updated
            };
            tx.execute(
                "UPDATE messages SET conversation_id=?2, created_at=?3, data=?4 WHERE id=?1",
                params![message_key, conversation_key, created_at, data],
            )?;
            return Ok(outcome);
        }
    }
    Ok(match previous {
        None => {
            tx.execute(
                "INSERT INTO messages(id, conversation_id, created_at, data) VALUES (?1, ?2, ?3, ?4)",
                params![message_key, conversation_key, created_at, data],
            )?;
            MessageUpsertOutcome::Inserted
        }
        Some((_, old)) if old == data => MessageUpsertOutcome::Unchanged,
        Some(_) => {
            tx.execute(
                "UPDATE messages SET created_at=?2, data=?3 WHERE id=?1",
                params![message_key, created_at, data],
            )?;
            MessageUpsertOutcome::Updated
        }
    })
}

/// Recompute the conversation summary from its messages and persist it.
fn refresh_summary(
    tx: &rusqlite::Transaction<'_>,
    conversation_key: &str,
    mut conversation: Conversation,
) -> Result<(), StoreError> {
    let mut summary = None;
    let mut identities = vec![conversation.id.clone()];
    identities.extend(conversation.aliases.iter().cloned());
    for identity in identities {
        let key = conversation_id(&identity)?;
        let latest: Option<i64> = tx.query_row(
            "SELECT MAX(created_at) FROM messages WHERE conversation_id=?1",
            [key],
            |r| r.get(0),
        )?;
        summary = summary.max(latest);
    }
    conversation.last_message_at = summary;
    let encoded = serde_json::to_vec(&conversation).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "UPDATE conversations SET last_message_at=?2, data=?3 WHERE id=?1",
        params![conversation_key, summary, encoded],
    )?;
    Ok(())
}

pub fn ingest_message(
    store: &mut EncryptedStore,
    message: &Message,
) -> Result<MessageUpsertOutcome, StoreError> {
    let message_id = message_id(message.id())?;
    let conversation_key = conversation_id(message.conversation_id())?;
    let data = serde_json::to_vec(message).map_err(|_| StoreError::Schema)?;
    let tx = store._connection.transaction()?;
    let blob: Option<Vec<u8>> = tx
        .query_row(
            "SELECT data FROM conversations WHERE id=?1",
            [&conversation_key],
            |r| r.get(0),
        )
        .optional()?;
    let conversation = blob
        .map(|blob| serde_json::from_slice::<Conversation>(&blob))
        .transpose()
        .map_err(|_| StoreError::Schema)?;
    let aliases = conversation
        .as_ref()
        .map(|conversation| {
            conversation
                .aliases
                .iter()
                .map(conversation_id)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let outcome = upsert_message_row(
        &tx,
        &message_id,
        &conversation_key,
        message.created_at,
        &data,
        &aliases,
        message.conversation_id(),
    )?;
    let conversation = conversation.ok_or(StoreError::Schema)?;
    refresh_summary(&tx, &conversation_key, conversation)?;
    tx.commit()?;
    Ok(outcome)
}

/// Create or update the conversation and ingest its message in ONE transaction.
///
/// The engine re-delivers queued pushes, so this must be idempotent: an unchanged
/// message reports `Unchanged` and must not create a duplicate row or a second
/// conversation. Returns whether the conversation row was created.
pub fn ingest_inbound(
    store: &mut EncryptedStore,
    conversation: &Conversation,
    message: &Message,
) -> Result<(MessageUpsertOutcome, bool), StoreError> {
    if conversation.id != *message.conversation_id() {
        return Err(StoreError::MessageConversationMismatch);
    }
    let conversation_key = conversation_id(&conversation.id)?;
    let message_key = message_id(message.id())?;
    let data = serde_json::to_vec(message).map_err(|_| StoreError::Schema)?;
    let tx = store._connection.transaction()?;

    let existing_blob: Option<Vec<u8>> = tx
        .query_row(
            "SELECT data FROM conversations WHERE id=?1",
            [&conversation_key],
            |r| r.get(0),
        )
        .optional()?;
    let created = existing_blob.is_none();
    let merged = match existing_blob {
        None => conversation.clone(),
        Some(blob) => {
            let existing: Conversation =
                serde_json::from_slice(&blob).map_err(|_| StoreError::Schema)?;
            Conversation {
                created_at: existing.created_at.min(conversation.created_at),
                title: conversation.title.clone().or(existing.title),
                participants: if conversation.participants.is_empty() {
                    existing.participants
                } else {
                    conversation.participants.clone()
                },
                ..conversation.clone()
            }
        }
    };
    let encoded = serde_json::to_vec(&merged).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "INSERT INTO conversations(id, created_at, last_message_at, data) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET created_at=excluded.created_at, last_message_at=excluded.last_message_at, data=excluded.data",
        params![
            &conversation_key,
            merged.created_at,
            merged.last_message_at,
            encoded
        ],
    )?;
    let aliases = merged
        .aliases
        .iter()
        .map(conversation_id)
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = upsert_message_row(
        &tx,
        &message_key,
        &conversation_key,
        message.created_at,
        &data,
        &aliases,
        &merged.id,
    )?;
    refresh_summary(&tx, &conversation_key, merged)?;
    tx.commit()?;
    Ok((outcome, created))
}

pub fn update_delivery(
    store: &mut EncryptedStore,
    conversation: &ConversationId,
    message: &MessageId,
    delivery: DeliveryState,
) -> Result<Option<Message>, StoreError> {
    let canonical = super::conversations::get_conversation(store, conversation)?;
    let canonical_id = canonical
        .as_ref()
        .map(|row| row.id.clone())
        .unwrap_or_else(|| conversation.clone());
    let mut accepted = vec![conversation_id(&canonical_id)?];
    if let Some(row) = canonical.as_ref() {
        accepted.extend(
            row.aliases
                .iter()
                .map(conversation_id)
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let message_key = message_id(message)?;
    let tx = store._connection.transaction()?;
    let row: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT conversation_id, data FROM messages WHERE id=?1",
            [&message_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((owner, data)) = row else {
        tx.commit()?;
        return Ok(None);
    };
    if !accepted.contains(&owner) {
        return Err(StoreError::MessageConversationMismatch);
    }
    let mut value: Message = serde_json::from_slice(&data).map_err(|_| StoreError::Schema)?;
    if message_id(value.id())? != message_key || conversation_id(value.conversation_id())? != owner
    {
        return Err(StoreError::Schema);
    }
    value.delivery = delivery;
    let encoded = serde_json::to_vec(&value).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "UPDATE messages SET data=?2 WHERE id=?1",
        params![message_key, encoded],
    )?;
    tx.commit()?;
    super::messages::remap_message(&value, &canonical_id).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{get_conversation, get_message, list_conversations, KEY_BYTES};
    use bloom_domain::{ConversationKind, Handle, HandleKind, MessagePart, TransportId};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, EncryptedStore, ConversationId) {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut store =
            EncryptedStore::open_with_key(private.join("store.db"), &[73; KEY_BYTES]).unwrap();
        let cid = ConversationId::new(TransportId::new("ingest-test").unwrap(), "room").unwrap();
        let conversation = Conversation {
            id: cid.clone(),
            remote_id: "private-remote".into(),
            kind: ConversationKind::Direct,
            title: Some("private-title".into()),
            participants: vec![],
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };
        super::super::conversations::upsert_conversation(&mut store, &conversation).unwrap();
        (dir, store, cid)
    }

    fn message(
        cid: &ConversationId,
        value: &str,
        at: i64,
        body: &str,
        state: DeliveryState,
    ) -> Message {
        Message::new(
            MessageId::new(cid.transport().clone(), value).unwrap(),
            cid.clone(),
            None,
            Handle::new(HandleKind::Opaque, "synthetic-sender").unwrap(),
            vec![MessagePart::Text(body.into())],
            at,
            None,
            state,
            false,
            None,
            vec![],
            None,
        )
        .unwrap()
    }

    #[test]
    fn ingest_updates_summary_delivery_and_keeps_database_encrypted() {
        let (_dir, mut store, cid) = fixture();
        let secret = "ingest-private-plaintext-7f23";
        let first = message(&cid, "first", 10, secret, DeliveryState::Queued);
        let later = message(&cid, "later", 20, "later-secret", DeliveryState::Sent);
        assert_eq!(
            ingest_message(&mut store, &first).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        assert_eq!(
            ingest_message(&mut store, &first).unwrap(),
            MessageUpsertOutcome::Unchanged
        );
        assert_eq!(
            ingest_message(&mut store, &later).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        let changed = message(&cid, "later", 5, "edited-secret", DeliveryState::Sent);
        assert_eq!(
            ingest_message(&mut store, &changed).unwrap(),
            MessageUpsertOutcome::Updated
        );
        assert_eq!(
            get_conversation(&store, &cid)
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(10)
        );
        let indexed: Option<i64> = store
            ._connection
            .query_row(
                "SELECT last_message_at FROM conversations WHERE id=?1",
                [conversation_id(&cid).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, Some(10));
        let ordered = list_conversations(&store).unwrap();
        assert_eq!(ordered[0].id, cid);
        assert_eq!(
            get_message(&store, changed.id()).unwrap(),
            Some(changed.clone())
        );

        let delivered = update_delivery(&mut store, &cid, changed.id(), DeliveryState::Delivered)
            .unwrap()
            .unwrap();
        assert_eq!(delivered.delivery, DeliveryState::Delivered);
        assert_eq!(delivered.id(), changed.id());
        assert_eq!(delivered.conversation_id(), &cid);
        assert_eq!(
            get_conversation(&store, &cid)
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(10)
        );
        let wrong = ConversationId::new(TransportId::new("other").unwrap(), "room").unwrap();
        assert!(matches!(
            update_delivery(&mut store, &wrong, changed.id(), DeliveryState::Read),
            Err(StoreError::MessageConversationMismatch)
        ));
        assert!(update_delivery(
            &mut store,
            &cid,
            &MessageId::new(cid.transport().clone(), "absent").unwrap(),
            DeliveryState::Read
        )
        .unwrap()
        .is_none());
        drop(store);
        let bytes = fs::read(_dir.path().join("private/store.db")).unwrap();
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()));
    }

    #[test]
    fn failed_ingests_roll_back_message_and_summary() {
        let (_dir, mut store, cid) = fixture();
        let base = message(&cid, "stable", 40, "stable-secret", DeliveryState::Sent);
        ingest_message(&mut store, &base).unwrap();
        let wrong = ConversationId::new(cid.transport().clone(), "elsewhere").unwrap();
        let moved = message(&wrong, "stable", 1, "moved-secret", DeliveryState::Sent);
        assert!(matches!(
            ingest_message(&mut store, &moved),
            Err(StoreError::MessageConversationMismatch)
        ));
        assert_eq!(get_message(&store, base.id()).unwrap(), Some(base.clone()));

        let missing = ConversationId::new(cid.transport().clone(), "missing").unwrap();
        let orphan = message(&missing, "orphan", 99, "orphan-secret", DeliveryState::Sent);
        assert!(matches!(
            ingest_message(&mut store, &orphan),
            Err(StoreError::Sqlite(_))
        ));
        let count: i64 = store
            ._connection
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let key = conversation_id(&cid).unwrap();
        store
            ._connection
            .execute("UPDATE conversations SET data=x'00' WHERE id=?1", [&key])
            .unwrap();
        let changed = message(&cid, "stable", 2, "changed-secret", DeliveryState::Sent);
        assert!(matches!(
            ingest_message(&mut store, &changed),
            Err(StoreError::Schema)
        ));
        assert_eq!(get_message(&store, base.id()).unwrap(), Some(base));
        let indexed: Option<i64> = store
            ._connection
            .query_row(
                "SELECT last_message_at FROM conversations WHERE id=?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, Some(40));
    }
}
