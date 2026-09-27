use super::{conversations::merge_conversation, EncryptedStore, StoreError};
use bloom_domain::{
    Conversation, ConversationId, ConversationKind, Handle, HandleKind, Participant,
};
use rusqlite::params;
use std::collections::BTreeMap;

fn stable_id(id: &ConversationId) -> Result<String, StoreError> {
    serde_json::to_string(id).map_err(|_| StoreError::Schema)
}

fn stored_conversations(
    connection: &rusqlite::Connection,
) -> Result<Vec<Conversation>, StoreError> {
    let mut statement = connection.prepare("SELECT data FROM conversations")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    rows.map(|row| serde_json::from_slice(&row?).map_err(|_| StoreError::Schema))
        .collect()
}

fn write_conversation(
    tx: &rusqlite::Transaction<'_>,
    conversation: &Conversation,
) -> Result<(), StoreError> {
    let key = stable_id(&conversation.id)?;
    let data = serde_json::to_vec(conversation).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "INSERT INTO conversations(id, created_at, last_message_at, data) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET created_at=excluded.created_at, last_message_at=excluded.last_message_at, data=excluded.data",
        params![key, conversation.created_at, conversation.last_message_at, data],
    )?;
    Ok(())
}

/// Non-destructively associate exact direct-chat identities while retaining every
/// original conversation row and message. The canonical row carries alias IDs;
/// list/history APIs resolve those IDs back to the canonical thread.
pub fn reconcile_direct_conversation(
    store: &mut EncryptedStore,
    incoming: &Conversation,
) -> Result<(Conversation, usize), StoreError> {
    let records = stored_conversations(&store._connection)?;
    let Some(handle) = direct_handle(incoming) else {
        if incoming.kind == ConversationKind::Group
            && records.iter().any(|row| {
                (row.id == incoming.id || row.aliases.contains(&incoming.id))
                    && row.kind == ConversationKind::Direct
            })
        {
            return Err(StoreError::IdentityConflict);
        }
        merge_conversation(store, incoming)?;
        return Ok((incoming.clone(), 0));
    };
    let identity = handle_key(handle).ok_or(StoreError::Schema)?;
    if records.iter().any(|row| {
        (row.id == incoming.id || row.aliases.contains(&incoming.id))
            && (row.kind != ConversationKind::Direct
                || direct_handle(row).and_then(handle_key).as_deref() != Some(&identity))
    }) {
        return Err(StoreError::IdentityConflict);
    }
    let synthetic_id = synthetic_id(handle);
    let tx = store._connection.transaction()?;

    let mut candidates = records
        .iter()
        .filter(|stored| {
            stored.id.transport() == incoming.id.transport()
                && (stored.id == incoming.id
                    || stored.aliases.contains(&incoming.id)
                    || direct_handle(stored).and_then(handle_key).as_deref() == Some(&identity))
        })
        .cloned()
        .collect::<Vec<_>>();

    if let Some(existing) = candidates.iter_mut().find(|row| row.id == incoming.id) {
        existing.remote_id = incoming.remote_id.clone();
        existing.kind = incoming.kind.clone();
        if incoming.title.is_some() {
            existing.title = incoming.title.clone();
        }
        if !incoming.participants.is_empty() {
            existing.participants = incoming.participants.clone();
        }
        existing.created_at = match (existing.created_at, incoming.created_at) {
            (0, value) => value,
            (value, 0) => value,
            (left, right) => left.min(right),
        };
        existing.last_message_at = existing.last_message_at.max(incoming.last_message_at);
    } else {
        candidates.push(incoming.clone());
    }
    candidates.sort_by(|left, right| left.id.value().cmp(right.id.value()));

    let canonical_index = candidates
        .iter()
        .position(|row| row.id.value() == synthetic_id)
        .or_else(|| candidates.iter().position(|row| row.id == incoming.id))
        .unwrap_or_else(|| {
            candidates
                .iter()
                .enumerate()
                .min_by_key(|(_, row)| {
                    (
                        if row.created_at == 0 {
                            i64::MAX
                        } else {
                            row.created_at
                        },
                        row.id.value(),
                    )
                })
                .map(|(index, _)| index)
                .unwrap_or(0)
        });
    let mut canonical = candidates
        .get(canonical_index)
        .cloned()
        .unwrap_or_else(|| incoming.clone());
    let old_alias_count = canonical.aliases.len();

    let mut aliases = BTreeMap::<String, ConversationId>::new();
    for row in &candidates {
        for alias in &row.aliases {
            if alias != &canonical.id {
                aliases.insert(stable_id(alias)?, alias.clone());
            }
        }
        if row.id != canonical.id {
            aliases.insert(stable_id(&row.id)?, row.id.clone());
        }
    }
    canonical.aliases = aliases.into_values().collect();
    canonical
        .aliases
        .sort_by(|left, right| left.value().cmp(right.value()));

    // Prefer the transport's real GUID for routing. The recipient-shaped ID is
    // only a provisional local identity created before the engine knows the chat.
    if incoming.id.value() != synthetic_id {
        canonical.remote_id = incoming.remote_id.clone();
    } else if let Some(engine_row) = candidates.iter().find(|row| row.id.value() != synthetic_id) {
        canonical.remote_id = engine_row.remote_id.clone();
    }

    canonical.created_at = candidates
        .iter()
        .map(|row| row.created_at)
        .filter(|value| *value > 0)
        .min()
        .unwrap_or_default();
    canonical.last_message_at = candidates
        .iter()
        .filter_map(|row| row.last_message_at)
        .max();
    if canonical.title.is_none() {
        canonical.title = candidates.iter().find_map(|row| row.title.clone());
    }
    let mut participants = Vec::<Participant>::new();
    for participant in candidates.iter().flat_map(|row| &row.participants) {
        let mut participant = participant.clone();
        if let Some(handle) = canonical_handle(&participant.handle) {
            participant.handle = handle;
        }
        if let Some(existing) = participants.iter_mut().find(|item| {
            item.is_me == participant.is_me && same_direct_handle(&item.handle, &participant.handle)
        }) {
            if existing.display_name.is_none() {
                existing.display_name = participant.display_name.clone();
            }
        } else {
            participants.push(participant);
        }
    }
    if !participants.is_empty() {
        canonical.participants = participants;
    }

    let added = canonical.aliases.len().saturating_sub(old_alias_count);
    for row in &mut candidates {
        if row.id != canonical.id {
            // Alias source rows stay intact, including their history, but do not
            // point back to the canonical row and create an alias cycle.
            row.aliases.clear();
            write_conversation(&tx, row)?;
        }
    }
    write_conversation(&tx, &canonical)?;
    tx.commit()?;
    Ok((canonical, added))
}

