use super::{EncryptedStore, StoreError};
use bloom_domain::{ConversationId, Handle, Message, MessageId, MessagePart, Reaction};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionOutcome {
    Applied,
    Unchanged,
    NotFound,
}

/// Result of a transactional edit or unsend application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationOutcome {
    /// The stored message changed.
    Applied,
    /// The event was a duplicate or strictly older than the stored state.
    Unchanged,
    /// No message row exists for the target.
    NotFound,
    /// The target cannot be mutated safely with the stored data.
    Unsupported,
}

/// Identity, ownership, part and timestamp shared by edit and unsend application.
pub struct MutationTarget<'a> {
    pub message_id: &'a MessageId,
    pub conversation_id: &'a ConversationId,
    pub aliases: &'a [ConversationId],
    pub sender: &'a Handle,
    pub part: u64,
    pub at: i64,
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

/// Applies one reaction change to the stored message, touching only its
/// `reactions` field. The read and optional write share one transaction so a
/// concurrent upsert cannot interleave between them.
pub fn apply_reaction(
    store: &mut EncryptedStore,
    target: &MessageId,
    reaction: &Reaction,
    active: bool,
) -> Result<ReactionOutcome, StoreError> {
    let key = stable_id(target)?;
    let tx = store._connection.transaction()?;
    let data: Option<Vec<u8>> = tx
        .query_row(
            "SELECT data FROM messages WHERE id=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(data) = data else {
        return Ok(ReactionOutcome::NotFound);
    };
    let mut message: Message = serde_json::from_slice(&data).map_err(|_| StoreError::Schema)?;
    let matches = |existing: &Reaction| {
        existing.sender == reaction.sender
            && existing.kind == reaction.kind
            && existing.part == reaction.part
    };
    let changed = if active {
        if message.reactions.iter().any(matches) {
            false
        } else {
            message.reactions.push(reaction.clone());
            true
        }
    } else {
        let before = message.reactions.len();
        message.reactions.retain(|existing| !matches(existing));
        message.reactions.len() != before
    };
    if !changed {
        return Ok(ReactionOutcome::Unchanged);
    }
    message.reactions.sort_by(|left, right| {
        let time = |reaction: &Reaction| {
            let timestamp = reaction.timestamp;
            (timestamp.is_none(), timestamp.unwrap_or_default())
        };
        time(left)
            .cmp(&time(right))
            .then_with(|| left.kind.cmp(&right.kind))
            // Handle's Debug is redacted, so compare its serialized form, which
            // includes the kind and value and is total.
            .then_with(|| {
                let key = |reaction: &Reaction| {
                    serde_json::to_string(&reaction.sender).unwrap_or_default()
                };
                key(left).cmp(&key(right))
            })
            .then_with(|| left.part.cmp(&right.part))
    });
    let updated = serde_json::to_vec(&message).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "UPDATE messages SET data=?2 WHERE id=?1",
        params![key, updated],
    )?;
    tx.commit()?;
    Ok(ReactionOutcome::Applied)
}

impl EncryptedStore {
    /// Applies a reaction and returns the reloaded stored message only when the
    /// reaction changed it. `None` covers a duplicate delivery and an absent target.
    pub fn apply_reaction(
        &mut self,
        target: &MessageId,
        reaction: &Reaction,
        active: bool,
    ) -> Result<Option<Message>, StoreError> {
        if self::apply_reaction(self, target, reaction, active)? != ReactionOutcome::Applied {
            return Ok(None);
        }
        get_message(self, target)
    }
}

enum MutationLookup {
    Ready(Box<Message>, Vec<Option<u64>>),
    Rejected(MutationOutcome),
}

