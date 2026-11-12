//! Synthetic demo fixture loader and seeder.
//!
//! The fixture is compiled into the daemon so a demo session never reads a file
//! from the host. Loading is strict: every sender, participant, reaction target,
//! read marker, draft owner, and attachment must resolve before a single row is
//! written. Seeding then uses the ordinary store APIs, so the demo database has
//! exactly the same shape as a real one.
//!
//! All timestamps are offsets from one caller-supplied session anchor. A fixed
//! anchor therefore produces byte-for-byte identical store contents and ordering;
//! a live launch passes the current time so the history looks recent.

use crate::store::{
    ingest_inbound, save_draft, DraftKey, DraftWriteOutcome, DurableDraft, EncryptedStore,
    StoreError, MAX_DRAFT_AGE_MS, MAX_DRAFT_BYTES,
};
use bloom_domain::{
    AttachmentRef, Conversation, ConversationId, ConversationKind, DeliveryState, Handle,
    HandleKind, Message, MessageId, MessagePart, Participant, Reaction, TransportId,
};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    fmt,
};
use unicode_segmentation::UnicodeSegmentation;

/// Versioned fixture content compiled into the demo daemon.
pub const FIXTURE_JSON: &str = include_str!("../../../../fixtures/demo/venue.json");
const FIXTURE_VERSION: u64 = 1;
const MIN_CONVERSATIONS: usize = 4;
const MAX_CONVERSATIONS: usize = 6;
const MIN_PAGEABLE_HISTORY: usize = 201;
const MAX_GENERATED_HISTORY: usize = 10_000;

/// Why the fixture was rejected or the store could not be seeded.
#[derive(Debug)]
pub enum SeedError {
    Json(serde_json::Error),
    Invalid(String),
    Store(StoreError),
}

impl fmt::Display for SeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "demo fixture JSON is invalid: {error}"),
            Self::Invalid(reason) => write!(f, "demo fixture is invalid: {reason}"),
            Self::Store(error) => write!(f, "demo store seeding failed: {error}"),
        }
    }
}

impl std::error::Error for SeedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<serde_json::Error> for SeedError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<StoreError> for SeedError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// What one seeding pass wrote, in fixture order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeedSummary {
    pub conversations: Vec<ConversationId>,
    pub message_count: usize,
    pub draft_count: usize,
    /// Local read positions keyed by conversation, marker message, and its time.
    pub read_positions: Vec<(ConversationId, MessageId, i64)>,
}

struct Fixture {
    transport: TransportId,
    me: Handle,
    conversations: Vec<ConversationSpec>,
}

struct ConversationSpec {
    id: String,
    remote_id: String,
    kind: ConversationKind,
    title: Option<String>,
    created_offset_seconds: i64,
    participants: Vec<ParticipantSpec>,
    messages: Vec<MessageSpec>,
    draft: Option<DraftSpec>,
    read_marker: Option<String>,
}

struct ParticipantSpec {
    handle: Handle,
    display_name: Option<String>,
    is_me: bool,
}

struct MessageSpec {
    id: String,
    sender: Handle,
    from_me: bool,
    offset_seconds: i64,
    sent_offset_seconds: Option<i64>,
    delivery: DeliveryState,
    parts: Vec<MessagePart>,
    edited_offset_seconds: Option<i64>,
    unsent_offset_seconds: Option<i64>,
    reactions: Vec<ReactionSpec>,
}

struct ReactionSpec {
    sender: Handle,
    kind: String,
    remote_id: Option<String>,
    part: Option<u32>,
    timestamp_offset_seconds: Option<i64>,
}

struct DraftSpec {
    body: String,
    cursor: usize,
    pasted_body: bool,
    uncertain: bool,
    revision: u64,
    updated_offset_seconds: i64,
}

/// Seed the compiled fixture into a freshly opened demo store.
pub fn seed(store: &mut EncryptedStore, anchor_ms: i64) -> Result<SeedSummary, SeedError> {
    let fixture = parse_fixture(FIXTURE_JSON)?;
    validate(&fixture)?;
    seed_fixture(store, &fixture, anchor_ms)
}

/// Seed the compiled fixture using the current wall clock as the session anchor.
pub fn seed_now(store: &mut EncryptedStore) -> Result<SeedSummary, SeedError> {
    seed(store, now_millis())
}