/// Consolidate already-stored duplicate direct identities at startup/list time.
/// Rows and message records remain untouched; only the canonical row gains aliases.
pub fn reconcile_stored_direct_conversations(
    store: &mut EncryptedStore,
) -> Result<usize, StoreError> {
    let rows = stored_conversations(&store._connection)?;
    let mut groups = BTreeMap::<(String, String), Vec<Conversation>>::new();
    for row in rows {
        let Some(handle) = direct_handle(&row) else {
            continue;
        };
        let Some(identity) = handle_key(handle) else {
            continue;
        };
        groups
            .entry((row.id.transport().as_str().to_owned(), identity))
            .or_default()
            .push(row);
    }

    let mut linked = 0usize;
    for rows in groups.values_mut().filter(|rows| rows.len() > 1) {
        rows.sort_by(|left, right| left.id.value().cmp(right.id.value()));
        let Some(handle) = rows.first().and_then(direct_handle) else {
            continue;
        };
        let synthetic = synthetic_id(handle);
        let incoming = rows
            .iter()
            .find(|row| row.id.value() != synthetic)
            .or_else(|| rows.first())
            .cloned();
        if let Some(incoming) = incoming {
            linked = linked.saturating_add(reconcile_direct_conversation(store, &incoming)?.1);
        }
    }
    Ok(linked)
}

fn direct_handle(conversation: &Conversation) -> Option<&Handle> {
    if conversation.kind != ConversationKind::Direct {
        return None;
    }
    let mut remote = conversation
        .participants
        .iter()
        .filter(|participant| !participant.is_me)
        .map(|participant| &participant.handle);
    let handle = remote.next()?;
    if remote.any(|candidate| handle_key(candidate) != handle_key(handle)) {
        return None;
    }
    Some(handle)
}

fn same_direct_handle(left: &Handle, right: &Handle) -> bool {
    matches!((handle_key(left), handle_key(right)), (Some(left), Some(right)) if left == right)
}

pub(crate) fn direct_conversation_matches(conversation: &Conversation, handle: &Handle) -> bool {
    direct_handle(conversation).is_some_and(|candidate| same_direct_handle(candidate, handle))
}

