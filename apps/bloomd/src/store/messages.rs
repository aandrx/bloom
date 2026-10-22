use super::{EncryptedStore, StoreError};
use bloom_domain::{ConversationId, Message, MessageId, MessagePart};
use rusqlite::{params, OptionalExtension};
use std::collections::HashSet;

pub use bloom_ipc::MessageSearchHit;

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
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let query_lower = query.to_lowercase();
    if query_lower.is_empty() {
        return Ok(Vec::new());
    }

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
}