/// Parse and validate fixture JSON without writing anything.
#[cfg(test)]
pub fn validate_fixture(json: &str) -> Result<(), SeedError> {
    let fixture = parse_fixture(json)?;
    validate(&fixture)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn seed_fixture(
    store: &mut EncryptedStore,
    fixture: &Fixture,
    anchor_ms: i64,
) -> Result<SeedSummary, SeedError> {
    let mut summary = SeedSummary::default();
    for spec in &fixture.conversations {
        let id = ConversationId::new(fixture.transport.clone(), spec.id.clone())
            .map_err(|_| invalid(format!("conversation `{}` has an invalid id", spec.id)))?;
        let participants = spec
            .participants
            .iter()
            .map(|participant| Participant {
                handle: participant.handle.clone(),
                display_name: participant.display_name.clone(),
                is_me: participant.is_me,
            })
            .collect();
        let conversation = Conversation {
            id: id.clone(),
            remote_id: spec.remote_id.clone(),
            kind: spec.kind.clone(),
            title: spec.title.clone(),
            participants,
            created_at: timestamp(
                anchor_ms,
                spec.created_offset_seconds,
                "conversation created_at",
            )?,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };

        let mut messages_by_id = HashMap::new();
        for message_spec in &spec.messages {
            let message_id = MessageId::new(fixture.transport.clone(), message_spec.id.clone())
                .map_err(|_| invalid(format!("message `{}` has an invalid id", message_spec.id)))?;
            let created_at =
                timestamp(anchor_ms, message_spec.offset_seconds, "message created_at")?;
            let sent_at = message_spec
                .sent_offset_seconds
                .map(|offset| timestamp(anchor_ms, offset, "message sent_at"))
                .transpose()?;
            let edited_at = message_spec
                .edited_offset_seconds
                .map(|offset| timestamp(anchor_ms, offset, "message edited_at"))
                .transpose()?;
            let unsent_at = message_spec
                .unsent_offset_seconds
                .map(|offset| timestamp(anchor_ms, offset, "message unsent_at"))
                .transpose()?;
            let reactions = message_spec
                .reactions
                .iter()
                .map(|reaction| {
                    Ok(Reaction {
                        sender: reaction.sender.clone(),
                        kind: reaction.kind.clone(),
                        remote_id: reaction.remote_id.clone(),
                        part: reaction.part,
                        timestamp: reaction
                            .timestamp_offset_seconds
                            .map(|offset| timestamp(anchor_ms, offset, "reaction timestamp"))
                            .transpose()?,
                    })
                })
                .collect::<Result<Vec<_>, SeedError>>()?;
            let message = Message::new(
                message_id.clone(),
                id.clone(),
                None,
                message_spec.sender.clone(),
                message_spec.parts.clone(),
                created_at,
                sent_at,
                message_spec.delivery.clone(),
                message_spec.from_me,
                edited_at,
                reactions,
                unsent_at,
            )
            .map_err(|_| {
                invalid(format!(
                    "message `{}` has inconsistent identity",
                    message_spec.id
                ))
            })?;
            ingest_inbound(store, &conversation, &message)?;
            messages_by_id.insert(message_spec.id.clone(), (created_at, message_id));
            summary.message_count += 1;
        }

        if let Some(marker) = &spec.read_marker {
            let (created_at, message_id) = messages_by_id.get(marker).ok_or_else(|| {
                invalid(format!(
                    "conversation `{}` read marker `{marker}` does not resolve",
                    spec.id
                ))
            })?;
            summary
                .read_positions
                .push((id.clone(), message_id.clone(), *created_at));
        }

        if let Some(draft) = &spec.draft {
            let key = DraftKey::Conversation(id.clone());
            let durable = DurableDraft {
                body: draft.body.clone(),
                cursor: draft.cursor,
                pasted_body: draft.pasted_body,
                uncertain: draft.uncertain,
            };
            let updated_at =
                timestamp(anchor_ms, draft.updated_offset_seconds, "draft updated_at")?;
            let outcome = save_draft(store, &key, &durable, draft.revision, updated_at)?;
            if outcome != DraftWriteOutcome::Applied {
                return Err(invalid(format!(
                    "conversation `{}` draft revision {} was rejected as stale",
                    spec.id, draft.revision
                )));
            }
            summary.draft_count += 1;
        }

        summary.conversations.push(id);
    }
    Ok(summary)
}

fn timestamp(anchor_ms: i64, offset_seconds: i64, context: &str) -> Result<i64, SeedError> {
    offset_seconds
        .checked_mul(1_000)
        .and_then(|offset| anchor_ms.checked_add(offset))
        .ok_or_else(|| invalid(format!("{context} overflows the timestamp range")))
}

fn parse_fixture(json: &str) -> Result<Fixture, SeedError> {
    let value: Value = serde_json::from_str(json)?;
    let root = as_object(&value, "fixture")?;
    let version = as_u64(required(root, "version", "fixture")?, "fixture.version")?;
    if version != FIXTURE_VERSION {
        return Err(invalid(format!(
            "unsupported fixture version {version}, expected {FIXTURE_VERSION}"
        )));
    }
    let transport = TransportId::new(as_str(
        required(root, "transport", "fixture")?,
        "fixture.transport",
    )?)
    .map_err(|_| invalid("fixture.transport must not be empty"))?;
    let me_object = as_object(required(root, "me", "fixture")?, "fixture.me")?;
    let me = parse_handle(
        required(me_object, "handle", "fixture.me")?,
        "fixture.me.handle",
    )?;
    validate_handle(&me, "fixture.me.handle")?;

    let conversation_values = as_array(
        required(root, "conversations", "fixture")?,
        "fixture.conversations",
    )?;
    let mut conversations = Vec::with_capacity(conversation_values.len());
    for (index, value) in conversation_values.iter().enumerate() {
        conversations.push(parse_conversation(value, &me, index)?);
    }
    Ok(Fixture {
        transport,
        me,
        conversations,
    })
}

fn parse_conversation(
    value: &Value,
    me: &Handle,
    index: usize,
) -> Result<ConversationSpec, SeedError> {
    let context = format!("fixture.conversations[{index}]");
    let object = as_object(value, &context)?;
    let id = as_str(required(object, "id", &context)?, &format!("{context}.id"))?.to_owned();
    let remote_id = as_str(
        required(object, "remote_id", &context)?,
        &format!("{context}.remote_id"),
    )?
    .to_owned();
    let kind = parse_kind(
        required(object, "kind", &context)?,
        &format!("{context}.kind"),
    )?;
    let title = optional_str(object, "title", &format!("{context}.title"))?.map(str::to_owned);
    let created_offset_seconds = as_i64(
        required(object, "created_offset_seconds", &context)?,
        &format!("{context}.created_offset_seconds"),
    )?;

    let participant_values = as_array(
        required(object, "participants", &context)?,
        &format!("{context}.participants"),
    )?;
    let mut participants = Vec::with_capacity(participant_values.len());
    for (participant_index, participant) in participant_values.iter().enumerate() {
        let participant_context = format!("{context}.participants[{participant_index}]");
        let participant = as_object(participant, &participant_context)?;
        participants.push(ParticipantSpec {
            handle: parse_handle(
                required(participant, "handle", &participant_context)?,
                &format!("{participant_context}.handle"),
            )?,
            display_name: optional_str(
                participant,
                "display_name",
                &format!("{participant_context}.display_name"),
            )?
            .map(str::to_owned),
            is_me: as_bool(
                required(participant, "is_me", &participant_context)?,
                &format!("{participant_context}.is_me"),
            )?,
        });
    }

    let messages_value = optional(object, "messages");
    let history_value = optional(object, "history");
    let messages = if let Some(history) = history_value {
        if let Some(messages) = messages_value {
            let messages = as_array(messages, &format!("{context}.messages"))?;
            if !messages.is_empty() {
                return Err(invalid(format!(
                    "{context} must use either `messages` or `history`, not both"
                )));
            }
        }
        parse_history(history, me, &id, &format!("{context}.history"))?
    } else if let Some(messages) = messages_value {
        parse_messages(messages, &format!("{context}.messages"))?
    } else {
        Vec::new()
    };

    let read_marker =
        optional_str(object, "read_marker", &format!("{context}.read_marker"))?.map(str::to_owned);
    let draft = optional(object, "draft")
        .map(|value| parse_draft(value, &format!("{context}.draft")))
        .transpose()?;

    Ok(ConversationSpec {
        id,
        remote_id,
        kind,
        title,
        created_offset_seconds,
        participants,
        messages,
        draft,
        read_marker,
    })
}

fn parse_messages(value: &Value, context: &str) -> Result<Vec<MessageSpec>, SeedError> {
    let values = as_array(value, context)?;
    let mut messages = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let message_context = format!("{context}[{index}]");
        let object = as_object(value, &message_context)?;
        messages.push(MessageSpec {
            id: as_str(
                required(object, "id", &message_context)?,
                &format!("{message_context}.id"),
            )?
            .to_owned(),
            sender: parse_handle(
                required(object, "sender", &message_context)?,
                &format!("{message_context}.sender"),
            )?,
            from_me: as_bool(
                required(object, "from_me", &message_context)?,
                &format!("{message_context}.from_me"),
            )?,
            offset_seconds: as_i64(
                required(object, "offset_seconds", &message_context)?,
                &format!("{message_context}.offset_seconds"),
            )?,
            sent_offset_seconds: optional_i64(
                object,
                "sent_offset_seconds",
                &format!("{message_context}.sent_offset_seconds"),
            )?,
            delivery: parse_delivery(
                required(object, "delivery", &message_context)?,
                &format!("{message_context}.delivery"),
            )?,
            parts: parse_parts(
                required(object, "parts", &message_context)?,
                &format!("{message_context}.parts"),
            )?,
            edited_offset_seconds: optional_i64(
                object,
                "edited_offset_seconds",
                &format!("{message_context}.edited_offset_seconds"),
            )?,
            unsent_offset_seconds: optional_i64(
                object,
                "unsent_offset_seconds",
                &format!("{message_context}.unsent_offset_seconds"),
            )?,
            reactions: parse_reactions(
                optional(object, "reactions"),
                &format!("{message_context}.reactions"),
            )?,
        });
    }
    Ok(messages)
}