/// Load and validate the stored mutation target inside the caller's transaction.
/// A missing row is `NotFound`; every unsafe or ambiguous shape is `Unsupported`.
fn load_mutation_target(
    tx: &rusqlite::Transaction<'_>,
    target: &MessageId,
    conversation: &ConversationId,
    aliases: &[ConversationId],
    sender: &Handle,
) -> Result<MutationLookup, StoreError> {
    let key = stable_id(target)?;
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
        .query_row(
            "SELECT messages.data, message_part_maps.data FROM messages
             LEFT JOIN message_part_maps ON message_part_maps.message_id = messages.id
             WHERE messages.id=?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((data, map)) = row else {
        return Ok(MutationLookup::Rejected(MutationOutcome::NotFound));
    };
    let message: Message = serde_json::from_slice(&data).map_err(|_| StoreError::Schema)?;
    if message.conversation_id() != conversation && !aliases.contains(message.conversation_id()) {
        return Ok(MutationLookup::Rejected(MutationOutcome::Unsupported));
    }
    if &message.sender != sender {
        return Ok(MutationLookup::Rejected(MutationOutcome::Unsupported));
    }
    let Some(map) = map else {
        return Ok(MutationLookup::Rejected(MutationOutcome::Unsupported));
    };
    let part_ids: Vec<Option<u64>> =
        serde_json::from_slice(&map).map_err(|_| StoreError::Schema)?;
    if part_ids.len() != message.parts.len() {
        return Ok(MutationLookup::Rejected(MutationOutcome::Unsupported));
    }
    Ok(MutationLookup::Ready(Box::new(message), part_ids))
}

/// Write the mutated message and its aligned part map in the caller's transaction.
fn write_mutation(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    message: &Message,
    part_ids: &[Option<u64>],
) -> Result<(), StoreError> {
    let data = serde_json::to_vec(message).map_err(|_| StoreError::Schema)?;
    let map = serde_json::to_vec(part_ids).map_err(|_| StoreError::Schema)?;
    tx.execute(
        "UPDATE messages SET data=?2 WHERE id=?1",
        params![key, data],
    )?;
    tx.execute(
        "UPDATE message_part_maps SET data=?2 WHERE message_id=?1",
        params![key, map],
    )?;
    Ok(())
}

/// Applies one edit event to a stored message. Every stored part mapped to the
/// target part is replaced, in place, by the replacement text parts, and the
/// replacement parts take over that target map identity. All other parts and
/// fields are preserved. The read and write share one transaction.
pub fn apply_edit(
    store: &mut EncryptedStore,
    target: &MutationTarget<'_>,
    replacements: &[MessagePart],
) -> Result<MutationOutcome, StoreError> {
    if replacements.is_empty()
        || replacements
            .iter()
            .any(|part| !matches!(part, MessagePart::Text(text) if !text.is_empty()))
    {
        return Ok(MutationOutcome::Unsupported);
    }
    let key = stable_id(target.message_id)?;
    let tx = store._connection.transaction()?;
    let (message, part_ids) = match load_mutation_target(
        &tx,
        target.message_id,
        target.conversation_id,
        target.aliases,
        target.sender,
    )? {
        MutationLookup::Ready(message, part_ids) => (message, part_ids),
        MutationLookup::Rejected(outcome) => return Ok(outcome),
    };
    let mut message = *message;
    if message.unsent_at.is_some() {
        return Ok(MutationOutcome::Unsupported);
    }
    let original_at = message.sent_at.unwrap_or(message.created_at);
    if target.at < original_at {
        return Ok(MutationOutcome::Unsupported);
    }
    if !part_ids.contains(&Some(target.part)) {
        return Ok(MutationOutcome::Unsupported);
    }
    if message
        .edited_at
        .is_some_and(|previous| target.at <= previous)
    {
        return Ok(MutationOutcome::Unchanged);
    }
    let mut parts = Vec::with_capacity(message.parts.len());
    let mut ids = Vec::with_capacity(part_ids.len());
    let mut replaced = false;
    for (index, stored) in message.parts.iter().enumerate() {
        if part_ids[index] == Some(target.part) {
            if !replaced {
                for replacement in replacements {
                    parts.push(replacement.clone());
                    ids.push(Some(target.part));
                }
                replaced = true;
            }
        } else {
            parts.push(stored.clone());
            ids.push(part_ids[index]);
        }
    }
    message.parts = parts;
    message.edited_at = Some(target.at);
    write_mutation(&tx, &key, &message, &ids)?;
    tx.commit()?;
    Ok(MutationOutcome::Applied)
}

/// Applies one unsend event to a stored message. Only a target part that owns
/// the whole stored body is accepted; content, reactions, and every other field
/// are retained for local audit. Repeated unsends are no-ops.
pub fn apply_unsend(
    store: &mut EncryptedStore,
    target: &MutationTarget<'_>,
) -> Result<MutationOutcome, StoreError> {
    let key = stable_id(target.message_id)?;
    let tx = store._connection.transaction()?;
    let (message, part_ids) = match load_mutation_target(
        &tx,
        target.message_id,
        target.conversation_id,
        target.aliases,
        target.sender,
    )? {
        MutationLookup::Ready(message, part_ids) => (message, part_ids),
        MutationLookup::Rejected(outcome) => return Ok(outcome),
    };
    let mut message = *message;
    if message.unsent_at.is_some() {
        return Ok(MutationOutcome::Unchanged);
    }
    let original_at = message.sent_at.unwrap_or(message.created_at);
    if target.at < original_at {
        return Ok(MutationOutcome::Unsupported);
    }
    if part_ids.is_empty() || !part_ids.iter().all(|stored| *stored == Some(target.part)) {
        return Ok(MutationOutcome::Unsupported);
    }
    message.unsent_at = Some(target.at);
    write_mutation(&tx, &key, &message, &part_ids)?;
    tx.commit()?;
    Ok(MutationOutcome::Applied)
}

impl EncryptedStore {
    /// Applies an edit and returns the reloaded stored message only on `Applied`.
    pub fn apply_edit(
        &mut self,
        target: &MutationTarget<'_>,
        replacements: &[MessagePart],
    ) -> Result<Option<Message>, StoreError> {
        if self::apply_edit(self, target, replacements)? != MutationOutcome::Applied {
            return Ok(None);
        }
        get_message(self, target.message_id)
    }

    /// Applies an unsend and returns the reloaded stored message only on `Applied`.
    pub fn apply_unsend(
        &mut self,
        target: &MutationTarget<'_>,
    ) -> Result<Option<Message>, StoreError> {
        if self::apply_unsend(self, target)? != MutationOutcome::Applied {
            return Ok(None);
        }
        get_message(self, target.message_id)
    }
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

/// Search stored text parts and reported attachment file names of stored
/// messages, newest first. The scan is bounded to the newest
/// [`SEARCH_SCAN_LIMIT`] rows so one query cannot walk an unbounded store,
/// and it stops as soon as `limit` hits have been collected.
pub fn search_messages(
    store: &EncryptedStore,
    query: &str,
    limit: u16,
) -> Result<Vec<MessageSearchHit>, StoreError> {
    search_messages_with_participant(store, query, limit, None)
}

/// Search stored text parts and reported attachment file names of stored
/// messages, optionally scoped to canonical conversations whose current
/// participants include the exact filter handle. Membership is checked before
/// hits are counted, so a limit never hides a matching older message behind
/// newer out-of-scope hits.
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

/// Select one snippet per message. All text parts are scanned first, in stored
/// order, so a matching body always wins over a filename. Only when no body
/// part matches are reported, nonempty attachment file names scanned in stored
/// order. Missing or empty names contribute nothing: names are never inferred
/// from IDs, MIME types, or part kinds. One message still yields one hit.
fn search_snippet(message: &Message, query_lower: &str) -> Option<String> {
    for part in message.parts() {
        let MessagePart::Text(text) = part else {
            continue;
        };
        if let Some(snippet) = field_snippet(text, query_lower) {
            return Some(snippet);
        }
    }
    for part in message.parts() {
        let MessagePart::Attachment(attachment) = part else {
            continue;
        };
        let Some(name) = attachment.file_name() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Some(window) = field_snippet(name, query_lower) {
            // The stored name is matched raw, then escaped for presentation.
            return Some(format!("Filename: {}", window.escape_debug()));
        }
    }
    None
}

/// Build a contextual snippet around the first literal match in one field, reusing
/// the original-span mapping when lowercasing changes byte lengths.
fn field_snippet(text: &str, query_lower: &str) -> Option<String> {
    let (lower, boundaries) = lowercase_with_boundaries(text);
    let start_lower = lower.find(query_lower)?;
    let end_lower = start_lower + query_lower.len();
    let start = boundaries[start_lower].0;
    let end = boundaries[end_lower - 1].1;
    Some(snippet_window(text, start, end))
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
        AttachmentRef, Conversation, ConversationKind, DeliveryState, Handle, HandleKind,
        MessagePart, Participant, Reaction, TransportId,
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
        fixture_message_parts_on("alpha", message, conversation, at, parts)
    }

    fn fixture_attachment(id: &str, name: Option<&str>) -> MessagePart {
        MessagePart::Attachment(
            bloom_domain::AttachmentRef::new(
                id,
                Some("text/plain".to_owned()),
                name.map(str::to_owned),
                Some(11),
            )
            .unwrap(),
        )
    }

    fn fixture_message_parts_on(
        transport: &str,
        message: &str,
        conversation: &str,
        at: i64,
        parts: Vec<MessagePart>,
    ) -> Message {
        let transport = TransportId::new(transport).unwrap();
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
    fn search_finds_filename_only_attachment() {
        let (_temp, mut store, conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "named",
                "room",
                1,
                vec![fixture_attachment("a1", Some("quarterly-needle.pdf"))],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id.value(), "named");
        assert_eq!(hits[0].conversation_id, conversation);
        assert_eq!(hits[0].snippet, "Filename: quarterly-needle.pdf");
    }

    #[test]
    fn search_prefers_matching_body_over_earlier_matching_attachment() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "body-wins",
                "room",
                1,
                vec![
                    fixture_attachment("a1", Some("needle-attachment.txt")),
                    MessagePart::Text("body needle".into()),
                ],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "body needle");
    }

    #[test]
    fn search_returns_one_hit_and_the_first_matching_filename() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "multi-name",
                "room",
                1,
                vec![
                    fixture_attachment("a1", Some("first-needle.txt")),
                    fixture_attachment("a2", Some("second-needle.txt")),
                ],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id.value(), "multi-name");
        assert_eq!(hits[0].snippet, "Filename: first-needle.txt");
    }

    #[test]
    fn search_treats_filename_punctuation_and_prefixes_literally() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "literal",
                "room",
                1,
                vec![fixture_attachment("a1", Some("from: bob?.txt"))],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "from: bob?", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Filename: from: bob?.txt");
    }

    #[test]
    fn filename_snippet_uses_original_offsets_when_lowercasing_expands() {
        let (_temp, mut store, _conversation) = setup();
        let name = format!("{}needle{}", "\u{0130}".repeat(40), "\u{e9}".repeat(40));
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "expand-name",
                "room",
                1,
                vec![fixture_attachment("a1", Some(name.as_str()))],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "NEEDLE", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(snippet.starts_with("Filename: \u{2026}"), "{snippet}");
        assert!(snippet.ends_with('\u{2026}'), "{snippet}");
        assert!(snippet.contains("needle"), "{snippet}");
        assert_eq!(
            snippet
                .chars()
                .filter(|character| *character == '\u{e9}')
                .count(),
            30,
            "30 original multibyte context characters after the match"
        );
    }

    #[test]
    fn filename_search_ignores_missing_names_ids_mime_types_and_unsupported_parts() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "anonymous",
                "room",
                1,
                vec![
                    MessagePart::Attachment(
                        AttachmentRef::new(
                            "needle-id",
                            Some("needle/mime".into()),
                            None,
                            Some(2048),
                        )
                        .unwrap(),
                    ),
                    MessagePart::Attachment(
                        AttachmentRef::new(
                            "empty-name",
                            Some("text/plain".into()),
                            Some(String::new()),
                            Some(5),
                        )
                        .unwrap(),
                    ),
                    MessagePart::Unsupported {
                        kind: "needle".into(),
                    },
                ],
            ),
        )
        .unwrap();

        for query in ["needle", "needle-id", "needle/mime", "text/plain"] {
            assert!(
                search_messages(&store, query, 10).unwrap().is_empty(),
                "{query} must not match inferred metadata"
            );
        }
    }

    #[test]
    fn filename_snippet_escapes_controls_bidi_quotes_and_backslashes() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "unsafe-name",
                "room",
                1,
                vec![fixture_attachment(
                    "a1",
                    Some("evil\\\"needle\u{202e}\u{1b}.txt"),
                )],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(snippet.starts_with("Filename: evil"), "{snippet}");
        assert!(snippet.contains(r"\\"), "{snippet}");
        assert!(snippet.contains(r#"\""#), "{snippet}");
        assert!(snippet.contains(r"\u{202e}"), "{snippet}");
        assert!(snippet.contains(r"\u{1b}"), "{snippet}");
        assert!(!snippet.contains('\u{202e}'), "{snippet}");
        assert!(!snippet.contains('\u{1b}'), "{snippet}");

        // The snippet crosses the unchanged DTO: escaping must survive transport,
        // not appear as raw control or bidi characters after a round trip.
        let encoded = serde_json::to_string(&hits[0]).unwrap();
        let restored: MessageSearchHit = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, hits[0]);
        assert!(restored.snippet.contains(r"\u{202e}"), "{encoded}");
        assert!(restored.snippet.contains(r"\u{1b}"), "{encoded}");
        assert!(!restored
            .snippet
            .chars()
            .any(|character| character == '\u{202e}' || character.is_control()));
        assert!(encoded.contains(r"\\u{202e}"), "{encoded}");
        assert!(encoded.contains(r"\\u{1b}"), "{encoded}");
    }

    #[test]
    fn truncated_filename_snippet_keeps_a_literal_ellipsis() {
        let (_temp, mut store, _conversation) = setup();
        let name = format!("needle{}", "a".repeat(40));
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "truncated-name",
                "room",
                1,
                vec![fixture_attachment("a1", Some(name.as_str()))],
            ),
        )
        .unwrap();

        let hits = search_messages(&store, "needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert_eq!(
            *snippet,
            format!("Filename: needle{}\u{2026}", "a".repeat(30))
        );
        assert!(!snippet.contains(r"\u{2026}"), "{snippet}");
    }

    #[test]
    fn newer_filename_hits_displace_older_body_hits_at_the_limit() {
        let (_temp, mut store, _conversation) = setup();
        upsert_message(
            &mut store,
            &fixture_message(
                "alpha",
                "body",
                "room",
                1,
                "needle body",
                DeliveryState::Sent,
            ),
        )
        .unwrap();
        upsert_message(
            &mut store,
            &fixture_message_parts(
                "file",
                "room",
                2,
                vec![fixture_attachment("a1", Some("needle-newer.bin"))],
            ),
        )
        .unwrap();

        let limited = search_messages(&store, "needle", 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].message_id.value(), "file");
        assert_eq!(limited[0].snippet, "Filename: needle-newer.bin");

        let both = search_messages(&store, "needle", 2).unwrap();
        assert_eq!(
            both.iter()
                .map(|hit| hit.message_id.value())
                .collect::<Vec<_>>(),
            vec!["file", "body"]
        );
    }

    #[test]
    fn filtered_search_finds_an_older_filename_behind_more_than_fifty_newer_hits() {
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
        for at in 10..=64 {
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
            &fixture_message_parts(
                "hidden",
                "room",
                1,
                vec![fixture_attachment("hidden-att", Some("hidden-needle.pdf"))],
            ),
        )
        .unwrap();

        let unfiltered = search_messages(&store, "needle", 50).unwrap();
        assert_eq!(unfiltered.len(), 50);
        assert!(unfiltered
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
            assert_eq!(hits[0].snippet, "Filename: hidden-needle.pdf");
        }
    }

    #[test]
    fn filename_search_uses_alias_group_and_exact_transport_membership() {
        let (_temp, mut store, _conversation) = setup();
        let friend = participant(HandleKind::Email, "member@example.invalid", Some("Member"));
        let alias = ConversationId::new(TransportId::new("alpha").unwrap(), "legacy").unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "canonical",
                ConversationKind::Direct,
                vec![friend.clone()],
                vec![alias],
            ),
        )
        .unwrap();
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "legacy",
                ConversationKind::Direct,
                Vec::new(),
                Vec::new(),
            ),
        )
        .unwrap();
        let group_member = participant(HandleKind::Tel, "group-member", None);
        super::super::conversations::upsert_conversation(
            &mut store,
            &scoped_conversation(
                "alpha",
                "group",
                ConversationKind::Group,
                vec![group_member.clone()],
                Vec::new(),
            ),
        )
        .unwrap();
        let same = Handle::new(HandleKind::Email, "same@example.invalid").unwrap();
        for transport in ["alpha", "beta"] {
            super::super::conversations::upsert_conversation(
                &mut store,
                &scoped_conversation(
                    transport,
                    &format!("{transport}-same"),
                    ConversationKind::Direct,
                    vec![Participant {
                        handle: same.clone(),
                        display_name: None,
                        is_me: false,
                    }],
                    Vec::new(),
                ),
            )
            .unwrap();
        }
        for message in [
            fixture_message_parts(
                "via-alias",
                "legacy",
                1,
                vec![fixture_attachment("a1", Some("needle-alias.txt"))],
            ),
            fixture_message_parts(
                "group-file",
                "group",
                2,
                vec![fixture_attachment("a2", Some("needle-group.txt"))],
            ),
            fixture_message_parts(
                "alpha-file",
                "alpha-same",
                3,
                vec![fixture_attachment("a3", Some("needle-alpha.txt"))],
            ),
            fixture_message_parts_on(
                "beta",
                "beta-file",
                "beta-same",
                4,
                vec![fixture_attachment("a4", Some("needle-beta.txt"))],
            ),
        ] {
            upsert_message(&mut store, &message).unwrap();
        }

        let member = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: friend.handle,
        };
        let hits = search_messages_with_participant(&store, "needle", 10, Some(&member)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id.value(), "canonical");
        assert_eq!(hits[0].snippet, "Filename: needle-alias.txt");

        let group = ParticipantFilter {
            transport: TransportId::new("alpha").unwrap(),
            handle: group_member.handle,
        };
        let hits = search_messages_with_participant(&store, "needle", 10, Some(&group)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id.value(), "group");
        assert_eq!(hits[0].snippet, "Filename: needle-group.txt");

        for (transport, expected) in [("alpha", "alpha-same"), ("beta", "beta-same")] {
            let filter = ParticipantFilter {
                transport: TransportId::new(transport).unwrap(),
                handle: same.clone(),
            };
            let hits =
                search_messages_with_participant(&store, "needle", 10, Some(&filter)).unwrap();
            assert_eq!(
                hits.len(),
                1,
                "{transport} must match exactly one conversation"
            );
            assert_eq!(hits[0].conversation_id.value(), expected);
            assert_eq!(hits[0].snippet, format!("Filename: needle-{transport}.txt"));
        }
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
                &fixture_message_parts(
                    id,
                    "room",
                    at,
                    vec![
                        MessagePart::Text("needle".into()),
                        fixture_attachment(&format!("att-{id}"), Some("needlefile")),
                    ],
                ),
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

            let hits = search_messages_with_participant(&store, "needlefile", 50, scope).unwrap();
            assert_eq!(
                hits.len(),
                1,
                "filename matching must use the same global scan boundary"
            );
            assert_eq!(hits[0].message_id.value(), "at-bound");
            assert_eq!(hits[0].snippet, "Filename: needlefile");
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

    fn fixture_reaction(
        sender: &str,
        kind: &str,
        part: Option<u32>,
        timestamp: Option<i64>,
    ) -> Reaction {
        Reaction {
            sender: Handle::new(HandleKind::Opaque, sender).unwrap(),
            kind: kind.into(),
            remote_id: None,
            part,
            timestamp,
        }
    }

    fn raw_message_data(store: &EncryptedStore, id: &MessageId) -> Option<Vec<u8>> {
        store
            ._connection
            .query_row(
                "SELECT data FROM messages WHERE id=?1",
                params![stable_id(id).unwrap()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn apply_reaction_adds_and_repeating_add_is_unchanged() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        let reaction = fixture_reaction("alice", "heart", None, Some(5));
        assert_eq!(
            apply_reaction(&mut store, message.id(), &reaction, true).unwrap(),
            ReactionOutcome::Applied
        );
        assert_eq!(
            get_message(&store, message.id())
                .unwrap()
                .unwrap()
                .reactions,
            vec![reaction.clone()]
        );
        assert_eq!(
            apply_reaction(&mut store, message.id(), &reaction, true).unwrap(),
            ReactionOutcome::Unchanged
        );
        assert_eq!(
            get_message(&store, message.id())
                .unwrap()
                .unwrap()
                .reactions,
            vec![reaction]
        );
    }

    #[test]
    fn apply_reaction_removes_exactly_the_matching_tuple_only() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        let removed = fixture_reaction("alice", "heart", Some(0), Some(5));
        let kept_by_sender = fixture_reaction("bob", "heart", Some(0), Some(6));
        let kept_by_kind = fixture_reaction("alice", "like", Some(0), Some(7));
        let kept_by_part = fixture_reaction("alice", "heart", Some(1), Some(8));
        for reaction in [&removed, &kept_by_sender, &kept_by_kind, &kept_by_part] {
            apply_reaction(&mut store, message.id(), reaction, true).unwrap();
        }
        assert_eq!(
            apply_reaction(&mut store, message.id(), &removed, false).unwrap(),
            ReactionOutcome::Applied
        );
        let reactions = get_message(&store, message.id())
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(reactions.len(), 3);
        assert!(!reactions.contains(&removed));
        assert!(reactions.contains(&kept_by_sender));
        assert!(reactions.contains(&kept_by_kind));
        assert!(reactions.contains(&kept_by_part));
    }

    #[test]
    fn apply_reaction_removing_missing_tuple_is_unchanged() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        let stored = fixture_reaction("alice", "heart", Some(0), Some(5));
        apply_reaction(&mut store, message.id(), &stored, true).unwrap();
        let before = raw_message_data(&store, message.id());
        let absent = fixture_reaction("alice", "heart", Some(1), Some(5));
        assert_eq!(
            apply_reaction(&mut store, message.id(), &absent, false).unwrap(),
            ReactionOutcome::Unchanged
        );
        assert_eq!(raw_message_data(&store, message.id()), before);
    }

    #[test]
    fn apply_reaction_missing_or_wrong_target_is_not_found_and_writes_nothing() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        let before = raw_message_data(&store, message.id());
        let reaction = fixture_reaction("alice", "heart", None, Some(5));
        for target in [message_id("alpha", "missing"), message_id("beta", "m1")] {
            assert_eq!(
                apply_reaction(&mut store, &target, &reaction, true).unwrap(),
                ReactionOutcome::NotFound
            );
        }
        assert_eq!(raw_message_data(&store, message.id()), before);
        assert!(get_message(&store, &message_id("alpha", "missing"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn apply_reaction_part_distinguishes_otherwise_identical_reactions() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        let whole = fixture_reaction("alice", "heart", None, Some(5));
        let part_zero = fixture_reaction("alice", "heart", Some(0), Some(5));
        assert_eq!(
            apply_reaction(&mut store, message.id(), &whole, true).unwrap(),
            ReactionOutcome::Applied
        );
        assert_eq!(
            apply_reaction(&mut store, message.id(), &part_zero, true).unwrap(),
            ReactionOutcome::Applied
        );
        let reactions = get_message(&store, message.id())
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(reactions.len(), 2);
        assert!(reactions.contains(&whole));
        assert!(reactions.contains(&part_zero));
    }

    #[test]
    fn apply_reaction_ordering_is_deterministic_across_inserted_order() {
        let (_temp_a, mut first, _conversation_a) = setup();
        let (_temp_b, mut second, _conversation_b) = setup();
        let first_message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        let second_message = first_message.clone();
        upsert_message(&mut first, &first_message).unwrap();
        upsert_message(&mut second, &second_message).unwrap();
        let early_alice = fixture_reaction("alice", "heart", None, Some(5));
        let early_bob = fixture_reaction("bob", "heart", None, Some(5));
        let late = fixture_reaction("alice", "laugh", None, Some(10));
        let undated = fixture_reaction("alice", "like", None, None);
        for reaction in [&early_bob, &undated, &late, &early_alice] {
            apply_reaction(&mut first, first_message.id(), reaction, true).unwrap();
        }
        for reaction in [&early_alice, &late, &early_bob, &undated] {
            apply_reaction(&mut second, second_message.id(), reaction, true).unwrap();
        }
        let expected = vec![early_alice, early_bob, late, undated];
        assert_eq!(
            get_message(&first, first_message.id())
                .unwrap()
                .unwrap()
                .reactions,
            expected
        );
        assert_eq!(
            get_message(&second, second_message.id())
                .unwrap()
                .unwrap()
                .reactions,
            expected
        );
    }

    #[test]
    fn apply_reaction_preserves_every_non_reaction_field() {
        let (_temp, mut store, conversation) = setup();
        let message = Message::new(
            message_id("alpha", "m1"),
            conversation,
            Some("remote-1".into()),
            Handle::new(HandleKind::Opaque, "sender-1").unwrap(),
            vec![
                MessagePart::Text("body".into()),
                fixture_attachment("attachment-1", Some("file.txt")),
            ],
            10,
            Some(11),
            DeliveryState::Delivered,
            true,
            Some(12),
            vec![],
            Some(13),
        )
        .unwrap();
        upsert_message(&mut store, &message).unwrap();
        let reaction = fixture_reaction("alice", "heart", Some(0), Some(5));
        assert_eq!(
            apply_reaction(&mut store, message.id(), &reaction, true).unwrap(),
            ReactionOutcome::Applied
        );
        let stored = get_message(&store, message.id()).unwrap().unwrap();
        assert_eq!(stored.id(), message.id());
        assert_eq!(stored.conversation_id(), message.conversation_id());
        assert_eq!(stored.remote_id, message.remote_id);
        assert_eq!(stored.sender, message.sender);
        assert_eq!(stored.parts, message.parts);
        assert_eq!(stored.created_at, message.created_at);
        assert_eq!(stored.sent_at, message.sent_at);
        assert_eq!(stored.delivery, message.delivery);
        assert_eq!(stored.from_me, message.from_me);
        assert_eq!(stored.edited_at, message.edited_at);
        assert_eq!(stored.unsent_at, message.unsent_at);
        assert_eq!(stored.reactions, vec![reaction]);
    }

    fn store_part_map(store: &EncryptedStore, id: &MessageId, map: &[Option<u64>]) {
        let data = serde_json::to_vec(map).unwrap();
        store
            ._connection
            .execute(
                "INSERT INTO message_part_maps(message_id, data) VALUES (?1, ?2)
                 ON CONFLICT(message_id) DO UPDATE SET data=excluded.data",
                params![stable_id(id).unwrap(), data],
            )
            .unwrap();
    }

    fn raw_part_map(store: &EncryptedStore, id: &MessageId) -> Option<Vec<u8>> {
        store
            ._connection
            .query_row(
                "SELECT data FROM message_part_maps WHERE message_id=?1",
                params![stable_id(id).unwrap()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    fn edit(
        store: &mut EncryptedStore,
        message: &Message,
        part: u64,
        replacements: &[MessagePart],
        at: i64,
    ) -> MutationOutcome {
        apply_edit(
            store,
            &MutationTarget {
                message_id: message.id(),
                conversation_id: message.conversation_id(),
                aliases: &[],
                sender: &message.sender,
                part,
                at,
            },
            replacements,
        )
        .unwrap()
    }

    fn unsend(
        store: &mut EncryptedStore,
        message: &Message,
        part: u64,
        at: i64,
    ) -> MutationOutcome {
        apply_unsend(
            store,
            &MutationTarget {
                message_id: message.id(),
                conversation_id: message.conversation_id(),
                aliases: &[],
                sender: &message.sender,
                part,
                at,
            },
        )
        .unwrap()
    }

    #[test]
    fn edit_replaces_only_the_target_part_group_and_preserves_everything_else() {
        let (_temp, mut store, conversation) = setup();
        let reaction = fixture_reaction("alice", "heart", Some(0), Some(5));
        let message = Message::new(
            message_id("alpha", "m1"),
            conversation,
            Some("remote-1".into()),
            Handle::new(HandleKind::Opaque, "synthetic-sender").unwrap(),
            vec![
                MessagePart::Text("a".into()),
                MessagePart::Text("b".into()),
                MessagePart::Text("c".into()),
                fixture_attachment("attachment-1", Some("file.txt")),
            ],
            10,
            Some(11),
            DeliveryState::Delivered,
            true,
            None,
            vec![reaction.clone()],
            None,
        )
        .unwrap();
        upsert_message(&mut store, &message).unwrap();
        store_part_map(&store, message.id(), &[Some(0), Some(1), None, Some(1)]);

        assert_eq!(
            edit(
                &mut store,
                &message,
                1,
                &[MessagePart::Text("x".into()), MessagePart::Text("y".into())],
                20,
            ),
            MutationOutcome::Applied
        );
        let stored = get_message(&store, message.id()).unwrap().unwrap();
        assert_eq!(
            stored.parts,
            vec![
                MessagePart::Text("a".into()),
                MessagePart::Text("x".into()),
                MessagePart::Text("y".into()),
                MessagePart::Text("c".into()),
            ]
        );
        assert_eq!(
            raw_part_map(&store, message.id()),
            Some(serde_json::to_vec(&[Some(0), Some(1), Some(1), None]).unwrap())
        );
        assert_eq!(stored.id(), message.id());
        assert_eq!(stored.conversation_id(), message.conversation_id());
        assert_eq!(stored.remote_id, message.remote_id);
        assert_eq!(stored.sender, message.sender);
        assert_eq!(stored.created_at, message.created_at);
        assert_eq!(stored.sent_at, message.sent_at);
        assert_eq!(stored.delivery, message.delivery);
        assert_eq!(stored.from_me, message.from_me);
        assert_eq!(stored.edited_at, Some(20));
        assert_eq!(stored.reactions, vec![reaction]);
        assert_eq!(stored.unsent_at, None);
    }

    #[test]
    fn duplicate_and_older_edits_are_unchanged_and_empty_replacements_rejected() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        store_part_map(&store, message.id(), &[Some(0)]);
        let replacement = [MessagePart::Text("first".into())];
        assert_eq!(
            edit(&mut store, &message, 0, &replacement, 10),
            MutationOutcome::Applied
        );
        let before = raw_message_data(&store, message.id());
        assert_eq!(
            edit(&mut store, &message, 0, &replacement, 10),
            MutationOutcome::Unchanged
        );
        assert_eq!(
            edit(&mut store, &message, 0, &replacement, 9),
            MutationOutcome::Unchanged
        );
        assert_eq!(
            edit(&mut store, &message, 0, &[], 20),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            edit(
                &mut store,
                &message,
                0,
                &[MessagePart::Text(String::new())],
                20,
            ),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            edit(
                &mut store,
                &message,
                0,
                &[fixture_attachment("replacement", Some("file.bin"))],
                20,
            ),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            edit(&mut store, &message, 0, &replacement, 0),
            MutationOutcome::Unsupported
        );
        assert_eq!(raw_message_data(&store, message.id()), before);
    }

    #[test]
    fn missing_or_misaligned_maps_are_unsupported_and_write_nothing() {
        let (_temp, mut store, _conversation) = setup();
        let no_map = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        let misaligned = fixture_message("alpha", "m2", "room", 2, "body", DeliveryState::Sent);
        upsert_message(&mut store, &no_map).unwrap();
        upsert_message(&mut store, &misaligned).unwrap();
        store_part_map(&store, misaligned.id(), &[Some(0), Some(0)]);
        let replacement = [MessagePart::Text("edited".into())];
        for target in [&no_map, &misaligned] {
            let before = raw_message_data(&store, target.id());
            assert_eq!(
                edit(&mut store, target, 0, &replacement, 10),
                MutationOutcome::Unsupported
            );
            assert_eq!(raw_message_data(&store, target.id()), before);
            assert_eq!(
                unsend(&mut store, target, 0, 10),
                MutationOutcome::Unsupported
            );
            assert_eq!(raw_message_data(&store, target.id()), before);
        }
        assert_eq!(raw_part_map(&store, no_map.id()), None);
    }

    #[test]
    fn wrong_sender_or_conversation_is_unsupported_and_unknown_target_is_not_found() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message("alpha", "m1", "room", 1, "body", DeliveryState::Sent);
        upsert_message(&mut store, &message).unwrap();
        store_part_map(&store, message.id(), &[Some(0)]);
        let replacement = [MessagePart::Text("edited".into())];
        let before = raw_message_data(&store, message.id());
        let wrong_sender = Handle::new(HandleKind::Opaque, "other-sender").unwrap();
        assert_eq!(
            apply_edit(
                &mut store,
                &MutationTarget {
                    message_id: message.id(),
                    conversation_id: message.conversation_id(),
                    aliases: &[],
                    sender: &wrong_sender,
                    part: 0,
                    at: 10,
                },
                &replacement,
            )
            .unwrap(),
            MutationOutcome::Unsupported
        );
        let wrong_conversation =
            ConversationId::new(TransportId::new("beta").unwrap(), "room").unwrap();
        assert_eq!(
            apply_unsend(
                &mut store,
                &MutationTarget {
                    message_id: message.id(),
                    conversation_id: &wrong_conversation,
                    aliases: &[],
                    sender: &message.sender,
                    part: 0,
                    at: 10,
                },
            )
            .unwrap(),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            apply_unsend(
                &mut store,
                &MutationTarget {
                    message_id: &message_id("alpha", "missing"),
                    conversation_id: message.conversation_id(),
                    aliases: &[],
                    sender: &message.sender,
                    part: 0,
                    at: 10,
                },
            )
            .unwrap(),
            MutationOutcome::NotFound
        );
        assert_eq!(raw_message_data(&store, message.id()), before);
        assert!(get_message(&store, &message_id("alpha", "missing"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn whole_body_unsend_retains_content_and_reactions_and_is_idempotent() {
        let (_temp, mut store, _conversation) = setup();
        let reaction = fixture_reaction("alice", "heart", Some(0), Some(5));
        let message = fixture_message_parts(
            "m1",
            "room",
            1,
            vec![
                MessagePart::Text("first".into()),
                MessagePart::Text("second".into()),
            ],
        );
        let mut message = message;
        message.reactions = vec![reaction.clone()];
        upsert_message(&mut store, &message).unwrap();
        store_part_map(&store, message.id(), &[Some(0), Some(0)]);
        assert_eq!(
            unsend(&mut store, &message, 0, 0),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            unsend(&mut store, &message, 0, 20),
            MutationOutcome::Applied
        );
        let stored = get_message(&store, message.id()).unwrap().unwrap();
        assert_eq!(stored.parts, message.parts);
        assert_eq!(stored.reactions, vec![reaction]);
        assert_eq!(stored.unsent_at, Some(20));
        assert_eq!(stored.edited_at, None);
        let before = raw_message_data(&store, message.id());
        assert_eq!(
            unsend(&mut store, &message, 0, 21),
            MutationOutcome::Unchanged
        );
        assert_eq!(
            unsend(&mut store, &message, 0, 19),
            MutationOutcome::Unchanged
        );
        assert_eq!(
            edit(
                &mut store,
                &message,
                0,
                &[MessagePart::Text("too late".into())],
                30,
            ),
            MutationOutcome::Unsupported
        );
        assert_eq!(raw_message_data(&store, message.id()), before);
    }

    #[test]
    fn partial_multipart_unsend_is_unsupported_and_writes_nothing() {
        let (_temp, mut store, _conversation) = setup();
        let message = fixture_message_parts(
            "m1",
            "room",
            1,
            vec![
                MessagePart::Text("first".into()),
                MessagePart::Text("second".into()),
            ],
        );
        upsert_message(&mut store, &message).unwrap();
        store_part_map(&store, message.id(), &[Some(0), Some(1)]);
        let before = raw_message_data(&store, message.id());
        assert_eq!(
            unsend(&mut store, &message, 0, 20),
            MutationOutcome::Unsupported
        );
        assert_eq!(
            unsend(&mut store, &message, 1, 20),
            MutationOutcome::Unsupported
        );
        assert_eq!(raw_message_data(&store, message.id()), before);
    }
}
