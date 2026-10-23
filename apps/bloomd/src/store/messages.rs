use super::{EncryptedStore, StoreError};
use bloom_domain::{ConversationId, Message, MessageId, MessagePart};
use rusqlite::{params, OptionalExtension};
use std::collections::HashSet;

pub use bloom_ipc::MessageSearchHit;
use bloom_ipc::ParticipantFilter;

const SEARCH_SCAN_LIMIT: usize = 20_000;
const SNIPPET_CONTEXT_CHARS: usize = 30;
const ELLIPSIS: char = '\u{2026}';

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

/// Search text parts of stored messages, newest first. The scan is bounded to the
/// newest [`SEARCH_SCAN_LIMIT`] rows so one query cannot walk an unbounded store,
/// and it stops as soon as `limit` hits have been collected.
pub fn search_messages(
    store: &EncryptedStore,
    query: &str,
    limit: u16,
) -> Result<Vec<MessageSearchHit>, StoreError> {
    search_messages_with_participant(store, query, limit, None)
}

/// Search text parts of stored messages, optionally scoped to canonical
/// conversations whose current participants include the exact filter handle.
/// Membership is checked before hits are counted, so a limit never hides a
/// matching older message behind newer out-of-scope hits.
pub fn search_messages_with_participant(
    store: &EncryptedStore,
    query: &str,
    limit: u16,
    participant: Option<&ParticipantFilter>,
) -> Result<Vec<MessageSearchHit>, StoreError> {
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let query_lower = query.to_lowercase();
    if query_lower.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve current canonical membership once, from metadata only. Retired rows
    // are excluded by `list_conversations`, so an alias-only participant cannot
    // grant membership to the canonical conversation.
    let membership = match participant {
        Some(filter) => {
            let matching = super::conversations::list_conversations(store)?
                .into_iter()
                .filter(|conversation| {
                    conversation.id.transport() == &filter.transport
                        && conversation
                            .participants
                            .iter()
                            .any(|participant| participant.handle == filter.handle)
                })
                .map(|conversation| conversation.id)
                .collect::<HashSet<_>>();
            if matching.is_empty() {
                return Ok(Vec::new());
            }
            Some(matching)
        }
        None => None,
    };

    // `messages_recent_order` serves this ordering, so the LIMIT bounds the rows
    // read rather than sorting the whole table first.
    let mut statement = store
        ._connection
        .prepare("SELECT id, data FROM messages ORDER BY created_at DESC, id DESC LIMIT ?1")?;
    let mut rows = statement.query(params![SEARCH_SCAN_LIMIT as i64])?;
    // Resolve aliases from one read instead of decoding every conversation per hit.
    let lookup = super::conversations::alias_map(store)?;
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let data: Vec<u8> = row.get(1)?;
        let message: Message = serde_json::from_slice(&data).map_err(|_| StoreError::Schema)?;
        let Some(snippet) = search_snippet(&message, &query_lower) else {
            continue;
        };
        if !seen.insert(key) {
            continue;
        }
        let conversation_id = lookup
            .get(message.conversation_id())
            .cloned()
            .unwrap_or_else(|| message.conversation_id().clone());
        if let Some(membership) = &membership {
            if !membership.contains(&conversation_id) {
                continue;
            }
        }
        hits.push(MessageSearchHit {
            conversation_id,
            message_id: message.id().clone(),
            created_at: message.created_at,
            snippet,
        });
        if hits.len() >= limit as usize {
            break;
        }
    }
    Ok(hits)
}

fn search_snippet(message: &Message, query_lower: &str) -> Option<String> {
    for part in message.parts() {
        let MessagePart::Text(text) = part else {
            continue;
        };
        let (lower, boundaries) = lowercase_with_boundaries(text);
        let Some(start_lower) = lower.find(query_lower) else {
            continue;
        };
        let end_lower = start_lower + query_lower.len();
        let start = boundaries[start_lower].0;
        let end = boundaries[end_lower - 1].1;
        return Some(snippet_window(text, start, end));
    }
    None
}

/// Lowercase a string and record the original character span of every lowered
/// byte. Unicode lowercasing can change byte lengths, so slicing the lowered text
/// and reusing those offsets on the original would not be char-boundary safe.
fn lowercase_with_boundaries(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut lower = String::with_capacity(text.len());
    let mut boundaries = Vec::with_capacity(text.len());
    for (start, character) in text.char_indices() {
        let end = start + character.len_utf8();
        let before = lower.len();
        lower.extend(character.to_lowercase());
        boundaries.extend(std::iter::repeat_n((start, end), lower.len() - before));
    }
    (lower, boundaries)
}