fn parse_history(
    value: &Value,
    me: &Handle,
    conversation_id: &str,
    context: &str,
) -> Result<Vec<MessageSpec>, SeedError> {
    let object = as_object(value, context)?;
    let count = as_u64(
        required(object, "count", context)?,
        &format!("{context}.count"),
    )?;
    let count =
        usize::try_from(count).map_err(|_| invalid(format!("{context}.count is too large")))?;
    if count > MAX_GENERATED_HISTORY {
        return Err(invalid(format!(
            "{context}.count exceeds the {MAX_GENERATED_HISTORY} row limit"
        )));
    }
    let start_offset_seconds = as_i64(
        required(object, "start_offset_seconds", context)?,
        &format!("{context}.start_offset_seconds"),
    )?;
    let step_seconds = as_i64(
        required(object, "step_seconds", context)?,
        &format!("{context}.step_seconds"),
    )?;
    if step_seconds <= 0 {
        return Err(invalid(format!("{context}.step_seconds must be positive")));
    }
    let turns = as_array(
        required(object, "turns", context)?,
        &format!("{context}.turns"),
    )?;
    if turns.is_empty() {
        return Err(invalid(format!("{context}.turns must not be empty")));
    }
    let mut parsed_turns = Vec::with_capacity(turns.len());
    for (index, turn) in turns.iter().enumerate() {
        let turn_context = format!("{context}.turns[{index}]");
        let turn = as_object(turn, &turn_context)?;
        parsed_turns.push((
            parse_handle(
                required(turn, "sender", &turn_context)?,
                &format!("{turn_context}.sender"),
            )?,
            as_str(
                required(turn, "text", &turn_context)?,
                &format!("{turn_context}.text"),
            )?
            .to_owned(),
        ));
    }

    let mut messages = Vec::with_capacity(count);
    for index in 0..count {
        let (sender, text) = &parsed_turns[index % parsed_turns.len()];
        let offset_seconds = (index as i64)
            .checked_mul(step_seconds)
            .and_then(|offset| start_offset_seconds.checked_add(offset))
            .ok_or_else(|| invalid(format!("{context} timestamp offset overflows")))?;
        let from_me = sender == me;
        messages.push(MessageSpec {
            id: format!("{conversation_id}-{:04}", index + 1),
            sender: sender.clone(),
            from_me,
            offset_seconds,
            sent_offset_seconds: None,
            delivery: if from_me {
                DeliveryState::Read
            } else {
                DeliveryState::Delivered
            },
            parts: vec![MessagePart::Text(text.clone())],
            edited_offset_seconds: None,
            unsent_offset_seconds: None,
            reactions: Vec::new(),
        });
    }
    Ok(messages)
}

