use super::{EncryptedStore, StoreError};
use bloom_domain::{ConversationId, Message, MessageId};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageUpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

fn stable_id(id: &MessageId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}

fn stable_conversation_id(id: &ConversationId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}

/// Upserts a message scoped by its transport-aware MessageId. Conversation summaries and
/// delivery state are owned by Wave B2b3, not this operation.
pub fn upsert_message(
    store: &mut EncryptedStore,
    message: &Message,
) -> Result<MessageUpsertOutcome, StoreError> {
    let id = stable_id(message.id())?;
    let conversation_id = stable_conversation_id(message.conversation_id())?;
    let data = serde_json::to_vec(message).map_err(|_| StoreError::Schema)?;
    let tx = store._connection.transaction()?;
    let existing: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT conversation_id, data FROM messages WHERE id=?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let outcome = match existing {
        None => {
            tx.execute("INSERT INTO messages(id, conversation_id, created_at, data) VALUES (?1, ?2, ?3, ?4)", params![id, conversation_id, message.created_at, data])?;
            MessageUpsertOutcome::Inserted
        }
        Some((old_conversation, old_data))
            if old_conversation == conversation_id && old_data == data =>
        {
            MessageUpsertOutcome::Unchanged
        }
        Some((old_conversation, _)) if old_conversation != conversation_id => {
            return Err(StoreError::MessageConversationMismatch);
        }
        Some(_) => {
            tx.execute(
                "UPDATE messages SET created_at=?2, data=?3 WHERE id=?1",
                params![id, message.created_at, data],
            )?;
            MessageUpsertOutcome::Updated
        }
    };
    tx.commit()?;
    Ok(outcome)
}