fn canonical_handle(handle: &Handle) -> Option<Handle> {
    match handle.kind() {
        HandleKind::Tel => Handle::new(
            HandleKind::Tel,
            handle_key(handle)?.strip_prefix("tel:")?.to_owned(),
        )
        .ok(),
        HandleKind::Email | HandleKind::Opaque => Some(handle.clone()),
    }
}

fn handle_key(handle: &Handle) -> Option<String> {
    let value = match handle.kind() {
        HandleKind::Email => format!("email:{}", handle.value()),
        HandleKind::Opaque => format!("opaque:{}", handle.value()),
        HandleKind::Tel => {
            let value = handle.value();
            let mut normalized = String::new();
            let mut digits = 0usize;
            for (index, character) in value.chars().enumerate() {
                if character.is_ascii_digit() {
                    digits += 1;
                    normalized.push(character);
                } else if character == '+' && index == 0 {
                    normalized.push(character);
                } else if character.is_whitespace() || "().-".contains(character) {
                    continue;
                } else {
                    return None;
                }
            }
            if digits == 0 {
                return None;
            }
            format!("tel:{normalized}")
        }
    };
    Some(value)
}

fn synthetic_id(handle: &Handle) -> String {
    let recipient = match handle.kind() {
        HandleKind::Email => format!("mailto:{}", handle.value()),
        HandleKind::Tel => format!(
            "tel:{}",
            handle_key(handle)
                .unwrap_or_default()
                .trim_start_matches("tel:")
        ),
        HandleKind::Opaque => handle.value().to_owned(),
    };
    format!("iMessage;-;{recipient}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::KEY_BYTES;
    use bloom_domain::{
        ConversationId, DeliveryState, Message, MessageId, MessagePart, Participant, TransportId,
    };
    use std::fs;

    fn setup() -> (tempfile::TempDir, EncryptedStore) {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store =
            EncryptedStore::open_with_key(private.join("store.db"), &[47; KEY_BYTES]).unwrap();
        (dir, store)
    }

    fn direct(
        id: &str,
        remote_id: &str,
        recipient: &str,
        kind: HandleKind,
        last: Option<i64>,
    ) -> Conversation {
        let transport = TransportId::new("openbubbles").unwrap();
        Conversation {
            id: ConversationId::new(transport, id).unwrap(),
            remote_id: remote_id.to_owned(),
            kind: ConversationKind::Direct,
            title: None,
            participants: vec![Participant {
                handle: Handle::new(kind, recipient).unwrap(),
                display_name: None,
                is_me: false,
            }],
            created_at: 10,
            last_message_at: last,
            aliases: Vec::new(),
            preview: None,
        }
    }

    fn message(conversation: &Conversation, id: &str, at: i64, body: &str) -> Message {
        Message::new(
            MessageId::new(conversation.id.transport().clone(), id).unwrap(),
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Email, "person@example.test").unwrap(),
            vec![MessagePart::Text(body.to_owned())],
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

    #[test]
    fn direct_guid_links_only_exact_normalized_participant_and_preserves_rows() {
        let (_dir, mut store) = setup();
        let local = direct(
            "iMessage;-;mailto:person@example.test",
            "iMessage;-;mailto:person@example.test",
            "person@example.test",
            HandleKind::Email,
            Some(20),
        );
        let engine = direct(
            "iMessage;-;person@example.test",
            "iMessage;-;person@example.test",
            "person@example.test",
            HandleKind::Email,
            Some(30),
        );
        let other = direct(
            "iMessage;-;other@example.test",
            "iMessage;-;other@example.test",
            "other@example.test",
            HandleKind::Email,
            Some(40),
        );
        super::super::conversations::upsert_conversation(&mut store, &local).unwrap();
        super::super::conversations::upsert_conversation(&mut store, &engine).unwrap();
        super::super::conversations::upsert_conversation(&mut store, &other).unwrap();
        let local_message = message(&local, "outbound", 25, "outbound history");
        let engine_message = message(&engine, "inbound", 35, "inbound history");
        crate::store::ingest_inbound(&mut store, &local, &local_message).unwrap();
        crate::store::ingest_inbound(&mut store, &engine, &engine_message).unwrap();

        let linked = reconcile_stored_direct_conversations(&mut store).unwrap();
        assert_eq!(linked, 1);
        let conversations = super::super::conversations::list_conversations(&store).unwrap();
        assert_eq!(conversations.len(), 2);
        let canonical = conversations
            .iter()
            .find(|conversation| conversation.id == local.id)
            .unwrap();
        assert_eq!(canonical.remote_id, engine.remote_id);
        assert_eq!(canonical.aliases, vec![engine.id.clone()]);
        assert_eq!(canonical.last_message_at, Some(35));
        assert!(
            super::super::conversations::get_conversation(&store, &engine.id)
                .unwrap()
                .is_some_and(|resolved| resolved.id == local.id)
        );
        let history = crate::store::history(&store, &local.id, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history
            .iter()
            .all(|message| message.conversation_id() == &local.id));
        assert_eq!(
            history[0].parts(),
            &[MessagePart::Text("outbound history".into())]
        );
        assert_eq!(
            history[1].parts(),
            &[MessagePart::Text("inbound history".into())]
        );

        let replay = crate::store::remap_message(&engine_message, &local.id).unwrap();
        let (outcome, _) = crate::store::ingest_inbound(&mut store, canonical, &replay).unwrap();
        assert_eq!(outcome, crate::store::MessageUpsertOutcome::Unchanged);
        assert_eq!(
            crate::store::history(&store, &local.id, 10).unwrap().len(),
            2
        );
        assert_eq!(
            reconcile_stored_direct_conversations(&mut store).unwrap(),
            0
        );

        let database = store.path().to_path_buf();
        drop(store);
        let reopened = EncryptedStore::open_with_key(database, &[47; KEY_BYTES]).unwrap();
        assert_eq!(
            super::super::conversations::list_conversations(&reopened)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            crate::store::history(&reopened, &local.id, 10)
                .unwrap()
                .len(),
            2
        );
        assert!(
            super::super::conversations::get_conversation(&reopened, &engine.id)
                .unwrap()
                .is_some_and(|resolved| resolved.id == local.id)
        );
    }

    #[test]
    fn groups_and_different_handle_kinds_are_never_linked() {
        let (_dir, mut store) = setup();
        let local = direct("local", "local", "+15550000001", HandleKind::Tel, None);
        let mut group = direct(
            "group-guid",
            "group-guid",
            "+15550000001",
            HandleKind::Tel,
            None,
        );
        group.kind = ConversationKind::Group;
        let opaque = direct(
            "opaque-guid",
            "opaque-guid",
            "+15550000001",
            HandleKind::Opaque,
            None,
        );
        let mut other_transport = direct(
            "other-transport-guid",
            "other-transport-guid",
            "+15550000001",
            HandleKind::Tel,
            None,
        );
        other_transport.id = ConversationId::new(
            TransportId::new("another-transport").unwrap(),
            "other-transport-guid",
        )
        .unwrap();
        super::super::conversations::upsert_conversation(&mut store, &local).unwrap();
        reconcile_direct_conversation(&mut store, &group).unwrap();
        reconcile_direct_conversation(&mut store, &opaque).unwrap();
        reconcile_direct_conversation(&mut store, &other_transport).unwrap();
        assert_eq!(
            super::super::conversations::list_conversations(&store)
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn mismatched_recipient_or_group_reuse_of_an_identity_fails_safely() {
        let (_dir, mut store) = setup();
        let existing = direct(
            "engine-guid",
            "engine-guid",
            "person@example.test",
            HandleKind::Email,
            Some(10),
        );
        super::super::conversations::upsert_conversation(&mut store, &existing).unwrap();

        let different = direct(
            "engine-guid",
            "engine-guid",
            "other@example.test",
            HandleKind::Email,
            Some(20),
        );
        assert!(matches!(
            reconcile_direct_conversation(&mut store, &different),
            Err(StoreError::IdentityConflict)
        ));
        let mut group = existing.clone();
        group.kind = ConversationKind::Group;
        assert!(matches!(
            reconcile_direct_conversation(&mut store, &group),
            Err(StoreError::IdentityConflict)
        ));
        let rows = super::super::conversations::list_conversations(&store).unwrap();
        assert_eq!(rows, vec![existing]);
    }

    #[test]
    fn phone_formatting_normalizes_but_plus_and_digits_remain_exact() {
        assert_eq!(
            handle_key(&Handle::new(HandleKind::Tel, "+1 (555) 000-0001").unwrap()),
            Some("tel:+15550000001".into())
        );
        assert_ne!(
            handle_key(&Handle::new(HandleKind::Tel, "+15550000001").unwrap()),
            handle_key(&Handle::new(HandleKind::Tel, "15550000001").unwrap())
        );
        assert_ne!(
            handle_key(&Handle::new(HandleKind::Email, "a@example.test").unwrap()),
            handle_key(&Handle::new(HandleKind::Opaque, "a@example.test").unwrap())
        );
    }
}