fn parse_draft(value: &Value, context: &str) -> Result<DraftSpec, SeedError> {
    let object = as_object(value, context)?;
    Ok(DraftSpec {
        body: as_str(
            required(object, "body", context)?,
            &format!("{context}.body"),
        )?
        .to_owned(),
        cursor: usize::try_from(as_u64(
            required(object, "cursor", context)?,
            &format!("{context}.cursor"),
        )?)
        .map_err(|_| invalid(format!("{context}.cursor is too large")))?,
        pasted_body: as_bool(
            required(object, "pasted_body", context)?,
            &format!("{context}.pasted_body"),
        )?,
        uncertain: as_bool(
            required(object, "uncertain", context)?,
            &format!("{context}.uncertain"),
        )?,
        revision: as_u64(
            required(object, "revision", context)?,
            &format!("{context}.revision"),
        )?,
        updated_offset_seconds: as_i64(
            required(object, "updated_offset_seconds", context)?,
            &format!("{context}.updated_offset_seconds"),
        )?,
    })
}

fn parse_parts(value: &Value, context: &str) -> Result<Vec<MessagePart>, SeedError> {
    let values = as_array(value, context)?;
    let mut parts = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let part_context = format!("{context}[{index}]");
        let object = as_object(value, &part_context)?;
        let kind = as_str(
            required(object, "type", &part_context)?,
            &format!("{part_context}.type"),
        )?;
        match kind {
            "text" => {
                let text = as_str(
                    required(object, "value", &part_context)?,
                    &format!("{part_context}.value"),
                )?;
                if text.is_empty() {
                    return Err(invalid(format!("{part_context} text must not be empty")));
                }
                parts.push(MessagePart::Text(text.to_owned()));
            }
            "attachment" => {
                let attachment_context = format!("{part_context}.value");
                let attachment = as_object(
                    required(object, "value", &part_context)?,
                    &attachment_context,
                )?;
                let id = as_str(
                    required(attachment, "id", &attachment_context)?,
                    &format!("{attachment_context}.id"),
                )?;
                if id.trim().is_empty() {
                    return Err(invalid(format!(
                        "{attachment_context}.id must not be empty"
                    )));
                }
                let mime_type = optional_str(
                    attachment,
                    "mime_type",
                    &format!("{attachment_context}.mime_type"),
                )?
                .map(str::to_owned);
                let file_name = optional_str(
                    attachment,
                    "file_name",
                    &format!("{attachment_context}.file_name"),
                )?
                .map(str::to_owned);
                let byte_len = optional_u64(
                    attachment,
                    "byte_len",
                    &format!("{attachment_context}.byte_len"),
                )?;
                if !mime_type
                    .as_deref()
                    .is_some_and(|mime| !mime.trim().is_empty() && mime.contains('/'))
                {
                    return Err(invalid(format!(
                        "{attachment_context}.mime_type must be a non-empty MIME type"
                    )));
                }
                if !file_name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                {
                    return Err(invalid(format!(
                        "{attachment_context}.file_name must be a non-empty synthetic name"
                    )));
                }
                if !byte_len.is_some_and(|len| len > 0) {
                    return Err(invalid(format!(
                        "{attachment_context}.byte_len must be positive"
                    )));
                }
                let attachment = AttachmentRef::new(id.to_owned(), mime_type, file_name, byte_len)
                    .map_err(|_| invalid(format!("{attachment_context} is not valid")))?;
                parts.push(MessagePart::Attachment(attachment));
            }
            other => {
                return Err(invalid(format!(
                    "{part_context} has unsupported part type `{other}`"
                )))
            }
        }
    }
    Ok(parts)
}