pub fn get_message(store: &EncryptedStore, id: &MessageId) -> Result<Option<Message>, StoreError> {
    let key = stable_id(id)?;
    let data: Option<Vec<u8>> = store
        ._connection
        .query_row(
            "SELECT data FROM messages WHERE id=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    data.map(|data| serde_json::from_slice(&data).map_err(|_| StoreError::Schema))
        .transpose()
}

pub(crate) fn remap_message(
    message: &Message,
    conversation_id: &ConversationId,
) -> Result<Message, StoreError> {
    Message::new(
        message.id().clone(),
        conversation_id.clone(),
        message.remote_id.clone(),
        message.sender.clone(),
        message.parts.clone(),
        message.created_at,
        message.sent_at,
        message.delivery.clone(),
        message.from_me,
        message.edited_at,
        message.reactions.clone(),
        message.unsent_at,
    )
    .map_err(|_| StoreError::MessageConversationMismatch)
}

pub fn history(
    store: &EncryptedStore,
    conversation_id: &ConversationId,
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if let Some(canonical) = super::conversations::get_conversation(store, conversation_id)? {
        return history_for_conversation(store, &canonical, limit);
    }
    history_for_identities(store, conversation_id, &[], limit)
}

pub(crate) fn history_for_conversation(
    store: &EncryptedStore,
    conversation: &bloom_domain::Conversation,
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    history_for_identities(store, &conversation.id, &conversation.aliases, limit)
}

fn history_for_identities(
    store: &EncryptedStore,
    canonical_id: &ConversationId,
    aliases: &[ConversationId],
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let canonical_id = canonical_id.clone();
    let mut identities = vec![canonical_id.clone()];
    identities.extend(aliases.iter().cloned());
    identities.sort_by(|left, right| left.value().cmp(right.value()));
    identities.dedup();

    let mut messages = Vec::new();
    for identity in identities {
        let key = stable_conversation_id(&identity)?;
        let mut statement = store._connection.prepare("SELECT data FROM (SELECT id, data, created_at FROM messages WHERE conversation_id=?1 ORDER BY created_at DESC, id DESC LIMIT ?2) ORDER BY created_at ASC, id ASC")?;
        let records = statement.query_map(params![key, limit], |row| row.get::<_, Vec<u8>>(0))?;
        for record in records {
            let message: Message =
                serde_json::from_slice(&record?).map_err(|_| StoreError::Schema)?;
            messages.push(remap_message(&message, &canonical_id)?);
        }
    }
    messages.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id().value().cmp(right.id().value()))
    });
    if messages.len() > limit as usize {
        messages.drain(..messages.len() - limit as usize);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::KEY_BYTES;
    use bloom_domain::{
        Conversation, ConversationKind, DeliveryState, Handle, HandleKind, MessagePart, TransportId,
    };
    use std::fs;

    fn fixture_message(
        transport: &str,
        message: &str,
        conversation: &str,
        at: i64,
        body: &str,
        delivery: DeliveryState,
    ) -> Message {
        let transport = TransportId::new(transport).unwrap();
        Message::new(
            MessageId::new(transport.clone(), message).unwrap(),
            ConversationId::new(transport, conversation).unwrap(),
            None,
            Handle::new(HandleKind::Opaque, "synthetic-sender").unwrap(),
            vec![MessagePart::Text(body.into())],
            at,
            None,
            delivery,
            false,
            None,
            vec![],
            None,
        )
        .unwrap()
    }

    fn setup() -> (tempfile::TempDir, EncryptedStore, ConversationId) {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let conversation = ConversationId::new(TransportId::new("alpha").unwrap(), "room").unwrap();
        let mut store =
            EncryptedStore::open_with_key(private.join("messages.db"), &[9; KEY_BYTES]).unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: conversation.clone(),
                remote_id: "room".into(),
                kind: ConversationKind::Direct,
                title: None,
                participants: Vec::new(),
                created_at: 0,
                last_message_at: None,
                aliases: Vec::new(),
                preview: None,
            },
        )
        .unwrap();
        (temp, store, conversation)
    }

    #[test]
    fn transport_identity_upserts_history_and_encryption() {
        let (_temp, mut store, conversation) = setup();
        let body = "unique-message-plaintext-b2b2";
        let first = fixture_message("alpha", "same", "room", 10, body, DeliveryState::Queued);
        let second = fixture_message(
            "beta",
            "same",
            "elsewhere",
            20,
            "other",
            DeliveryState::Sent,
        );
        let second_conversation = second.conversation_id().clone();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: second_conversation.clone(),
                remote_id: "elsewhere".into(),
                kind: ConversationKind::Direct,
                title: None,
                participants: Vec::new(),
                created_at: 0,
                last_message_at: None,
                aliases: Vec::new(),
                preview: None,
            },
        )
        .unwrap();
        assert_eq!(
            upsert_message(&mut store, &first).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        assert_eq!(
            upsert_message(&mut store, &first).unwrap(),
            MessageUpsertOutcome::Unchanged
        );
        assert_eq!(
            upsert_message(&mut store, &second).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        let changed = fixture_message("alpha", "same", "room", 10, body, DeliveryState::Delivered);
        assert_eq!(
            upsert_message(&mut store, &changed).unwrap(),
            MessageUpsertOutcome::Updated
        );
        assert_eq!(
            get_message(&store, changed.id()).unwrap(),
            Some(changed.clone())
        );
        assert!(history(&store, &second_conversation, 10)
            .unwrap()
            .iter()
            .any(|m| m.id() == second.id()));
        let wrong_transport =
            ConversationId::new(TransportId::new("wrong").unwrap(), "room").unwrap();
        assert!(history(&store, &wrong_transport, 10).unwrap().is_empty());
        let tied = fixture_message("alpha", "tie", "room", 10, "tie", DeliveryState::Sent);
        let later = fixture_message("alpha", "later", "room", 20, "later", DeliveryState::Sent);
        upsert_message(&mut store, &tied).unwrap();
        upsert_message(&mut store, &later).unwrap();
        let all = history(&store, &conversation, 10).unwrap();
        let ids: Vec<_> = all.iter().map(|m| m.id().value()).collect();
        assert_eq!(ids, vec!["same", "tie", "later"]);
        assert_eq!(history(&store, &conversation, 0).unwrap().len(), 0);
        assert_eq!(
            history(&store, &conversation, 2)
                .unwrap()
                .iter()
                .map(|m| m.id().value())
                .collect::<Vec<_>>(),
            vec!["tie", "later"]
        );
        for suffix in ["", "-wal"] {
            let path = format!("{}{}", store.path().display(), suffix);
            if let Ok(bytes) = fs::read(path) {
                assert!(!bytes.windows(body.len()).any(|w| w == body.as_bytes()));
            }
        }
    }

    #[test]
    fn missing_conversation_and_conversation_move_rejected() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "move", "room", 1, "body", DeliveryState::Sent);
        assert_eq!(
            upsert_message(&mut store, &message).unwrap(),
            MessageUpsertOutcome::Inserted
        );
        let missing = fixture_message("alpha", "orphan", "missing", 2, "body", DeliveryState::Sent);
        assert!(matches!(
            upsert_message(&mut store, &missing),
            Err(StoreError::Sqlite(_))
        ));
        let other = ConversationId::new(TransportId::new("alpha").unwrap(), "other").unwrap();
        let key = stable_conversation_id(&other).unwrap();
        store
            ._connection
            .execute(
                "INSERT INTO conversations(id, created_at, data) VALUES (?1, 0, x'00')",
                params![key],
            )
            .unwrap();
        let moved = fixture_message("alpha", "move", "other", 3, "changed", DeliveryState::Sent);
        assert!(matches!(
            upsert_message(&mut store, &moved),
            Err(StoreError::MessageConversationMismatch)
        ));
        assert_eq!(get_message(&store, message.id()).unwrap(), Some(message));
    }
}