fn snippet_window(text: &str, start: usize, end: usize) -> String {
    let before: Vec<usize> = text[..start]
        .char_indices()
        .map(|(index, _)| index)
        .collect();
    let context_start = if before.len() > SNIPPET_CONTEXT_CHARS {
        before[before.len() - SNIPPET_CONTEXT_CHARS]
    } else {
        0
    };
    let after: Vec<usize> = text[end..]
        .char_indices()
        .map(|(index, _)| end + index)
        .collect();
    let context_end = if after.len() > SNIPPET_CONTEXT_CHARS {
        after[SNIPPET_CONTEXT_CHARS]
    } else {
        text.len()
    };

    let body = text[context_start..context_end]
        .replace("\r\n", " ")
        .replace(&['\r', '\n'][..], " ");
    let mut snippet = String::with_capacity(body.len() + 2 * ELLIPSIS.len_utf8());
    if context_start > 0 {
        snippet.push(ELLIPSIS);
    }
    snippet.push_str(&body);
    if context_end < text.len() {
        snippet.push(ELLIPSIS);
    }
    snippet
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

pub fn history_before(
    store: &EncryptedStore,
    conversation_id: &ConversationId,
    before_created_at: i64,
    before_id: &MessageId,
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if let Some(canonical) = super::conversations::get_conversation(store, conversation_id)? {
        return history_before_for_identities(
            store,
            &canonical.id,
            &canonical.aliases,
            before_created_at,
            before_id,
            limit,
        );
    }
    history_before_for_identities(
        store,
        conversation_id,
        &[],
        before_created_at,
        before_id,
        limit,
    )
}

pub(crate) fn history_for_conversation(
    store: &EncryptedStore,
    conversation: &bloom_domain::Conversation,
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    history_for_identities(store, &conversation.id, &conversation.aliases, limit)
}

fn history_before_for_identities(
    store: &EncryptedStore,
    canonical_id: &ConversationId,
    aliases: &[ConversationId],
    before_created_at: i64,
    before_id: &MessageId,
    limit: u16,
) -> Result<Vec<Message>, StoreError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let canonical_id = canonical_id.clone();
    let before_id = stable_id(before_id)?;
    let mut identities = vec![canonical_id.clone()];
    identities.extend(aliases.iter().cloned());
    identities.sort_by(|left, right| left.value().cmp(right.value()));
    identities.dedup();

    let mut messages = Vec::new();
    for identity in identities {
        let key = stable_conversation_id(&identity)?;
        let mut statement = store._connection.prepare("SELECT data FROM (SELECT id, data, created_at FROM messages WHERE conversation_id=?1 AND (created_at < ?2 OR (created_at = ?2 AND id < ?3)) ORDER BY created_at DESC, id DESC LIMIT ?4) ORDER BY created_at ASC, id ASC")?;
        let records = statement
            .query_map(params![key, before_created_at, before_id, limit], |row| {
                row.get::<_, Vec<u8>>(0)
            })?;
        for record in records {
            let message: Message =
                serde_json::from_slice(&record?).map_err(|_| StoreError::Schema)?;
            messages.push(remap_message(&message, &canonical_id)?);
        }
    }
    sort_by_store_order(&mut messages)?;
    if messages.len() > limit as usize {
        messages.drain(..messages.len() - limit as usize);
    }
    Ok(messages)
}