fn parse_reactions(value: Option<&Value>, context: &str) -> Result<Vec<ReactionSpec>, SeedError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = as_array(value, context)?;
    let mut reactions = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let reaction_context = format!("{context}[{index}]");
        let object = as_object(value, &reaction_context)?;
        let part = optional_u64(object, "part", &format!("{reaction_context}.part"))?
            .map(|part| {
                u32::try_from(part)
                    .map_err(|_| invalid(format!("{reaction_context}.part is too large")))
            })
            .transpose()?;
        reactions.push(ReactionSpec {
            sender: parse_handle(
                required(object, "sender", &reaction_context)?,
                &format!("{reaction_context}.sender"),
            )?,
            kind: as_str(
                required(object, "kind", &reaction_context)?,
                &format!("{reaction_context}.kind"),
            )?
            .to_owned(),
            remote_id: optional_str(
                object,
                "remote_id",
                &format!("{reaction_context}.remote_id"),
            )?
            .map(str::to_owned),
            part,
            timestamp_offset_seconds: optional_i64(
                object,
                "timestamp_offset_seconds",
                &format!("{reaction_context}.timestamp_offset_seconds"),
            )?,
        });
    }
    Ok(reactions)
}

fn validate(fixture: &Fixture) -> Result<(), SeedError> {
    if !(MIN_CONVERSATIONS..=MAX_CONVERSATIONS).contains(&fixture.conversations.len()) {
        return Err(invalid(format!(
            "expected {MIN_CONVERSATIONS}-{MAX_CONVERSATIONS} conversations, found {}",
            fixture.conversations.len()
        )));
    }
    let me_key = handle_key(&fixture.me);
    let mut conversation_ids = HashSet::new();
    let mut message_ids = HashSet::new();
    let mut pageable_history = false;
    let mut unread_incoming = false;
    let mut draft_count = 0usize;
    let mut attachment_count = 0usize;
    let mut reaction_count = 0usize;

    for conversation in &fixture.conversations {
        if !conversation_ids.insert(conversation.id.clone()) {
            return Err(invalid(format!(
                "conversation id `{}` is duplicated",
                conversation.id
            )));
        }
        if conversation.id.trim().is_empty() {
            return Err(invalid("conversation id must not be empty"));
        }
        if conversation.remote_id.trim().is_empty() {
            return Err(invalid(format!(
                "conversation `{}` remote_id must not be empty",
                conversation.id
            )));
        }
        if conversation
            .title
            .as_deref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err(invalid(format!(
                "conversation `{}` title must not be blank",
                conversation.id
            )));
        }
        if conversation.participants.len() < 2 {
            return Err(invalid(format!(
                "conversation `{}` needs at least two participants",
                conversation.id
            )));
        }

        let mut participant_keys = HashSet::new();
        let mut me_count = 0usize;
        let mut other_count = 0usize;
        for participant in &conversation.participants {
            validate_handle(
                &participant.handle,
                &format!("conversation `{}` participant", conversation.id),
            )?;
            if !participant_keys.insert(handle_key(&participant.handle)) {
                return Err(invalid(format!(
                    "conversation `{}` repeats participant `{}`",
                    conversation.id,
                    participant.handle.value()
                )));
            }
            if participant.is_me {
                me_count += 1;
                if handle_key(&participant.handle) != me_key {
                    return Err(invalid(format!(
                        "conversation `{}` marks a non-me handle as me",
                        conversation.id
                    )));
                }
            } else {
                other_count += 1;
            }
        }
        if me_count != 1 {
            return Err(invalid(format!(
                "conversation `{}` must mark exactly one participant as me",
                conversation.id
            )));
        }
        if !participant_keys.contains(&me_key) {
            return Err(invalid(format!(
                "conversation `{}` is missing the fixture me participant",
                conversation.id
            )));
        }
        match &conversation.kind {
            ConversationKind::Direct if other_count != 1 => {
                return Err(invalid(format!(
                    "direct conversation `{}` must have exactly one other participant",
                    conversation.id
                )));
            }
            ConversationKind::Group if other_count < 2 => {
                return Err(invalid(format!(
                    "group conversation `{}` needs at least two other participants",
                    conversation.id
                )));
            }
            _ => {}
        }

        if conversation.messages.is_empty() {
            return Err(invalid(format!(
                "conversation `{}` has no messages",
                conversation.id
            )));
        }
        if conversation.messages.len() >= MIN_PAGEABLE_HISTORY {
            pageable_history = true;
        }
        if conversation.created_offset_seconds > conversation.messages[0].offset_seconds {
            return Err(invalid(format!(
                "conversation `{}` was created after its first message",
                conversation.id
            )));
        }

        let mut conversation_message_ids = HashSet::new();
        let mut previous_offset = i64::MIN;
        for message in &conversation.messages {
            if message.id.trim().is_empty() {
                return Err(invalid(format!(
                    "conversation `{}` has a blank message id",
                    conversation.id
                )));
            }
            if !conversation_message_ids.insert(message.id.clone()) {
                return Err(invalid(format!(
                    "conversation `{}` repeats message id `{}`",
                    conversation.id, message.id
                )));
            }
            if !message_ids.insert(message.id.clone()) {
                return Err(invalid(format!(
                    "message id `{}` is duplicated across conversations",
                    message.id
                )));
            }
            if message.offset_seconds < previous_offset {
                return Err(invalid(format!(
                    "conversation `{}` messages are not in timestamp order",
                    conversation.id
                )));
            }
            previous_offset = message.offset_seconds;
            if message.offset_seconds > 0 {
                return Err(invalid(format!(
                    "message `{}` is scheduled after the session anchor",
                    message.id
                )));
            }
            for (field, offset) in [
                ("sent_offset_seconds", message.sent_offset_seconds),
                ("edited_offset_seconds", message.edited_offset_seconds),
                ("unsent_offset_seconds", message.unsent_offset_seconds),
            ] {
                if offset.is_some_and(|offset| offset < message.offset_seconds) {
                    return Err(invalid(format!(
                        "message `{}` {field} precedes its creation",
                        message.id
                    )));
                }
            }
            let sender = conversation
                .participants
                .iter()
                .find(|participant| participant.handle == message.sender)
                .ok_or_else(|| {
                    invalid(format!(
                        "message `{}` sender `{}` is not a participant of `{}`",
                        message.id,
                        message.sender.value(),
                        conversation.id
                    ))
                })?;
            if sender.is_me != message.from_me {
                return Err(invalid(format!(
                    "message `{}` from_me disagrees with its participant",
                    message.id
                )));
            }
            if message.from_me != (handle_key(&message.sender) == me_key) {
                return Err(invalid(format!(
                    "message `{}` from_me disagrees with the fixture me handle",
                    message.id
                )));
            }
            if message.parts.is_empty() {
                return Err(invalid(format!("message `{}` has no parts", message.id)));
            }
            for part in &message.parts {
                if let MessagePart::Attachment(_) = part {
                    attachment_count += 1;
                }
            }
            for reaction in &message.reactions {
                reaction_count += 1;
                if !conversation
                    .participants
                    .iter()
                    .any(|participant| participant.handle == reaction.sender)
                {
                    return Err(invalid(format!(
                        "message `{}` reaction sender `{}` is not a participant",
                        message.id,
                        reaction.sender.value()
                    )));
                }
                if reaction.kind.trim().is_empty() {
                    return Err(invalid(format!(
                        "message `{}` has a blank reaction kind",
                        message.id
                    )));
                }
                if reaction
                    .part
                    .is_some_and(|part| part as usize >= message.parts.len())
                {
                    return Err(invalid(format!(
                        "message `{}` reaction targets a missing part",
                        message.id
                    )));
                }
                if reaction
                    .timestamp_offset_seconds
                    .is_some_and(|offset| offset < message.offset_seconds)
                {
                    return Err(invalid(format!(
                        "message `{}` reaction precedes the message",
                        message.id
                    )));
                }
            }
        }

        if let Some(marker) = &conversation.read_marker {
            let marker_index = conversation
                .messages
                .iter()
                .position(|message| &message.id == marker)
                .ok_or_else(|| {
                    invalid(format!(
                        "conversation `{}` read marker `{marker}` does not resolve",
                        conversation.id
                    ))
                })?;
            if conversation.messages[marker_index + 1..]
                .iter()
                .any(|message| !message.from_me)
            {
                unread_incoming = true;
            }
        }

        if let Some(draft) = &conversation.draft {
            draft_count += 1;
            if draft.body.is_empty() {
                return Err(invalid(format!(
                    "conversation `{}` draft body must not be empty",
                    conversation.id
                )));
            }
            if draft.body.len() > MAX_DRAFT_BYTES {
                return Err(invalid(format!(
                    "conversation `{}` draft exceeds the byte limit",
                    conversation.id
                )));
            }
            if draft.cursor > draft.body.graphemes(true).count() {
                return Err(invalid(format!(
                    "conversation `{}` draft cursor is past the body",
                    conversation.id
                )));
            }
            if draft.revision == 0 {
                return Err(invalid(format!(
                    "conversation `{}` draft revision must be positive",
                    conversation.id
                )));
            }
            if draft.updated_offset_seconds > 0 || draft.updated_offset_seconds < -MAX_DRAFT_AGE_MS
            {
                return Err(invalid(format!(
                    "conversation `{}` draft is outside the retention window",
                    conversation.id
                )));
            }
        }
    }

    if !pageable_history {
        return Err(invalid(format!(
            "fixture needs one conversation with at least {MIN_PAGEABLE_HISTORY} messages"
        )));
    }
    if !unread_incoming {
        return Err(invalid(
            "fixture must leave at least one incoming message unread",
        ));
    }
    if draft_count == 0 {
        return Err(invalid("fixture must include a parked draft"));
    }
    if attachment_count == 0 {
        return Err(invalid("fixture must include named synthetic attachments"));
    }
    if reaction_count == 0 {
        return Err(invalid("fixture must include read-only reactions"));
    }
    Ok(())
}

fn validate_handle(handle: &Handle, context: &str) -> Result<(), SeedError> {
    if handle.kind() != &HandleKind::Email || !handle.value().ends_with("@example.invalid") {
        return Err(invalid(format!(
            "{context} must be a reserved example.invalid email handle"
        )));
    }
    Ok(())
}

fn handle_key(handle: &Handle) -> String {
    let kind = match handle.kind() {
        HandleKind::Tel => "tel",
        HandleKind::Email => "email",
        HandleKind::Opaque => "opaque",
    };
    format!("{kind}:{}", handle.value())
}

fn invalid(reason: impl Into<String>) -> SeedError {
    SeedError::Invalid(reason.into())
}

fn required<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a Value, SeedError> {
    object
        .get(key)
        .ok_or_else(|| invalid(format!("{context} is missing `{key}`")))
}

fn optional<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).filter(|value| !value.is_null())
}

fn as_object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, SeedError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{context} must be an object")))
}

fn as_array<'a>(value: &'a Value, context: &str) -> Result<&'a Vec<Value>, SeedError> {
    value
        .as_array()
        .ok_or_else(|| invalid(format!("{context} must be an array")))
}

fn as_str<'a>(value: &'a Value, context: &str) -> Result<&'a str, SeedError> {
    value
        .as_str()
        .ok_or_else(|| invalid(format!("{context} must be a string")))
}