/// Read a bounded window centred on one exact stored message. The target is
/// always included when it belongs to the requested conversation (or one of its
/// canonical aliases). Missing targets, cross-conversation targets, and a zero
/// limit return `None`; dispatch rejects zero and out-of-range limits first.
///
/// The older and newer sides are selected with strict `(created_at, stable id)`
/// keyset predicates against the target's currently stored timestamp, so no
/// timestamp arithmetic or synthetic cursor is involved.
pub fn message_context(
    store: &EncryptedStore,
    conversation_id: &ConversationId,
    message_id: &MessageId,
    limit: u16,
) -> Result<Option<Vec<Message>>, StoreError> {
    if limit == 0 {
        return Ok(None);
    }
    let Some(target) = get_message(store, message_id)? else {
        return Ok(None);
    };
    let (canonical_id, aliases) =
        match super::conversations::get_conversation(store, conversation_id)? {
            Some(canonical) => (canonical.id, canonical.aliases),
            None => (conversation_id.clone(), Vec::new()),
        };
    let belongs = target.conversation_id() == &canonical_id
        || aliases
            .iter()
            .any(|alias| alias == target.conversation_id());
    if !belongs {
        return Ok(None);
    }

    let older_quota = (limit as usize - 1) / 2;
    let newer_quota = limit as usize - 1 - older_quota;
    let target_key = stable_id(target.id())?;
    let target_at = target.created_at;

    let mut identities = vec![canonical_id.clone()];
    identities.extend(aliases.iter().cloned());
    identities.sort_by(|left, right| left.value().cmp(right.value()));
    identities.dedup();

    let mut older = Vec::new();
    let mut newer = Vec::new();
    for identity in identities {
        let key = stable_conversation_id(&identity)?;
        if older_quota > 0 {
            let mut statement = store._connection.prepare("SELECT data FROM (SELECT id, data, created_at FROM messages WHERE conversation_id=?1 AND (created_at < ?2 OR (created_at = ?2 AND id < ?3)) ORDER BY created_at DESC, id DESC LIMIT ?4) ORDER BY created_at ASC, id ASC")?;
            let records = statement.query_map(
                params![key, target_at, target_key, older_quota as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for record in records {
                let message: Message =
                    serde_json::from_slice(&record?).map_err(|_| StoreError::Schema)?;
                older.push(remap_message(&message, &canonical_id)?);
            }
        }
        if newer_quota > 0 {
            let mut statement = store._connection.prepare("SELECT data FROM (SELECT id, data, created_at FROM messages WHERE conversation_id=?1 AND (created_at > ?2 OR (created_at = ?2 AND id > ?3)) ORDER BY created_at ASC, id ASC LIMIT ?4) ORDER BY created_at ASC, id ASC")?;
            let records = statement.query_map(
                params![key, target_at, target_key, newer_quota as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )?;
            for record in records {
                let message: Message =
                    serde_json::from_slice(&record?).map_err(|_| StoreError::Schema)?;
                newer.push(remap_message(&message, &canonical_id)?);
            }
        }
    }

    // Each identity contributed its closest quota rows. Merge across aliases and
    // keep the closest rows to the target: the tail of the older set and the head
    // of the newer set.
    sort_by_store_order(&mut older)?;
    if older.len() > older_quota {
        older.drain(..older.len() - older_quota);
    }
    sort_by_store_order(&mut newer)?;
    if newer.len() > newer_quota {
        newer.truncate(newer_quota);
    }

    let mut messages = older;
    messages.push(remap_message(&target, &canonical_id)?);
    messages.extend(newer);
    Ok(Some(messages))
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
    sort_by_store_order(&mut messages)?;
    if messages.len() > limit as usize {
        messages.drain(..messages.len() - limit as usize);
    }
    Ok(messages)
}

/// Orders a merged page exactly the way the store does. The SQL index orders by the
/// stable JSON encoding of the id, so a merge that compared raw id values would
/// disagree with the keyset predicate for ids containing escaped characters, and a
/// page boundary inside a `created_at` tie could repeat or skip a message.
fn sort_by_store_order(messages: &mut [Message]) -> Result<(), StoreError> {
    let mut keyed = Vec::with_capacity(messages.len());
    for message in messages.iter() {
        keyed.push((stable_id(message.id())?, message.created_at));
    }
    let mut order: Vec<usize> = (0..messages.len()).collect();
    order.sort_by(|left, right| {
        keyed[*left]
            .1
            .cmp(&keyed[*right].1)
            .then_with(|| keyed[*left].0.cmp(&keyed[*right].0))
    });
    if order
        .iter()
        .enumerate()
        .all(|(index, source)| index == *source)
    {
        return Ok(());
    }
    let mut slots: Vec<Option<Message>> = messages.iter().cloned().map(Some).collect();
    for (target, source) in order.into_iter().enumerate() {
        if let Some(message) = slots[source].take() {
            messages[target] = message;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::KEY_BYTES;
    use bloom_domain::{
        Conversation, ConversationKind, DeliveryState, Handle, HandleKind, MessagePart,
        Participant, TransportId,
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

    fn message_id(transport: &str, value: &str) -> MessageId {
        MessageId::new(TransportId::new(transport).unwrap(), value).unwrap()
    }

    fn fixture_message_parts(
        message: &str,
        conversation: &str,
        at: i64,
        parts: Vec<MessagePart>,
    ) -> Message {
        let transport = TransportId::new("alpha").unwrap();
        Message::new(
            MessageId::new(transport.clone(), message).unwrap(),
            ConversationId::new(transport, conversation).unwrap(),
            None,
            Handle::new(HandleKind::Opaque, "synthetic-sender").unwrap(),
            parts,
            at,
            None,
            DeliveryState::Sent,
            false,
            None,
            vec![],
            None,
        )
        .unwrap()
    }

    #[test]
    fn search_matches_case_insensitively_and_ignores_non_text_parts() {
        let (_temp, mut store, conversation) = setup();
        for message in [
            fixture_message(
                "alpha",
                "m1",
                "room",
                1,
                "HeLLo NeEdLe",
                DeliveryState::Sent,
            ),
            fixture_message_parts(
                "m2",
                "room",
                2,
                vec![MessagePart::Unsupported {
                    kind: "needle".into(),
                }],
            ),
        ] {
            upsert_message(&mut store, &message).unwrap();
        }

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id.value(), "m1");
        assert_eq!(hits[0].conversation_id, conversation);
        assert_eq!(hits[0].snippet, "HeLLo NeEdLe");
    }

    #[test]
    fn search_empty_or_whitespace_query_returns_nothing() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "m1", "room", 1, "needle", DeliveryState::Sent),
        )
        .unwrap();
        assert!(search_messages(&store, "", 10).unwrap().is_empty());
        assert!(search_messages(&store, "   \t\n", 10).unwrap().is_empty());
        assert!(search_messages(&store, "needle", 0).unwrap().is_empty());
    }

    #[test]
    fn search_snippet_marks_only_the_side_it_truncates() {
        let (_temp, mut store, _conversation) = setup();
        let tail = "abcdefghijklmnopqrstuvwxyz0123456789";
        let head_text = format!("MATCH{tail}");
        let tail_text = format!("{tail}MATCH");
        upsert_message(
            &mut store,
            &fixture_message("alpha", "head", "room", 2, &head_text, DeliveryState::Sent),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "tail", "room", 1, &tail_text, DeliveryState::Sent),
        )
        .unwrap();

        let hits = search_messages(&store, "match", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].snippet,
            format!("MATCH{}…", &tail[..30]),
            "a match at the start has no leading ellipsis"
        );
        assert_eq!(
            hits[1].snippet,
            format!("…{}MATCH", &tail[tail.len() - 30..]),
            "a match at the end has no trailing ellipsis"
        );
    }

    #[test]
    fn search_snippet_uses_original_offsets_when_lowercasing_expands() {
        let (_temp, mut store, _conversation) = setup();
        // U+0130 lowercases to two chars, so lowered byte offsets no longer match
        // the original string; the snippet must still slice the real match.
        let text = format!("{}needle{}", "\u{0130}".repeat(40), "tail".repeat(10));
        upsert_message(
            &mut store,
            &fixture_message("alpha", "expand", "room", 1, &text, DeliveryState::Sent),
        )
        .unwrap();

        let hits = search_messages(&store, "NEEDLE", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("needle"),
            "the snippet must contain the matched run: {:?}",
            hits[0].snippet
        );
        assert!(hits[0].snippet.starts_with('…'));
        assert!(hits[0].snippet.ends_with('…'));
    }

    #[test]
    fn search_snippet_has_context_and_ellipses() {
        let (_temp, mut store, _conversation) = setup();
        let prefix = "abcdefghijklmnopqrstuvwxyz0123456789";
        let suffix = "abcdefghijklmnopqrstuvwxyz0123456789";
        let text = format!("{prefix}MATCH{suffix}");
        upsert_message(
            &mut store,
            &fixture_message("alpha", "m1", "room", 1, &text, DeliveryState::Sent),
        )
        .unwrap();

        let hits = search_messages(&store, "match", 10).unwrap();
        assert_eq!(
            hits[0].snippet,
            "\u{2026}ghijklmnopqrstuvwxyz0123456789MATCHabcdefghijklmnopqrstuvwxyz0123\u{2026}"
        );
    }

    #[test]
    fn search_snippet_replaces_newlines_with_spaces() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "m1",
                "room",
                1,
                "line one\nneedle\r\nline three",
                DeliveryState::Sent,
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits[0].snippet, "line one needle line three");
    }

    #[test]
    fn search_snippet_is_char_boundary_safe_for_multibyte_text() {
        let (_temp, mut store, _conversation) = setup();
        let text = format!(
            "{}\u{e9}needle\u{e9}{}",
            "\u{e9}".repeat(40),
            "\u{e9}".repeat(40)
        );
        upsert_message(
            &mut store,
            &fixture_message("alpha", "m1", "room", 1, &text, DeliveryState::Sent),
        )
        .unwrap();

        let hits = search_messages(&store, "NEEDLE", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(snippet.starts_with('\u{2026}'), "{snippet}");
        assert!(snippet.ends_with('\u{2026}'), "{snippet}");
        assert!(snippet.contains("needle"), "{snippet}");
        assert_eq!(
            snippet
                .chars()
                .filter(|character| *character == '\u{e9}')
                .count(),
            60,
            "30 context characters on each side"
        );
    }

    #[test]
    fn search_respects_limit_and_orders_newest_first_without_duplicates() {
        let (_temp, mut store, _conversation) = setup();
        for at in 1..=5 {
            let message = fixture_message(
                "alpha",
                &format!("m{at}"),
                "room",
                at,
                "needle",
                DeliveryState::Sent,
            );
            upsert_message(&mut store, &message).unwrap();
        }
        // Two matching text parts in one message must still produce one hit.
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "multi",
                "room",
                6,
                vec![
                    MessagePart::Text("needle one".into()),
                    MessagePart::Text("needle two".into()),
                ],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 3).unwrap();
        assert_eq!(
            hits.iter()
                .map(|hit| hit.message_id.value())
                .collect::<Vec<_>>(),
            vec!["multi", "m5", "m4"]
        );
        let all = search_messages(&store, "needle", 10).unwrap();
        let ids: Vec<_> = all.iter().map(|hit| hit.message_id.value()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn search_reports_canonical_conversation_for_alias() {
        let (_temp, mut store, conversation) = setup();
        let alias = ConversationId::new(TransportId::new("alpha").unwrap(), "legacy").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: alias.clone(),
                remote_id: "legacy".into(),
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
                aliases: vec![alias],
                preview: None,
            },
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "a1", "legacy", 1, "needle", DeliveryState::Sent),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, conversation);
    }

    #[test]
    fn filtered_search_finds_a_match_behind_more_than_fifty_newer_global_hits() {
        let (_temp, mut store, conversation) = setup();
        let friend = participant(HandleKind::Email, "friend@example.invalid", Some("Friend"));
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "room",
                ConversationKind::Direct,
                vec![friend.clone()],
                Vec::new(),
            ),
        )
        .unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "elsewhere",
                ConversationKind::Direct,
                Vec::new(),
                Vec::new(),
            ),
        )
        .unwrap();
        for at in 10..=60 {
            upsert_message(
                &mut store,
                &fixture_message(
                    "alpha",
                    &format!("newer-{at}"),
                    "elsewhere",
                    at,
                    "needle elsewhere",
                    DeliveryState::Sent,
                ),
            )
            .unwrap();
        }
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "hidden",
                "room",
                1,
                "needle in the scoped chat",
                DeliveryState::Sent,
            ),
        )
        .unwrap();

        let unfiltered = search_messages(&store, "needle", 1).unwrap();
        assert_eq!(unfiltered[0].message_id.value(), "newer-60");
        assert_eq!(
            unfiltered[0].snippet, "needle elsewhere",
            "unfiltered snippets stay unchanged"
        );

        let newest_fifty = search_messages(&store, "needle", 50).unwrap();
        assert_eq!(newest_fifty.len(), 50);
        assert_eq!(newest_fifty.first().unwrap().message_id.value(), "newer-60");
        assert_eq!(newest_fifty.last().unwrap().message_id.value(), "newer-11");
        assert!(newest_fifty
            .iter()
            .all(|hit| hit.message_id.value() != "hidden"));

        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        for limit in [1, 50] {
            let hits =
                search_messages_with_participant(&store, "needle", limit, Some(&filter)).unwrap();
            assert_eq!(hits.len(), 1, "limit {limit} counts only in-scope hits");
            assert_eq!(hits[0].message_id.value(), "hidden");
            assert_eq!(hits[0].conversation_id, conversation);
            assert_eq!(hits[0].snippet, "needle in the scoped chat");
        }
    }

    #[test]
    fn participant_and_unfiltered_search_share_the_global_scan_boundary() {
        let (_temp, mut store, _) = setup();
        let friend = participant(HandleKind::Email, "friend@example.invalid", None);
        for (id, people) in [("room", vec![friend.clone()]), ("elsewhere", Vec::new())] {
            super::super::conversations::upsert_conversation(
                &mut store,
                &scoped_conversation("alpha", id, ConversationKind::Direct, people, Vec::new()),
            )
            .unwrap();
        }
        for (id, at) in [("outside-bound", 0), ("at-bound", 1)] {
            upsert_message(
                &mut store,
                &fixture_message("alpha", id, "room", at, "needle", DeliveryState::Sent),
            )
            .unwrap();
        }
        for index in 0..SEARCH_SCAN_LIMIT - 1 {
            upsert_message(
                &mut store,
                &fixture_message(
                    "alpha",
                    &format!("recent-{index}"),
                    "elsewhere",
                    index as i64 + 2,
                    "haystack",
                    DeliveryState::Sent,
                ),
            )
            .unwrap();
        }
        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        for scope in [None, Some(&filter)] {
            let hits = search_messages_with_participant(&store, "needle", 50, scope).unwrap();
            assert_eq!(
                hits.len(),
                1,
                "membership must not move the global scan boundary"
            );
            assert_eq!(hits[0].message_id.value(), "at-bound");
        }
    }

    #[test]
    fn filter_matches_exact_transport_handle_kind_and_value_ignoring_labels() {
        let (_temp, mut store, _conversation) = setup();
        for (transport, value, kind) in [
            ("alpha", "email-room", HandleKind::Email),
            ("alpha", "tel-room", HandleKind::Tel),
            ("alpha", "opaque-room", HandleKind::Opaque),
            ("beta", "email-room", HandleKind::Email),
        ] {
            super::super::conversations::upsert_conversation(
                &mut store,
                &scoped_conversation(
                    transport,
                    value,
                    ConversationKind::Direct,
                    vec![participant(kind, "same-value", Some("Duplicate label"))],
                    Vec::new(),
                ),
            )
            .unwrap();
        }
        for (transport, value, at) in [
            ("alpha", "email-room", 1),
            ("alpha", "tel-room", 2),
            ("alpha", "opaque-room", 3),
            ("beta", "email-room", 4),
        ] {
            upsert_message(
                &mut store,
                &fixture_message(
                    transport,
                    &format!("m-{transport}-{value}"),
                    value,
                    at,
                    "needle",
                    DeliveryState::Sent,
                ),
            )
            .unwrap();
        }

        for (transport, kind, expected) in [
            ("alpha", HandleKind::Email, Some("email-room")),
            ("alpha", HandleKind::Tel, Some("tel-room")),
            ("alpha", HandleKind::Opaque, Some("opaque-room")),
            ("beta", HandleKind::Email, Some("email-room")),
            ("alpha-other", HandleKind::Email, None),
        ] {
            let filter = ParticipantFilter {
                transport: TransportId::new(transport).unwrap(),
                handle: Handle::new(kind, "same-value").unwrap(),
            };
            let hits =
                search_messages_with_participant(&store, "needle", 10, Some(&filter)).unwrap();
            match expected {
                Some(expected) => {
                    assert_eq!(hits.len(), 1, "{transport} {expected}");
                    assert_eq!(hits[0].conversation_id.transport().as_str(), transport);
                    assert_eq!(hits[0].conversation_id.value(), expected);
                }
                None => assert!(hits.is_empty(), "a different transport must not match"),
            }
        }

        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: Handle::new(HandleKind::Email, "different-value").unwrap(),
        };
        assert!(
            search_messages_with_participant(&store, "needle", 10, Some(&filter))
                .unwrap()
                .is_empty(),
            "a different handle value must not match"
        );
    }

    #[test]
    fn group_participant_matches_regardless_of_message_sender() {
        let (_temp, mut store, _conversation) = setup();
        let friend = participant(HandleKind::Email, "member@example.invalid", Some("Member"));
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "group",
                ConversationKind::Group,
                vec![
                    friend.clone(),
                    participant(HandleKind::Opaque, "someone-else", Some("Other")),
                ],
                Vec::new(),
            ),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "g1",
                "group",
                1,
                "needle from another sender",
                DeliveryState::Sent,
            ),
        )
        .unwrap();

        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        let hits = search_messages_with_participant(&store, "needle", 10, Some(&filter)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id.value(), "group");
    }

    #[test]
    fn alias_messages_use_current_canonical_membership_not_retired_metadata() {
        let (_temp, mut store, _conversation) = setup();
        let friend = participant(HandleKind::Email, "friend@example.invalid", Some("Friend"));
        let good_alias =
            ConversationId::new(TransportId::new("alpha").unwrap(), "legacy-good").unwrap();
        let bad_alias =
            ConversationId::new(TransportId::new("alpha").unwrap(), "legacy-bad").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "canonical",
                ConversationKind::Direct,
                vec![friend.clone()],
                vec![good_alias.clone()],
            ),
        )
        .unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "legacy-good",
                ConversationKind::Direct,
                Vec::new(),
                Vec::new(),
            ),
        )
        .unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "canonical-without-member",
                ConversationKind::Direct,
                Vec::new(),
                vec![bad_alias.clone()],
            ),
        )
        .unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "legacy-bad",
                ConversationKind::Direct,
                vec![friend.clone()],
                Vec::new(),
            ),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "via-alias",
                "legacy-good",
                2,
                "needle through the alias",
                DeliveryState::Sent,
            ),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "retired-only",
                "legacy-bad",
                1,
                "needle only in retired metadata",
                DeliveryState::Sent,
            ),
        )
        .unwrap();

        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        let hits = search_messages_with_participant(&store, "needle", 10, Some(&filter)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id.value(), "via-alias");
        assert_eq!(
            hits[0].conversation_id.value(),
            "canonical",
            "the alias message reports its canonical conversation"
        );
    }

    #[test]
    fn filtered_search_returns_empty_for_missing_metadata_or_no_body_match() {
        let (_temp, mut store, _conversation) = setup();
        // Inject an orphan only in this synthetic test. Normal writes enforce the
        // FK; filtering must still fail closed if imported/corrupt data lacks metadata.
        store
            ._connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "unlisted",
                "unlisted",
                2,
                "needle without conversation metadata",
                DeliveryState::Sent,
            ),
        )
        .unwrap();
        store
            ._connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        let friend = participant(HandleKind::Email, "friend@example.invalid", Some("Friend"));
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "room",
                ConversationKind::Direct,
                vec![friend.clone()],
                Vec::new(),
            ),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "room-body",
                "room",
                1,
                "different body",
                DeliveryState::Sent,
            ),
        )
        .unwrap();

        let filter = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        assert!(
            search_messages_with_participant(&store, "needle", 10, Some(&filter))
                .unwrap()
                .is_empty(),
            "missing metadata yields no hits rather than unfiltered results"
        );
        assert!(
            search_messages_with_participant(&store, "absent", 10, Some(&filter))
                .unwrap()
                .is_empty()
        );
        let unknown = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: Handle::new(HandleKind::Email, "unknown@example.invalid").unwrap(),
        };
        assert!(
            search_messages_with_participant(&store, "different", 10, Some(&unknown))
                .unwrap()
                .is_empty(),
            "no matching conversation yields no hits"
        );
    }

    fn participant(kind: HandleKind, value: &str, display_name: Option<&str>) -> Participant {
        Participant {
            handle: Handle::new(kind, value).unwrap(),
            display_name: display_name.map(str::to_owned),
            is_me: false,
        }
    }

    fn scoped_conversation(
        transport: &str,
        value: &str,
        kind: ConversationKind,
        participants: Vec<Participant>,
        aliases: Vec<ConversationId>,
    ) -> Conversation {
        Conversation {
            id: ConversationId::new(TransportId::new(transport).unwrap(), value).unwrap(),
            remote_id: value.into(),
            kind,
            title: None,
            participants,
            created_at: 0,
            last_message_at: None,
            aliases,
            preview: None,
        }
    }

    #[test]
    fn history_before_pages_without_gaps_or_duplicates() {
        let (_temp, mut store, conversation) = setup();
        for at in 1..=6 {
            let message = fixture_message(
                "alpha",
                &format!("m{at}"),
                "room",
                at,
                "body",
                DeliveryState::Sent,
            );
            upsert_message(&mut store, &message).unwrap();
        }

        let newest =
            history_before(&store, &conversation, 7, &message_id("alpha", "m7"), 3).unwrap();
        assert_eq!(
            newest
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["m4", "m5", "m6"]
        );

        let oldest_loaded = newest.first().unwrap();
        let older = history_before(
            &store,
            &conversation,
            oldest_loaded.created_at,
            oldest_loaded.id(),
            3,
        )
        .unwrap();
        assert_eq!(
            older
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["m1", "m2", "m3"]
        );

        let ids: Vec<_> = older
            .iter()
            .chain(newest.iter())
            .map(|message| message.id().value())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "m3", "m4", "m5", "m6"]);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn history_order_matches_the_keyset_order_for_escaped_ids() {
        // Raw id text and its JSON encoding sort differently once a value contains a
        // quote: raw `a"x` is below `a*x`, but the encoded form `a\\"x` is above
        // `a*x`. The store indexes and compares the encoded form, so a page whose
        // boundary sits inside a `created_at` tie must be ordered the same way or the
        // next cursor re-serves a message that was already delivered.
        let (_temp, mut store, conversation) = setup();
        let older = fixture_message("alpha", "old", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &older).unwrap();
        for value in ["a\"x", "a*x"] {
            let message = fixture_message("alpha", value, "room", 10, "body", DeliveryState::Sent);
            upsert_message(&mut store, &message).unwrap();
        }

        let newest =
            history_before(&store, &conversation, 20, &message_id("alpha", "after"), 2).unwrap();
        assert_eq!(
            newest
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["a*x", "a\"x"],
            "the merged page follows the store's encoded ordering"
        );

        let oldest_loaded = newest.first().unwrap();
        let older_page = history_before(
            &store,
            &conversation,
            oldest_loaded.created_at,
            oldest_loaded.id(),
            2,
        )
        .unwrap();
        assert_eq!(
            older_page
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["old"],
            "no already-delivered message reappears across the tie boundary"
        );
    }

    #[test]
    fn history_before_splits_created_at_ties_by_id() {
        let (_temp, mut store, conversation) = setup();
        let earlier = fixture_message("alpha", "z", "room", 5, "body", DeliveryState::Sent);
        upsert_message(&mut store, &earlier).unwrap();
        for value in ["a", "b", "c", "d", "e"] {
            let message = fixture_message("alpha", value, "room", 10, "body", DeliveryState::Sent);
            upsert_message(&mut store, &message).unwrap();
        }
        let later = fixture_message("alpha", "y", "room", 20, "body", DeliveryState::Sent);
        upsert_message(&mut store, &later).unwrap();

        let middle =
            history_before(&store, &conversation, 10, &message_id("alpha", "c"), 10).unwrap();
        assert_eq!(
            middle
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["z", "a", "b"]
        );
        let middle_limited =
            history_before(&store, &conversation, 10, &message_id("alpha", "c"), 2).unwrap();
        assert_eq!(
            middle_limited
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        let mut cursor_at = 21;
        let mut cursor_id = message_id("alpha", "zzz");
        let mut pages = Vec::new();
        // Bound the loop: a cursor that repeats a page must fail the coverage
        // assertion instead of looping forever.
        for _ in 0..8 {
            let page = history_before(&store, &conversation, cursor_at, &cursor_id, 2).unwrap();
            let Some(oldest) = page.first() else {
                break;
            };
            cursor_at = oldest.created_at;
            cursor_id = oldest.id().clone();
            pages.push(page);
        }
        pages.reverse();
        let ids: Vec<_> = pages
            .iter()
            .flatten()
            .map(|message| message.id().value())
            .collect();
        assert_eq!(ids, vec!["z", "a", "b", "c", "d", "e", "y"]);
    }

    #[test]
    fn history_before_remaps_alias_across_page_boundary() {
        let (_temp, mut store, conversation) = setup();
        let alias = ConversationId::new(TransportId::new("alpha").unwrap(), "legacy").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: alias.clone(),
                remote_id: "legacy".into(),
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
                aliases: vec![alias.clone()],
                preview: None,
            },
        )
        .unwrap();

        let messages = [
            fixture_message("alpha", "c1", "room", 1, "body", DeliveryState::Sent),
            fixture_message("alpha", "a1", "legacy", 2, "body", DeliveryState::Sent),
            fixture_message("alpha", "c2", "room", 3, "body", DeliveryState::Sent),
            fixture_message("alpha", "a2", "legacy", 4, "body", DeliveryState::Sent),
        ];
        for message in &messages {
            upsert_message(&mut store, message).unwrap();
        }

        let newest =
            history_before(&store, &conversation, 5, &message_id("alpha", "zzz"), 2).unwrap();
        assert_eq!(
            newest
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["c2", "a2"]
        );
        let oldest_loaded = newest.first().unwrap();
        let older = history_before(
            &store,
            &conversation,
            oldest_loaded.created_at,
            oldest_loaded.id(),
            2,
        )
        .unwrap();
        assert_eq!(
            older
                .iter()
                .map(|message| message.id().value())
                .collect::<Vec<_>>(),
            vec!["c1", "a1"]
        );
        for message in older.iter().chain(newest.iter()) {
            assert_eq!(message.conversation_id(), &conversation);
        }
    }

    #[test]
    fn history_before_empty_cursor_and_zero_limit() {
        let (_temp, mut store, conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 10, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        assert!(history_before(&store, &conversation, 0, message.id(), 5)
            .unwrap()
            .is_empty());
        assert!(history_before(&store, &conversation, 20, message.id(), 0)
            .unwrap()
            .is_empty());
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

    fn ids(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.id().value())
            .collect()
    }

    #[test]
    fn message_context_includes_old_target_in_long_history() {
        let (_temp, mut store, conversation) = setup();
        for index in 0..2000 {
            let message = fixture_message(
                "alpha",
                &format!("m{index:04}"),
                "room",
                1_000 + index,
                "body",
                DeliveryState::Sent,
            );
            upsert_message(&mut store, &message).unwrap();
        }
        let target = message_id("alpha", "m0007");
        let context = message_context(&store, &conversation, &target, 5)
            .unwrap()
            .expect("target exists");
        assert_eq!(ids(&context), ["m0005", "m0006", "m0007", "m0008", "m0009"]);
        let far = message_id("alpha", "m1420");
        let context = message_context(&store, &conversation, &far, 5)
            .unwrap()
            .unwrap();
        assert_eq!(ids(&context), ["m1418", "m1419", "m1420", "m1421", "m1422"]);
        let context = message_context(&store, &conversation, &target, 2)
            .unwrap()
            .expect("target exists");
        assert_eq!(ids(&context), ["m0007", "m0008"]);
    }

    #[test]
    fn message_context_sorts_escaped_same_timestamp_neighbours_by_store_key() {
        let (_temp, mut store, conversation) = setup();
        let suffixes = ["\"", "\\", "\u{e9}", "}"];
        for index in 0..250 {
            let value = format!("m{index:03}{}", suffixes[index % suffixes.len()]);
            let message = fixture_message(
                "alpha",
                &value,
                "room",
                i64::MAX,
                "body",
                DeliveryState::Sent,
            );
            upsert_message(&mut store, &message).unwrap();
        }
        let target_index = 124;
        let target_value = format!(
            "m{target_index:03}{}",
            suffixes[target_index % suffixes.len()]
        );
        let target = message_id("alpha", &target_value);
        let context = message_context(&store, &conversation, &target, 200)
            .unwrap()
            .unwrap();
        assert_eq!(context.len(), 200);
        let position = context
            .iter()
            .position(|message| message.id().value() == target_value)
            .expect("exact target is present");
        assert_eq!(position, 99, "older side gets (200-1)/2 slots");

        let mut all: Vec<(String, i64)> = context
            .iter()
            .map(|message| (stable_id(message.id()).unwrap(), message.created_at))
            .collect();
        all.sort();
        let mut expected: Vec<(String, i64)> = (0..250)
            .map(|index| {
                let value = format!("m{index:03}{}", suffixes[index % suffixes.len()]);
                (stable_id(&message_id("alpha", &value)).unwrap(), i64::MAX)
            })
            .collect();
        expected.sort();
        let target_key = stable_id(&target).unwrap();
        let target_position = expected
            .iter()
            .position(|(key, _)| key == &target_key)
            .unwrap();
        let start = target_position - 99;
        assert_eq!(all, expected[start..start + 200]);
    }

    #[test]
    fn message_context_limit_one_and_zero_are_well_defined() {
        let (_temp, mut store, conversation) = setup();
        for index in 0..5 {
            upsert_message(
                &mut store,
                &fixture_message(
                    "alpha",
                    &format!("m{index}"),
                    "room",
                    10 + index,
                    "body",
                    DeliveryState::Sent,
                ),
            )
            .unwrap();
        }
        let target = message_id("alpha", "m2");
        let one = message_context(&store, &conversation, &target, 1)
            .unwrap()
            .unwrap();
        assert_eq!(ids(&one), ["m2"]);
        assert!(message_context(&store, &conversation, &target, 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn message_context_missing_and_wrong_conversation_are_none() {
        let (_temp, mut store, conversation) = setup();
        let other = ConversationId::new(TransportId::new("alpha").unwrap(), "other").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: other.clone(),
                remote_id: "other".into(),
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
        upsert_message(
            &mut store,
            &fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "x1", "other", 2, "body", DeliveryState::Sent),
        )
        .unwrap();
        let target = message_id("alpha", "m1");
        assert!(
            message_context(&store, &conversation, &message_id("alpha", "missing"), 5)
                .unwrap()
                .is_none()
        );
        assert!(message_context(&store, &other, &target, 5)
            .unwrap()
            .is_none());
    }

    #[test]
    fn message_context_resolves_aliases_and_remaps_results() {
        let (_temp, mut store, conversation) = setup();
        let alias = ConversationId::new(TransportId::new("alpha").unwrap(), "legacy").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &Conversation {
                id: alias.clone(),
                remote_id: "legacy".into(),
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
                aliases: vec![alias.clone()],
                preview: None,
            },
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "a1", "legacy", 1, "one", DeliveryState::Sent),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "c2", "room", 2, "two", DeliveryState::Sent),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message("alpha", "a3", "legacy", 3, "three", DeliveryState::Sent),
        )
        .unwrap();
        let target = message_id("alpha", "a1");
        for requested in [&conversation, &alias] {
            let context = message_context(&store, requested, &target, 4)
                .unwrap()
                .unwrap();
            assert_eq!(ids(&context), ["a1", "c2", "a3"]);
            assert!(context
                .iter()
                .all(|message| message.conversation_id() == &conversation));
        }
    }
}