fn as_bool(value: &Value, context: &str) -> Result<bool, SeedError> {
    value
        .as_bool()
        .ok_or_else(|| invalid(format!("{context} must be a boolean")))
}

fn as_i64(value: &Value, context: &str) -> Result<i64, SeedError> {
    value
        .as_i64()
        .ok_or_else(|| invalid(format!("{context} must be an integer")))
}

fn as_u64(value: &Value, context: &str) -> Result<u64, SeedError> {
    value
        .as_u64()
        .ok_or_else(|| invalid(format!("{context} must be a non-negative integer")))
}

fn optional_str<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, SeedError> {
    optional(object, key)
        .map(|value| as_str(value, context))
        .transpose()
}

fn optional_i64(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Option<i64>, SeedError> {
    optional(object, key)
        .map(|value| as_i64(value, context))
        .transpose()
}

fn optional_u64(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Option<u64>, SeedError> {
    optional(object, key)
        .map(|value| as_u64(value, context))
        .transpose()
}

fn parse_handle(value: &Value, context: &str) -> Result<Handle, SeedError> {
    serde_json::from_value(value.clone())
        .map_err(|_| invalid(format!("{context} is not a valid handle")))
}

fn parse_kind(value: &Value, context: &str) -> Result<ConversationKind, SeedError> {
    serde_json::from_value(value.clone())
        .map_err(|_| invalid(format!("{context} must be `direct` or `group`")))
}

fn parse_delivery(value: &Value, context: &str) -> Result<DeliveryState, SeedError> {
    serde_json::from_value(value.clone())
        .map_err(|_| invalid(format!("{context} is not a valid delivery state")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{history, history_before, list_conversations, load_draft, search_messages};
    use std::path::PathBuf;

    const ANCHOR: i64 = 1_700_000_000_000;

    fn database_path() -> (tempfile::TempDir, PathBuf) {
        let owner = tempfile::tempdir().unwrap();
        let private = owner.path().join("private");
        std::fs::create_dir(&private).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        (owner, private.join("store.db"))
    }

    fn seeded() -> (tempfile::TempDir, EncryptedStore, SeedSummary) {
        let (owner, path) = database_path();
        let mut store = EncryptedStore::open_demo(&path).unwrap();
        let summary = seed(&mut store, ANCHOR).unwrap();
        (owner, store, summary)
    }

    fn fixture_value() -> Value {
        serde_json::from_str(FIXTURE_JSON).unwrap()
    }

    #[test]
    fn fixture_referential_integrity_passes_and_rejects_corruption() {
        validate_fixture(FIXTURE_JSON).unwrap();

        let mut value = fixture_value();
        value["conversations"][0]["messages"][0]["sender"]["value"] =
            Value::String("intruder@example.invalid".into());
        let corrupted = serde_json::to_string(&value).unwrap();
        let error = validate_fixture(&corrupted).unwrap_err().to_string();
        assert!(
            error.contains("sender") && error.contains("participant"),
            "unexpected validator error: {error}"
        );

        let mut value = fixture_value();
        value["conversations"][0]["messages"][5]["parts"][1]["value"]["file_name"] = Value::Null;
        let corrupted = serde_json::to_string(&value).unwrap();
        let error = validate_fixture(&corrupted).unwrap_err().to_string();
        assert!(
            error.contains("file_name"),
            "unexpected validator error: {error}"
        );
    }

    #[test]
    fn seeding_is_deterministic_for_a_fixed_anchor() {
        let (_owner_a, mut store_a, summary_a) = seeded();
        let (_owner_b, mut store_b, summary_b) = seeded();
        assert_eq!(summary_a, summary_b);

        let conversations_a = list_conversations(&store_a).unwrap();
        let conversations_b = list_conversations(&store_b).unwrap();
        assert_eq!(conversations_a, conversations_b);

        for conversation in &conversations_a {
            assert_eq!(
                history(&store_a, &conversation.id, 500).unwrap(),
                history(&store_b, &conversation.id, 500).unwrap()
            );
            let key = DraftKey::Conversation(conversation.id.clone());
            assert_eq!(
                load_draft(&mut store_a, &key, ANCHOR).unwrap(),
                load_draft(&mut store_b, &key, ANCHOR).unwrap()
            );
        }
    }

    #[test]
    fn seeded_content_has_the_demo_shape() {
        let (_owner, mut store, summary) = seeded();
        assert_eq!(summary.conversations.len(), 5);
        assert_eq!(summary.message_count, 247);
        assert_eq!(summary.draft_count, 1);
        assert_eq!(summary.read_positions.len(), 5);

        let conversations = list_conversations(&store).unwrap();
        assert_eq!(conversations.len(), 5);
        assert!(conversations
            .iter()
            .any(|conversation| conversation.title.as_deref() == Some("Weekend crew")));

        let long = conversations
            .iter()
            .find(|conversation| conversation.title.as_deref() == Some("Harbour log"))
            .expect("longer history conversation");
        let long_history = history(&store, &long.id, 500).unwrap();
        assert_eq!(long_history.len(), 205);
        let page = history(&store, &long.id, 200).unwrap();
        assert_eq!(page.len(), 200);
        let oldest = page.first().unwrap();
        let older = history_before(&store, &long.id, oldest.created_at, oldest.id(), 200).unwrap();
        assert_eq!(older.len(), 5, "the long history must page past 200 rows");
        assert!(!search_messages(&store, "harbour", 10).unwrap().is_empty());

        let mut unread_conversations = 0;
        for (conversation_id, marker, _created_at) in &summary.read_positions {
            let messages = history(&store, conversation_id, 500).unwrap();
            let marker_index = messages
                .iter()
                .position(|message| message.id() == marker)
                .expect("read marker resolves");
            if messages[marker_index + 1..]
                .iter()
                .any(|message| !message.from_me)
            {
                unread_conversations += 1;
            }
        }
        assert!(
            unread_conversations >= 3,
            "expected several chats with unread incoming messages"
        );

        let mut drafts = 0;
        let mut attachments = 0;
        let mut reactions = 0;
        let mut multiline = false;
        for conversation in &conversations {
            let key = DraftKey::Conversation(conversation.id.clone());
            if load_draft(&mut store, &key, ANCHOR)
                .unwrap()
                .draft
                .is_some()
            {
                drafts += 1;
            }
            for message in history(&store, &conversation.id, 500).unwrap() {
                reactions += message.reactions.len();
                for part in message.parts() {
                    match part {
                        MessagePart::Text(text) => multiline |= text.contains('\n'),
                        MessagePart::Attachment(attachment) => {
                            attachments += 1;
                            assert!(!attachment.id().is_empty());
                            assert!(attachment
                                .file_name()
                                .is_some_and(|name| !name.trim().is_empty()));
                            let encoded = serde_json::to_value(attachment).unwrap();
                            assert!(encoded["mime_type"]
                                .as_str()
                                .is_some_and(|mime| mime.contains('/')));
                            assert!(encoded["byte_len"].as_u64().is_some_and(|len| len > 0));
                        }
                        MessagePart::Unsupported { .. } => {}
                    }
                }
            }
        }
        assert_eq!(drafts, 1, "one parked draft");
        assert!(attachments >= 4, "named synthetic attachments");
        assert!(reactions >= 5, "read-only reactions");
        assert!(multiline, "multiline message text");
    }
}
