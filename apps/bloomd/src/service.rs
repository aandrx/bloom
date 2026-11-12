//! Request handling over the encrypted store and the engine transport.
//!
//! Ordering rule: an event is persisted before it is published. A client must never
//! observe an event that was not committed, because a subscription client can
//! reconnect and re-read from the store.

use crate::{
    engine::{engine_slot, outgoing_text, Engine, EngineSlot},
    ipc::{HandlerFuture, RequestHandler, SubscriptionHub},
    readiness::{Finish, Readiness},
    store::{self, EncryptedStore},
};
use bloom_domain::{
    AttachmentRef, Conversation, ConversationId, ConversationKind, DeliveryState, Handle,
    HandleKind, Message, MessageId, MessagePart, OutgoingBody, Participant, Reaction, SessionState,
};
use bloom_ipc::{
    DraftKey as IpcDraftKey, DraftLoad as IpcDraftLoad, DraftWriteOutcome as IpcDraftWriteOutcome,
    DurableDraft as IpcDurableDraft, Event, EventKind, HistoryCursor, MessageSearchHit,
    ParticipantFilter, ProtocolError, ReadinessSnapshot,
};
use bloom_transport::{TransportError, TransportEvent};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

/// Engine-facing backend behind a [`Service`].
///
/// Normal mode holds the slot the recovery loop can replace. Demo mode has no
/// engine at all: there is no configuration, spawn, supervisor, recovery, or
/// discovery code on that path, and every engine-facing operation reports the
/// existing unsupported error. Store-backed history, search, drafts, and
/// persist-before-fanout stay shared between both variants.
enum Backend {
    Real(EngineSlot),
    Demo,
}

impl Backend {
    /// Clone the current engine out of the slot, if this backend has one.
    fn engine(&self) -> Option<Arc<Engine>> {
        match self {
            Self::Real(slot) => Some(Arc::clone(&slot.read().unwrap())),
            Self::Demo => None,
        }
    }

    fn is_demo(&self) -> bool {
        matches!(self, Self::Demo)
    }
}

pub struct Service {
    store: Arc<Mutex<EncryptedStore>>,
    backend: Backend,
    hub: SubscriptionHub,
    readiness: Readiness,
}

impl Service {
    pub fn new(
        store: Arc<Mutex<EncryptedStore>>,
        engine: Arc<Engine>,
        hub: SubscriptionHub,
    ) -> Self {
        let readiness = Readiness::new_ready(1, hub.clone());
        Self {
            store,
            backend: Backend::Real(engine_slot(engine)),
            hub,
            readiness,
        }
    }

    /// Build a service over a caller-owned slot, so a future generation can replace
    /// the engine without rebuilding the service.
    pub fn with_engine_slot(
        store: Arc<Mutex<EncryptedStore>>,
        engine: EngineSlot,
        hub: SubscriptionHub,
    ) -> Self {
        let readiness = Readiness::new_ready(1, hub.clone());
        Self {
            store,
            backend: Backend::Real(engine),
            hub,
            readiness,
        }
    }

    /// Production constructor: the caller owns the shared readiness state so the
    /// startup and recovery tasks publish into the same revisioned snapshot.
    pub fn with_engine_slot_and_readiness(
        store: Arc<Mutex<EncryptedStore>>,
        engine: EngineSlot,
        hub: SubscriptionHub,
        readiness: Readiness,
    ) -> Self {
        Self {
            store,
            backend: Backend::Real(engine),
            hub,
            readiness,
        }
    }

    /// Build the demo service: the same store-backed request handling over the
    /// synthetic store, with no engine and no engine configuration.
    ///
    /// Readiness starts ready so history, search, drafts, and listings work
    /// immediately; the demo backend rejects every engine-facing operation with
    /// the existing unsupported error before any preparation or store work.
    pub fn demo(store: Arc<Mutex<EncryptedStore>>, hub: SubscriptionHub) -> Self {
        let readiness = Readiness::new_ready(1, hub.clone());
        Self {
            store,
            backend: Backend::Demo,
            hub,
            readiness,
        }
    }

    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    pub fn readiness_snapshot(&self) -> ReadinessSnapshot {
        self.readiness.snapshot()
    }

    /// Run one explicit startup metadata refresh for `generation`. The engine's
    /// `sync_lock` serializes this with a manual sync, and the readiness token
    /// makes a late result from an older generation or operation inert.
    pub async fn run_sync_for_generation(self: Arc<Self>, generation: u64) {
        // The demo backend has no engine, so there is nothing to refresh.
        let Some(engine) = self.current_engine() else {
            return;
        };
        let Some(token) = self.readiness.begin(generation) else {
            return;
        };
        if engine.generation() != generation {
            self.readiness.finish(token, Finish::Unavailable);
            return;
        }
        let result = engine.sync_conversations().await;
        let finish = match result {
            Ok((limited, count)) => {
                if limited {
                    Finish::Limited(count)
                } else {
                    Finish::Complete(count)
                }
            }
            Err(TransportError::TransportUnavailable) => Finish::Unavailable,
            Err(TransportError::Timeout) => Finish::Timeout,
            Err(_) => Finish::Failed,
        };
        self.readiness.finish(token, finish);
    }

    /// Clone the current engine out of the slot. The lock is released here, so no
    /// caller holds it across an await. `None` means the demo backend, which has
    /// no engine.
    fn current_engine(&self) -> Option<Arc<Engine>> {
        self.backend.engine()
    }

    /// Persist a transport event, then fan it out. Returns whether anything changed.
    pub fn persist(&self, event: &TransportEvent) -> Result<bool, store::StoreError> {
        match event {
            TransportEvent::Message {
                conversation,
                message,
                part_ids,
                push_sequence,
            } => self.persist_message(conversation, message, part_ids, *push_sequence),
            TransportEvent::MessageUpdated {
                conversation,
                message,
            } => self.persist_message(conversation, message, &[], None),
            TransportEvent::DeliveryChanged {
                conversation,
                message,
                delivery,
            } => {
                let (canonical_id, updated) = {
                    let mut store = self.store.lock().unwrap();
                    let canonical_id = store::get_conversation(&store, conversation)?
                        .map(|row| row.id)
                        .unwrap_or_else(|| conversation.clone());
                    let updated = store::update_delivery(
                        &mut store,
                        conversation,
                        message,
                        delivery.clone(),
                    )?;
                    (canonical_id, updated)
                };
                if let Some(message) = updated {
                    self.hub.publish(Event {
                        event: EventKind::Delivery {
                            conversation_id: canonical_id,
                            message_id: message.id().clone(),
                            state: delivery.clone(),
                        },
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            TransportEvent::Conversation { conversation } => {
                let mut store = self.store.lock().unwrap();
                let (_, linked) = store::reconcile_direct_conversation(&mut store, conversation)?;
                log_identity_links(linked);
                Ok(false)
            }
            TransportEvent::ConversationSyncComplete { completion } => {
                let published = !completion.persistence_failed();
                if published {
                    self.hub.publish(Event {
                        event: EventKind::ConversationsChanged,
                    });
                }
                completion.acknowledge();
                Ok(published)
            }
            TransportEvent::ConversationsChanged => {
                // One refresh for a whole listing sync, after the rows are committed.
                self.hub.publish(Event {
                    event: EventKind::ConversationsChanged,
                });
                Ok(true)
            }
            TransportEvent::Session {
                generation, state, ..
            } => {
                if *generation != self.readiness.current_generation() {
                    return Ok(false);
                }
                self.hub.publish(Event {
                    event: EventKind::Session {
                        state: state.clone(),
                    },
                });
                // Active is independent engine readiness; lost or failed sessions
                // invalidate it. Routine local events never close the gate.
                if matches!(state, SessionState::Active) {
                    self.readiness.mark_engine_ready_current();
                } else if matches!(state, SessionState::Disconnected | SessionState::Failed) {
                    self.readiness.session_lost();
                }
                Ok(true)
            }
            TransportEvent::ReactionChanged {
                conversation_id,
                message_id,
                sender,
                kind,
                active,
                part,
                at,
            } => {
                let (canonical_id, stored) = {
                    let mut store = self.store.lock().unwrap();
                    let Some(conversation) = store::get_conversation(&store, conversation_id)?
                    else {
                        return Ok(false);
                    };
                    let Some(message) = store::get_message(&store, message_id)? else {
                        return Ok(false);
                    };
                    if message.conversation_id() != &conversation.id
                        && !conversation.aliases.contains(message.conversation_id())
                    {
                        return Ok(false);
                    }
                    let reaction = Reaction {
                        sender: sender.clone(),
                        kind: kind.clone(),
                        remote_id: None,
                        part: *part,
                        timestamp: *at,
                    };
                    let stored = store.apply_reaction(message_id, &reaction, *active)?;
                    (conversation.id, stored)
                };
                let Some(message) = stored else {
                    return Ok(false);
                };
                self.hub.publish(Event {
                    event: EventKind::Update {
                        conversation_id: canonical_id,
                        message,
                    },
                });
                Ok(true)
            }
            TransportEvent::MessageEdited {
                conversation_id,
                message_id,
                sender,
                part,
                replacement_parts,
                at,
            } => {
                let (canonical_id, stored) = {
                    let mut store = self.store.lock().unwrap();
                    let Some(conversation) = store::get_conversation(&store, conversation_id)?
                    else {
                        return Ok(false);
                    };
                    let target = store::MutationTarget {
                        message_id,
                        conversation_id: &conversation.id,
                        aliases: &conversation.aliases,
                        sender,
                        part: *part,
                        at: *at,
                    };
                    let stored = store.apply_edit(&target, replacement_parts)?;
                    (conversation.id, stored)
                };
                let Some(message) = stored else {
                    return Ok(false);
                };
                self.hub.publish(Event {
                    event: EventKind::Update {
                        conversation_id: canonical_id,
                        message,
                    },
                });
                Ok(true)
            }
            TransportEvent::MessageUnsent {
                conversation_id,
                message_id,
                sender,
                part,
                at,
            } => {
                let (canonical_id, stored) = {
                    let mut store = self.store.lock().unwrap();
                    let Some(conversation) = store::get_conversation(&store, conversation_id)?
                    else {
                        return Ok(false);
                    };
                    let target = store::MutationTarget {
                        message_id,
                        conversation_id: &conversation.id,
                        aliases: &conversation.aliases,
                        sender,
                        part: *part,
                        at: *at,
                    };
                    let stored = store.apply_unsend(&target)?;
                    (conversation.id, stored)
                };
                let Some(message) = stored else {
                    return Ok(false);
                };
                self.hub.publish(Event {
                    event: EventKind::Update {
                        conversation_id: canonical_id,
                        message,
                    },
                });
                Ok(true)
            }
        }
    }

    fn persist_message(
        &self,
        conversation: &Conversation,
        message: &Message,
        part_ids: &[Option<u64>],
        push_sequence: Option<u64>,
    ) -> Result<bool, store::StoreError> {
        let persistence_started = Instant::now();
        let (outcome, canonical, stored_message) = {
            let mut store = self.store.lock().unwrap();
            let (canonical, linked) =
                store::reconcile_direct_conversation(&mut store, conversation)?;
            log_identity_links(linked);
            let message_conversation = message.conversation_id();
            if message_conversation != &canonical.id
                && !canonical.aliases.contains(message_conversation)
            {
                return Err(store::StoreError::MessageConversationMismatch);
            }
            let message = store::remap_message(message, &canonical.id)?;
            let (outcome, _) =
                store::ingest_inbound_with_parts(&mut store, &canonical, &message, part_ids)?;
            (outcome, canonical, message)
        };
        if crate::diagnostics::enabled() {
            if let Some(sequence) = push_sequence {
                eprintln!(
                    "bloom timing daemon persist_us={} sequence={sequence}",
                    crate::diagnostics::elapsed_micros(persistence_started)
                );
            }
        }
        if matches!(outcome, store::MessageUpsertOutcome::Unchanged) {
            return Ok(false);
        }
        let publish_started = Instant::now();
        self.hub.publish(Event {
            event: EventKind::Message {
                conversation_id: canonical.id,
                message: stored_message,
                push_sequence,
            },
        });
        if crate::diagnostics::enabled() {
            if let Some(sequence) = push_sequence {
                eprintln!(
                    "bloom timing daemon publish_us={} sequence={sequence}",
                    crate::diagnostics::elapsed_micros(publish_started)
                );
            }
        }
        Ok(true)
    }

    pub async fn persist_events(
        self: Arc<Self>,
        mut events: tokio::sync::mpsc::Receiver<TransportEvent>,
    ) {
        let mut conversation_persistence_failed = false;
        while let Some(event) = events.recv().await {
            if let TransportEvent::ConversationSyncComplete { completion } = &event {
                if conversation_persistence_failed {
                    completion.mark_persistence_failed();
                }
                conversation_persistence_failed = false;
            }
            let is_conversation = matches!(&event, TransportEvent::Conversation { .. });
            let reconnect_candidate = matches!(
                &event,
                TransportEvent::Session {
                    generation,
                    state: SessionState::Active,
                    ..
                } if *generation == self.readiness.current_generation()
            );
            let persisted = match self.persist(&event) {
                Ok(persisted) => persisted,
                Err(error) => {
                    if is_conversation {
                        conversation_persistence_failed = true;
                    }
                    eprintln!("bloomd: dropping event that failed to persist: {error}");
                    false
                }
            };
            // Reserve the refresh only after the event was accepted for this
            // generation. A stale Active must not consume the reconnect slot.
            let reconnect = persisted && reconnect_candidate && self.readiness.begin_reconnect();
            if reconnect {
                // A session can return on the same engine generation after a
                // transport loss. Re-run the required refresh; sends stay gated
                // until it completes. Initial Active is not a reconnect because
                // the readiness phase is Refreshing, not Failed/Disconnected.
                let service = self.clone();
                let generation = self.readiness.current_generation();
                tokio::spawn(async move {
                    service.run_sync_for_generation(generation).await;
                });
            }
        }
    }

    fn send_blocking_prepare(
        &self,
        conversation_id: &ConversationId,
        body: &OutgoingBody,
    ) -> Result<(String, Vec<String>, Conversation), ProtocolError> {
        let text = outgoing_text(body).map_err(|_| ProtocolError::Unsupported)?;
        let (recipients, conversation) = self.send_targets(conversation_id)?;
        Ok((text, recipients, conversation))
    }

    fn send_targets(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<(Vec<String>, Conversation), ProtocolError> {
        let store = self.store.lock().unwrap();
        let conversation = store::get_conversation(&store, conversation_id)
            .map_err(|_| ProtocolError::Internal)?
            .ok_or(ProtocolError::NotFound)?;
        let recipients: Vec<String> = conversation
            .participants
            .iter()
            .filter(|participant| !participant.is_me)
            .map(|participant| rust_handle(&participant.handle))
            .collect();
        if recipients.is_empty() {
            return Err(ProtocolError::NotFound);
        }
        Ok((recipients, conversation))
    }
}

/// The sidecar re-validates before reading. This bound keeps a mistyped path
/// from becoming a multi-hour upload.
const MAX_OUTBOUND_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

fn outgoing_attachment_file(path: &str) -> Result<(String, u64), ProtocolError> {
    let candidate = std::path::Path::new(path);
    // Check raw segments: `Path::components` normalizes interior dots, which
    // would let `/tmp/./file` pass a components-only rule.
    if !candidate.is_absolute() || path.split('/').any(|part| part == "." || part == "..") {
        return Err(ProtocolError::InvalidRequest);
    }
    let metadata =
        std::fs::symlink_metadata(candidate).map_err(|_| ProtocolError::InvalidRequest)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProtocolError::InvalidRequest);
    }
    let byte_len = metadata.len();
    if byte_len == 0 || byte_len > MAX_OUTBOUND_ATTACHMENT_BYTES {
        return Err(ProtocolError::InvalidRequest);
    }
    let name = candidate
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .ok_or(ProtocolError::InvalidRequest)?;
    Ok((name.to_owned(), byte_len))
}

/// A one-line preview of a message for the sidebar's second line. Our own
/// messages are prefixed, the way a chat list usually reads.
fn preview_text(message: &Message) -> String {
    let body = message
        .parts()
        .iter()
        .map(|part| match part {
            bloom_domain::MessagePart::Text(text) => text.clone(),
            bloom_domain::MessagePart::Attachment(_) => "[attachment]".to_owned(),
            bloom_domain::MessagePart::Unsupported { .. } => "[attachment]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    if message.from_me {
        format!("You: {body}")
    } else {
        body
    }
}

impl RequestHandler for Service {
    fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>> {
        Box::pin(async move {
            let mut store = self.store.lock().unwrap();
            let linked = match store::reconcile_stored_direct_conversations(&mut store) {
                Ok(linked) => linked,
                Err(error) => {
                    eprintln!(
                        "bloomd: direct identity reconciliation failed during listing: {error}"
                    );
                    return Err(ProtocolError::Internal);
                }
            };
            log_identity_links(linked);
            let mut conversations =
                store::list_conversations(&store).map_err(|_| ProtocolError::Internal)?;
            // The engine's listing carries no message text, so the preview comes
            // from Bloom's own store. A conversation Bloom has never seen a message
            // for keeps a null preview and the sidebar shows its name alone.
            for conversation in &mut conversations {
                conversation.preview = store::history_for_conversation(&store, conversation, 1)
                    .map_err(|_| ProtocolError::Internal)?
                    .last()
                    .map(preview_text);
            }
            Ok(conversations)
        })
    }

    fn sync_conversations(&self) -> HandlerFuture<'_, (bloom_ipc::ConversationSyncStatus, u32)> {
        Box::pin(async move {
            // Demo has no engine transport to sync against; reject before the
            // readiness gate moves or any store work happens.
            if self.backend.is_demo() {
                return Err(ProtocolError::Unsupported);
            }
            let generation = self.readiness.current_generation();
            let Some(token) = self.readiness.begin(generation) else {
                return Err(ProtocolError::NotReady);
            };
            let engine = self.current_engine().ok_or(ProtocolError::Unsupported)?;
            if engine.generation() != generation {
                self.readiness.finish(token, Finish::Unavailable);
                return Err(ProtocolError::NotReady);
            }
            let result = engine.sync_conversations().await;
            let finish = match result {
                Ok((limited, count)) => {
                    if limited {
                        Finish::Limited(count)
                    } else {
                        Finish::Complete(count)
                    }
                }
                Err(TransportError::TransportUnavailable) => Finish::Unavailable,
                Err(TransportError::Timeout) => Finish::Timeout,
                Err(_) => Finish::Failed,
            };
            self.readiness.finish(token, finish);
            let (limited, count) = result.map_err(map_transport_error)?;
            // The engine resolves only after its ordered completion marker is persisted and
            // the conversation-list refresh event has been published.
            Ok((
                if limited {
                    bloom_ipc::ConversationSyncStatus::Limited
                } else {
                    bloom_ipc::ConversationSyncStatus::Complete
                },
                count,
            ))
        })
    }

    fn readiness(&self) -> HandlerFuture<'_, ReadinessSnapshot> {
        Box::pin(async move { Ok(self.readiness.snapshot()) })
    }

    fn history(
        &self,
        conversation_id: ConversationId,
        limit: u16,
    ) -> HandlerFuture<'_, Vec<Message>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::history(&store, &conversation_id, limit).map_err(|_| ProtocolError::Internal)
        })
    }

    fn history_before(
        &self,
        conversation_id: ConversationId,
        limit: u16,
        before: HistoryCursor,
    ) -> HandlerFuture<'_, Vec<Message>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::history_before(
                &store,
                &conversation_id,
                before.created_at,
                &before.id,
                limit,
            )
            .map_err(|_| ProtocolError::Internal)
        })
    }

    fn search_messages(
        &self,
        query: String,
        limit: u16,
    ) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::search_messages(&store, &query, limit).map_err(|_| ProtocolError::Internal)
        })
    }

    fn search_messages_with_participant(
        &self,
        query: String,
        limit: u16,
        participant: Option<ParticipantFilter>,
    ) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::search_messages_with_participant(&store, &query, limit, participant.as_ref())
                .map_err(|_| ProtocolError::Internal)
        })
    }

    fn message_context(
        &self,
        conversation_id: ConversationId,
        message_id: MessageId,
        limit: u16,
    ) -> HandlerFuture<'_, Vec<Message>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::message_context(&store, &conversation_id, &message_id, limit)
                .map_err(|_| ProtocolError::Internal)?
                .ok_or(ProtocolError::NotFound)
        })
    }

    fn start_conversation(
        &self,
        address: String,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId> {
        Box::pin(async move {
            // Demo has no engine: reject before address parsing or store work.
            if self.backend.is_demo() {
                return Err(ProtocolError::Unsupported);
            }
            if !self.readiness.can_send() {
                return Err(ProtocolError::NotReady);
            }
            let generation = self.readiness.current_generation();
            let text = outgoing_text(&body).map_err(|_| ProtocolError::Unsupported)?;
            let (handle, recipient) = normalize_address(&address)?;
            let engine_transport = crate::engine::transport_id();
            // Resolve one engine for the whole operation: a swap between building
            // the participants and sending must not mix two generations.
            let engine = self.current_engine().ok_or(ProtocolError::Unsupported)?;
            if engine.generation() != generation {
                return Err(ProtocolError::NotReady);
            }
            let conversation = {
                let mut store = self.store.lock().unwrap();
                let linked = match store::reconcile_stored_direct_conversations(&mut store) {
                    Ok(linked) => linked,
                    Err(error) => {
                        eprintln!("bloomd: direct identity reconciliation failed before new chat: {error}");
                        return Err(ProtocolError::Internal);
                    }
                };
                log_identity_links(linked);
                let found = store::list_conversations(&store)
                    .map_err(|_| ProtocolError::Internal)?
                    .into_iter()
                    .filter(|conversation| {
                        conversation.kind == ConversationKind::Direct
                            && conversation.id.transport() == &engine_transport
                    })
                    .find(|conversation| {
                        crate::store::direct_conversation_matches(conversation, &handle)
                    });
                match found {
                    Some(conversation) => conversation,
                    None => {
                        let remote_id = format!("iMessage;-;{}", recipient);
                        let id = ConversationId::new(engine_transport.clone(), remote_id.clone())
                            .map_err(|_| ProtocolError::InvalidRequest)?;
                        Conversation {
                            id,
                            remote_id,
                            kind: ConversationKind::Direct,
                            title: None,
                            participants: {
                                let mut participants = vec![Participant {
                                    handle,
                                    display_name: None,
                                    is_me: false,
                                }];
                                if let Some(me) = engine.primary_handle() {
                                    participants.push(Participant {
                                        handle: me,
                                        display_name: None,
                                        is_me: true,
                                    });
                                }
                                participants
                            },
                            created_at: now_millis(),
                            last_message_at: None,
                            aliases: Vec::new(),
                            preview: None,
                        }
                    }
                }
            };
            let recipients = vec![recipient];
            let message_id = engine
                .send_text(&recipients, Some(&conversation.remote_id), &text)
                .await
                .map_err(map_transport_error)?;
            let (canonical, message) = {
                let mut store = self.store.lock().unwrap();
                let (canonical, linked) =
                    store::reconcile_direct_conversation(&mut store, &conversation)
                        .map_err(|_| ProtocolError::Internal)?;
                log_identity_links(linked);
                let sender = canonical
                    .participants
                    .iter()
                    .find(|participant| participant.is_me)
                    .map(|participant| participant.handle.clone())
                    .unwrap_or_else(|| {
                        Handle::new(HandleKind::Opaque, "me").expect("static handle is valid")
                    });
                let message = Message::new(
                    message_id.clone(),
                    canonical.id.clone(),
                    None,
                    sender,
                    body.parts.clone(),
                    now_millis(),
                    None,
                    DeliveryState::Sent,
                    true,
                    None,
                    Vec::new(),
                    None,
                )
                .map_err(|_| ProtocolError::Internal)?;
                store::ingest_inbound(&mut store, &canonical, &message)
                    .map_err(|_| ProtocolError::Internal)?;
                (canonical, message)
            };
            self.hub.publish(Event {
                event: EventKind::Message {
                    conversation_id: canonical.id,
                    message,
                    push_sequence: None,
                },
            });
            Ok(message_id)
        })
    }

    fn send(
        &self,
        conversation_id: ConversationId,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId> {
        Box::pin(async move {
            // Demo has no engine: reject before preparation or store lookup.
            if self.backend.is_demo() {
                return Err(ProtocolError::Unsupported);
            }
            if !self.readiness.can_send() {
                return Err(ProtocolError::NotReady);
            }
            let generation = self.readiness.current_generation();
            let (text, recipients, conversation) =
                self.send_blocking_prepare(&conversation_id, &body)?;

            let engine = self.current_engine().ok_or(ProtocolError::Unsupported)?;
            if engine.generation() != generation {
                return Err(ProtocolError::NotReady);
            }
            let message_id = engine
                .send_text(&recipients, Some(&conversation.remote_id), &text)
                .await
                .map_err(map_transport_error)?;

            // Record our own outbound message so history and the UI stay coherent.
            let sender = conversation
                .participants
                .iter()
                .find(|participant| participant.is_me)
                .map(|participant| participant.handle.clone())
                .unwrap_or_else(|| {
                    Handle::new(HandleKind::Opaque, "me").expect("static handle is valid")
                });
            let created_at = now_millis();
            let message = Message::new(
                message_id.clone(),
                conversation.id.clone(),
                None,
                sender,
                body.parts.clone(),
                created_at,
                None,
                DeliveryState::Sent,
                true,
                None,
                Vec::new(),
                None,
            )
            .map_err(|_| ProtocolError::Internal)?;

            {
                let mut store = self.store.lock().unwrap();
                store::ingest_inbound(&mut store, &conversation, &message)
                    .map_err(|_| ProtocolError::Internal)?;
            }
            self.hub.publish(Event {
                event: EventKind::Message {
                    conversation_id: conversation.id,
                    message,
                    push_sequence: None,
                },
            });
            Ok(message_id)
        })
    }

    fn send_attachment(
        &self,
        conversation_id: ConversationId,
        path: String,
    ) -> HandlerFuture<'_, MessageId> {
        Box::pin(async move {
            // Demo disables local-file upload: reject before touching the path.
            if self.backend.is_demo() {
                return Err(ProtocolError::Unsupported);
            }
            if !self.readiness.can_send() {
                return Err(ProtocolError::NotReady);
            }
            let generation = self.readiness.current_generation();
            let (name, byte_len) = outgoing_attachment_file(&path)?;
            let (recipients, conversation) = self.send_targets(&conversation_id)?;

            let engine = self.current_engine().ok_or(ProtocolError::Unsupported)?;
            if engine.generation() != generation {
                return Err(ProtocolError::NotReady);
            }
            let message_id = engine
                .send_attachment(&recipients, Some(&conversation.remote_id), &path)
                .await
                .map_err(map_transport_error)?;

            let sender = conversation
                .participants
                .iter()
                .find(|participant| participant.is_me)
                .map(|participant| participant.handle.clone())
                .unwrap_or_else(|| {
                    Handle::new(HandleKind::Opaque, "me").expect("static handle is valid")
                });
            let created_at = now_millis();
            let attachment = AttachmentRef::new(
                format!("local:{}", message_id.value()),
                None,
                Some(name),
                Some(byte_len),
            )
            .map_err(|_| ProtocolError::Internal)?;
            let message = Message::new(
                message_id.clone(),
                conversation.id.clone(),
                None,
                sender,
                vec![MessagePart::Attachment(attachment)],
                created_at,
                None,
                DeliveryState::Sent,
                true,
                None,
                Vec::new(),
                None,
            )
            .map_err(|_| ProtocolError::Internal)?;

            {
                let mut store = self.store.lock().unwrap();
                store::ingest_inbound(&mut store, &conversation, &message)
                    .map_err(|_| ProtocolError::Internal)?;
            }
            self.hub.publish(Event {
                event: EventKind::Message {
                    conversation_id: conversation.id,
                    message,
                    push_sequence: None,
                },
            });
            Ok(message_id)
        })
    }

    fn fetch_attachment(&self, token: String) -> HandlerFuture<'_, (String, u64)> {
        Box::pin(async move {
            // Demo disables external opening: no engine fetch, no file access.
            if self.backend.is_demo() {
                return Err(ProtocolError::Unsupported);
            }
            if !bloom_domain::AttachmentRef::is_valid_fetch_token(&token) {
                return Err(ProtocolError::InvalidRequest);
            }
            let engine = self.current_engine().ok_or(ProtocolError::Unsupported)?;
            match engine.fetch_attachment(&token).await {
                Ok(file) => Ok((file.path, file.bytes)),
                Err(error) => {
                    // Bounded transport kind only; never token, path or content.
                    eprintln!("attachment fetch failed: {error:?}");
                    match error {
                        TransportError::Unsupported => Err(ProtocolError::Unsupported),
                        _ => Err(ProtocolError::Internal),
                    }
                }
            }
        })
    }

    fn draft_save(
        &self,
        key: IpcDraftKey,
        draft: IpcDurableDraft,
        revision: u64,
    ) -> HandlerFuture<'_, IpcDraftWriteOutcome> {
        Box::pin(async move {
            let key = store_draft_key(key);
            let draft = store_draft(draft);
            let updated_at = draft_now_millis()?;
            let mut store = self.store.lock().unwrap();
            store::save_draft(&mut store, &key, &draft, revision, updated_at)
                .map_err(map_draft_store_error)
                .map(ipc_draft_outcome)
        })
    }

    fn draft_load(&self, key: IpcDraftKey) -> HandlerFuture<'_, IpcDraftLoad> {
        Box::pin(async move {
            let key = store_draft_key(key);
            let now = draft_now_millis()?;
            let mut store = self.store.lock().unwrap();
            let loaded = store::load_draft(&mut store, &key, now).map_err(map_draft_store_error)?;
            Ok(IpcDraftLoad {
                draft: loaded.draft.map(ipc_draft),
                revision_floor: loaded.revision_floor,
                corrupt_deleted: loaded.corrupt_deleted,
            })
        })
    }

    fn draft_delete(
        &self,
        key: IpcDraftKey,
        revision: u64,
    ) -> HandlerFuture<'_, IpcDraftWriteOutcome> {
        Box::pin(async move {
            let key = store_draft_key(key);
            let updated_at = draft_now_millis()?;
            let mut store = self.store.lock().unwrap();
            store::delete_draft(&mut store, &key, revision, updated_at)
                .map_err(map_draft_store_error)
                .map(ipc_draft_outcome)
        })
    }
}

fn store_draft_key(key: IpcDraftKey) -> store::DraftKey {
    match key {
        IpcDraftKey::Conversation { owner } => store::DraftKey::Conversation(owner),
        IpcDraftKey::NewChat { transport, address } => {
            store::DraftKey::NewChat { transport, address }
        }
    }
}

fn store_draft(draft: IpcDurableDraft) -> store::DurableDraft {
    store::DurableDraft {
        body: draft.body,
        cursor: draft.cursor,
        pasted_body: draft.pasted_body,
        uncertain: draft.uncertain,
    }
}

fn ipc_draft(draft: store::DurableDraft) -> IpcDurableDraft {
    IpcDurableDraft {
        body: draft.body,
        cursor: draft.cursor,
        pasted_body: draft.pasted_body,
        uncertain: draft.uncertain,
    }
}

fn ipc_draft_outcome(outcome: store::DraftWriteOutcome) -> IpcDraftWriteOutcome {
    match outcome {
        store::DraftWriteOutcome::Applied => IpcDraftWriteOutcome::Applied,
        store::DraftWriteOutcome::Stale => IpcDraftWriteOutcome::Stale,
    }
}

fn map_draft_store_error(error: store::StoreError) -> ProtocolError {
    match error {
        store::StoreError::InvalidDraft => ProtocolError::InvalidRequest,
        _ => ProtocolError::Internal,
    }
}

fn log_identity_links(linked: usize) {
    if linked > 0 {
        eprintln!(
            "bloomd: direct identity reconciliation outcome=linked aliases_added={linked} match=exact_handle history=retained"
        );
    }
}

fn map_transport_error(error: TransportError) -> ProtocolError {
    match error {
        TransportError::SessionExpired => ProtocolError::SessionExpired,
        TransportError::AccountThrottled => ProtocolError::AccountThrottled,
        TransportError::RecipientUnreachable => ProtocolError::RecipientUnreachable,
        TransportError::NotRegistered => ProtocolError::NotRegistered,
        TransportError::TransportUnavailable => ProtocolError::TransportUnavailable,
        TransportError::Timeout => ProtocolError::Protocol,
        TransportError::Unsupported => ProtocolError::Unsupported,
        TransportError::Protocol => ProtocolError::Protocol,
        TransportError::Internal => ProtocolError::Internal,
    }
}

fn normalize_address(address: &str) -> Result<(Handle, String), ProtocolError> {
    let value = address.trim();
    if value.is_empty() {
        return Err(ProtocolError::InvalidRequest);
    }
    let is_phone_like = !value.contains('@');
    // Humans type phone numbers with spaces and punctuation; normalise them away.
    let value: String = if is_phone_like {
        value
            .chars()
            .filter(|c| !c.is_whitespace() && !"().-".contains(*c))
            .collect()
    } else {
        value.to_owned()
    };
    let value = value.as_str();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(ProtocolError::InvalidRequest);
    }
    let (kind, value) = if let Some(v) = value.strip_prefix("mailto:") {
        if v.contains('@')
            && v.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
            })
        {
            (HandleKind::Email, v)
        } else {
            return Err(ProtocolError::InvalidRequest);
        }
    } else if let Some(v) = value.strip_prefix("tel:") {
        if v.chars().any(|c| c.is_ascii_digit())
            && v.chars()
                .all(|c| c.is_ascii_digit() || "+()- .".contains(c))
        {
            (HandleKind::Tel, v)
        } else {
            return Err(ProtocolError::InvalidRequest);
        }
    } else if value.contains('@') {
        if value.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }) {
            (HandleKind::Email, value)
        } else {
            return Err(ProtocolError::InvalidRequest);
        }
    } else if value.chars().any(|c| c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || "+()- .".contains(c))
    {
        (HandleKind::Tel, value)
    } else {
        return Err(ProtocolError::InvalidRequest);
    };
    let handle = Handle::new(kind, value).map_err(|_| ProtocolError::InvalidRequest)?;
    Ok((handle.clone(), rust_handle(&handle)))
}

/// Render a domain handle back into the engine's `mailto:`/`tel:` form.
pub fn rust_handle(handle: &Handle) -> String {
    match handle.kind() {
        HandleKind::Email => format!("mailto:{}", handle.value()),
        HandleKind::Tel => format!("tel:{}", handle.value()),
        HandleKind::Opaque => handle.value().to_owned(),
    }
}

fn draft_now_millis() -> Result<i64, ProtocolError> {
    let now = now_millis();
    if now <= 0 {
        return Err(ProtocolError::Internal);
    }
    Ok(now)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod sync_barrier_tests {
    use super::*;
    use bloom_domain::{ConversationId, HandleKind, TransportId};
    use bloom_transport::{SyncCompletion, TransportEvent};
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn sync_ack_follows_persisted_rows_and_refresh_publication() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[5; 32]).unwrap(),
        ));
        let engine = Arc::new(Engine::new(crate::engine::EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }));
        let hub = SubscriptionHub::new(4);
        let mut subscriber = hub.subscribe();
        let service = Arc::new(Service::new(store.clone(), engine, hub));
        let (events_tx, events_rx) = mpsc::channel(2);

        let transport = TransportId::new("openbubbles").unwrap();
        let conversation = Conversation {
            id: ConversationId::new(transport.clone(), "sync-group").unwrap(),
            remote_id: "iMessage;-;sync-group".into(),
            kind: ConversationKind::Group,
            title: Some("Synced group".into()),
            participants: vec![Participant {
                handle: Handle::new(HandleKind::Opaque, "person@example.invalid").unwrap(),
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: Some(1),
            aliases: Vec::new(),
            preview: None,
        };
        let completion = Arc::new(SyncCompletion::default());
        let wait_for_completion = completion.clone();
        events_tx
            .send(TransportEvent::Conversation { conversation })
            .await
            .unwrap();
        events_tx
            .send(TransportEvent::ConversationSyncComplete { completion })
            .await
            .unwrap();
        drop(events_tx);

        let persistence = tokio::spawn(service.clone().persist_events(events_rx));
        tokio::time::timeout(Duration::from_secs(1), wait_for_completion.wait())
            .await
            .expect("sync completion waits for ordered persistence");
        assert!(!wait_for_completion.persistence_failed());
        let stored_count = {
            let stored = store.lock().unwrap();
            store::list_conversations(&stored).unwrap().len()
        };
        assert_eq!(stored_count, 1);
        assert!(matches!(
            subscriber.recv().await.unwrap().event,
            EventKind::ConversationsChanged
        ));
        persistence.await.unwrap();
    }

    #[tokio::test]
    async fn sync_ack_reports_conversation_persistence_failure_without_refresh() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[6; 32]).unwrap(),
        ));
        let engine = Arc::new(Engine::new(crate::engine::EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }));
        let hub = SubscriptionHub::new(4);
        let mut subscriber = hub.subscribe();
        let service = Arc::new(Service::new(store.clone(), engine, hub));

        let transport = TransportId::new("openbubbles").unwrap();
        let person = Handle::new(HandleKind::Opaque, "person@example.invalid").unwrap();
        let direct = Conversation {
            id: ConversationId::new(transport.clone(), "shared-id").unwrap(),
            remote_id: "direct-chat".into(),
            kind: ConversationKind::Direct,
            title: None,
            participants: vec![Participant {
                handle: person.clone(),
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };
        service
            .persist(&TransportEvent::Conversation {
                conversation: direct.clone(),
            })
            .unwrap();
        let conflicting_group = Conversation {
            id: direct.id,
            remote_id: "group-chat".into(),
            kind: ConversationKind::Group,
            title: None,
            participants: vec![Participant {
                handle: person,
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };

        let (events_tx, events_rx) = mpsc::channel(2);
        let completion = Arc::new(SyncCompletion::default());
        let wait_for_completion = completion.clone();
        events_tx
            .send(TransportEvent::Conversation {
                conversation: conflicting_group,
            })
            .await
            .unwrap();
        events_tx
            .send(TransportEvent::ConversationSyncComplete { completion })
            .await
            .unwrap();
        drop(events_tx);

        let persistence = tokio::spawn(service.clone().persist_events(events_rx));
        tokio::time::timeout(Duration::from_secs(1), wait_for_completion.wait())
            .await
            .expect("failed row still releases the sync waiter");
        assert!(wait_for_completion.persistence_failed());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err()
        );
        assert_eq!(
            store::list_conversations(&store.lock().unwrap())
                .unwrap()
                .len(),
            1
        );
        persistence.await.unwrap();
    }
}

#[cfg(test)]
mod draft_service_tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::ipc::RequestHandler;
    use bloom_domain::TransportId;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    fn harness() -> (tempfile::TempDir, Arc<Service>) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[9; 32]).unwrap(),
        ));
        let engine = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }));
        let service = Arc::new(Service::new(store, engine, SubscriptionHub::new(4)));
        (directory, service)
    }

    fn conversation_key(value: &str) -> IpcDraftKey {
        IpcDraftKey::Conversation {
            owner: ConversationId::new(TransportId::new("relay").unwrap(), value).unwrap(),
        }
    }

    fn draft(body: &str, cursor: usize) -> IpcDurableDraft {
        IpcDurableDraft {
            body: body.to_owned(),
            cursor,
            pasted_body: false,
            uncertain: false,
        }
    }

    #[test]
    fn draft_wire_and_store_limits_stay_equal() {
        assert_eq!(bloom_ipc::MAX_DRAFT_BYTES, store::MAX_DRAFT_BYTES);
    }

    #[tokio::test]
    async fn drafts_round_trip_exactly_and_reject_stale_writes() {
        let (_directory, service) = harness();
        let conversation = conversation_key("exact-conversation");
        let chat = IpcDraftKey::NewChat {
            transport: TransportId::new("relay").unwrap(),
            address: "Exact@Example.test".into(),
        };
        let other_chat = IpcDraftKey::NewChat {
            transport: TransportId::new("relay").unwrap(),
            address: "exact@example.test".into(),
        };
        assert_eq!(
            service
                .draft_save(conversation.clone(), draft("conversation body", 4), 1)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        assert_eq!(
            service
                .draft_save(chat.clone(), draft("exact body", 0), 1)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        assert_eq!(
            service
                .draft_save(other_chat.clone(), draft("lower body", 0), 1)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        let loaded = service.draft_load(conversation.clone()).await.unwrap();
        assert_eq!(loaded.draft.as_ref().unwrap().body, "conversation body");
        assert_eq!(loaded.draft.as_ref().unwrap().cursor, 4);
        assert_eq!(loaded.revision_floor, 1);
        assert!(!loaded.corrupt_deleted);
        assert_eq!(
            service
                .draft_load(chat.clone())
                .await
                .unwrap()
                .draft
                .unwrap()
                .body,
            "exact body"
        );
        assert_eq!(
            service
                .draft_save(conversation.clone(), draft("stale body", 0), 1)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Stale
        );
        assert_eq!(
            service
                .draft_load(conversation.clone())
                .await
                .unwrap()
                .draft
                .unwrap()
                .body,
            "conversation body"
        );
    }

    #[tokio::test]
    async fn delete_leaves_a_tombstone_that_blocks_late_saves() {
        let (_directory, service) = harness();
        let key = conversation_key("tombstone");
        service
            .draft_save(key.clone(), draft("live", 0), 1)
            .await
            .unwrap();
        assert_eq!(
            service.draft_delete(key.clone(), 2).await.unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        let deleted = service.draft_load(key.clone()).await.unwrap();
        assert_eq!(deleted.draft, None);
        assert_eq!(deleted.revision_floor, 2);
        assert!(!deleted.corrupt_deleted);
        assert_eq!(
            service
                .draft_save(key.clone(), draft("late", 0), 2)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Stale
        );
        assert_eq!(service.draft_load(key).await.unwrap().revision_floor, 2);
    }

    #[tokio::test]
    async fn corrupt_draft_load_reports_notice_and_preserves_revision_floor() {
        let (_directory, service) = harness();
        let key = conversation_key("corrupt-wire");
        assert_eq!(
            service
                .draft_save(key.clone(), draft("private", 0), 4)
                .await
                .unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        let store_key = store_draft_key(key.clone());
        store::corrupt_draft_data_for_test(&service.store.lock().unwrap(), &store_key, b"not json")
            .unwrap();
        let loaded = service.draft_load(key).await.unwrap();
        assert_eq!(loaded.draft, None);
        assert_eq!(loaded.revision_floor, 4);
        assert!(loaded.corrupt_deleted);
    }

    #[tokio::test]
    async fn attachment_fetch_validates_tokens_and_fails_without_a_started_engine() {
        let (_directory, service) = harness();
        assert_eq!(
            service
                .fetch_attachment("not-a-token".into())
                .await
                .unwrap_err(),
            ProtocolError::InvalidRequest
        );
        assert_eq!(
            service
                .fetch_attachment("0123456789abcdef0123456789abcdef".into())
                .await
                .unwrap_err(),
            ProtocolError::Internal,
            "an unstarted engine must not fabricate a path"
        );
    }

    #[tokio::test]
    async fn invalid_drafts_map_to_invalid_request_and_errors_stay_redacted() {
        let (_directory, service) = harness();
        let blank = IpcDraftKey::NewChat {
            transport: TransportId::new("relay").unwrap(),
            address: "   ".into(),
        };
        assert_eq!(
            service
                .draft_save(blank.clone(), draft("must not persist", 0), 1)
                .await
                .unwrap_err(),
            ProtocolError::InvalidRequest
        );
        assert_eq!(
            service.draft_load(blank.clone()).await.unwrap_err(),
            ProtocolError::InvalidRequest
        );
        assert_eq!(
            service.draft_delete(blank, 1).await.unwrap_err(),
            ProtocolError::InvalidRequest
        );
        let key = conversation_key("invalid");
        assert_eq!(
            service
                .draft_save(key.clone(), draft("abc", 4), 1)
                .await
                .unwrap_err(),
            ProtocolError::InvalidRequest
        );
        assert_eq!(
            service
                .draft_save(key.clone(), draft("abc", 0), u64::MAX)
                .await
                .unwrap_err(),
            ProtocolError::InvalidRequest
        );
        assert_eq!(
            service
                .draft_save(
                    key,
                    draft(&"x".repeat(bloom_ipc::MAX_DRAFT_BYTES + 1), 0),
                    1
                )
                .await
                .unwrap_err(),
            ProtocolError::InvalidRequest
        );
    }
}

#[cfg(test)]
mod engine_slot_service_tests {
    use super::*;
    use crate::engine::{engine_slot, EngineConfig};
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    fn store(directory: &tempfile::TempDir) -> Arc<Mutex<EncryptedStore>> {
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[5; 32]).unwrap(),
        ))
    }

    fn engine(directory: &tempfile::TempDir) -> Arc<Engine> {
        Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }))
    }

    #[test]
    fn new_builds_a_slot_holding_its_engine() {
        let directory = tempfile::tempdir().unwrap();
        let engine = engine(&directory);
        let service = Service::new(store(&directory), engine.clone(), SubscriptionHub::new(4));
        let Backend::Real(slot) = &service.backend else {
            panic!("Service::new must build a real backend");
        };
        assert!(Arc::ptr_eq(&engine, &slot.read().unwrap()));
    }

    #[test]
    fn with_engine_slot_shares_the_callers_slot() {
        let directory = tempfile::tempdir().unwrap();
        let slot = engine_slot(engine(&directory));
        let service =
            Service::with_engine_slot(store(&directory), slot.clone(), SubscriptionHub::new(4));
        let Backend::Real(shared) = &service.backend else {
            panic!("with_engine_slot must build a real backend");
        };
        assert!(Arc::ptr_eq(&slot, shared));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::preview_text;
    use bloom_domain::{
        ConversationId, DeliveryState, Handle, HandleKind, Message, MessageId, MessagePart,
        TransportId,
    };

    fn message(parts: Vec<MessagePart>, from_me: bool) -> Message {
        let transport = TransportId::new("iMessage").unwrap();
        Message::new(
            MessageId::new(transport.clone(), "m1").unwrap(),
            ConversationId::new(transport, "c1").unwrap(),
            None,
            Handle::new(HandleKind::Opaque, "someone").unwrap(),
            parts,
            1,
            None,
            DeliveryState::Delivered,
            from_me,
            None,
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn previews_join_text_mark_attachments_and_prefix_our_own() {
        assert_eq!(
            preview_text(&message(vec![MessagePart::Text("hi there".into())], false)),
            "hi there"
        );
        assert_eq!(
            preview_text(&message(vec![MessagePart::Text("on my way".into())], true)),
            "You: on my way"
        );
        assert_eq!(
            preview_text(&message(
                vec![
                    MessagePart::Unsupported {
                        kind: "attachment".into()
                    },
                    MessagePart::Text("caption".into())
                ],
                false
            )),
            "[attachment] caption"
        );
    }
}

#[cfg(test)]
mod reaction_service_tests {
    use super::*;
    use crate::engine::EngineConfig;
    use bloom_domain::{MessagePart, TransportId};
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

    fn harness() -> (tempfile::TempDir, Arc<Mutex<EncryptedStore>>, Arc<Service>) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[7; 32]).unwrap(),
        ));
        let engine = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }));
        let service = Arc::new(Service::new(store.clone(), engine, SubscriptionHub::new(4)));
        (directory, store, service)
    }

    fn fixtures() -> (Conversation, Message) {
        let transport = TransportId::new("openbubbles").unwrap();
        let sender = Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap();
        let conversation = Conversation {
            id: ConversationId::new(transport.clone(), "c1").unwrap(),
            remote_id: "chat-c1".into(),
            kind: ConversationKind::Direct,
            title: None,
            participants: vec![Participant {
                handle: sender.clone(),
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };
        let message = Message::new(
            MessageId::new(transport, "m1").unwrap(),
            conversation.id.clone(),
            None,
            sender,
            vec![MessagePart::Text("hello".into())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        (conversation, message)
    }

    fn reaction_event(conversation_id: ConversationId, message_id: MessageId) -> TransportEvent {
        TransportEvent::ReactionChanged {
            conversation_id,
            message_id,
            sender: Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap(),
            kind: "like".into(),
            active: true,
            part: Some(0),
            at: Some(9),
        }
    }

    fn edit_event(
        conversation_id: ConversationId,
        message_id: MessageId,
        at: i64,
    ) -> TransportEvent {
        edit_event_from(
            Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap(),
            conversation_id,
            message_id,
            at,
        )
    }

    fn edit_event_from(
        sender: Handle,
        conversation_id: ConversationId,
        message_id: MessageId,
        at: i64,
    ) -> TransportEvent {
        TransportEvent::MessageEdited {
            conversation_id,
            message_id,
            sender,
            part: 0,
            replacement_parts: vec![MessagePart::Text("edited".into())],
            at,
        }
    }

    fn unsend_event(
        conversation_id: ConversationId,
        message_id: MessageId,
        at: i64,
    ) -> TransportEvent {
        TransportEvent::MessageUnsent {
            conversation_id,
            message_id,
            sender: Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap(),
            part: 0,
            at,
        }
    }

    /// Store a message with an aligned engine part map so mutations are eligible.
    fn seed_mapped(
        store: &Arc<Mutex<EncryptedStore>>,
        conversation: &Conversation,
        message: &Message,
    ) {
        let mut guard = store.lock().unwrap();
        store::upsert_conversation(&mut guard, conversation).unwrap();
        store::ingest_inbound_with_parts(&mut guard, conversation, message, &[Some(0)]).unwrap();
    }

    async fn next_update(
        subscriber: &mut tokio::sync::broadcast::Receiver<Event>,
    ) -> (ConversationId, Message) {
        let published = tokio::time::timeout(Duration::from_millis(100), subscriber.recv())
            .await
            .expect("update event published")
            .unwrap();
        match published.event {
            EventKind::Update {
                conversation_id,
                message,
            } => (conversation_id, message),
            _ => panic!("unexpected event kind"),
        }
    }

    #[tokio::test]
    async fn applied_edit_publishes_one_canonical_update_and_is_idempotent() {
        let (_directory, store, service) = harness();
        let (conversation, message) = fixtures();
        let (conversation_id, message_id) = (conversation.id.clone(), message.id().clone());
        seed_mapped(&store, &conversation, &message);
        let mut subscriber = service.hub.subscribe();
        let event = edit_event(conversation_id.clone(), message_id.clone(), 10);

        assert!(service.persist(&event).unwrap());
        let (published_conversation, published) = next_update(&mut subscriber).await;
        assert_eq!(published_conversation, conversation_id);
        assert_eq!(published.id(), &message_id);
        assert_eq!(published.parts, vec![MessagePart::Text("edited".into())]);
        assert_eq!(published.edited_at, Some(10));

        assert!(!service.persist(&event).unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "duplicate edit must not publish"
        );
    }

    #[tokio::test]
    async fn applied_unsend_publishes_one_canonical_update_and_is_idempotent() {
        let (_directory, store, service) = harness();
        let (conversation, message) = fixtures();
        let (conversation_id, message_id) = (conversation.id.clone(), message.id().clone());
        seed_mapped(&store, &conversation, &message);
        let mut subscriber = service.hub.subscribe();
        let event = unsend_event(conversation_id.clone(), message_id.clone(), 11);

        assert!(service.persist(&event).unwrap());
        let (published_conversation, published) = next_update(&mut subscriber).await;
        assert_eq!(published_conversation, conversation_id);
        assert_eq!(published.id(), &message_id);
        assert_eq!(published.parts, message.parts);
        assert_eq!(published.unsent_at, Some(11));

        assert!(!service.persist(&event).unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "duplicate unsend must not publish"
        );
    }

    #[tokio::test]
    async fn mutation_for_a_missing_target_or_sender_publishes_nothing() {
        let (_directory, store, service) = harness();
        let (conversation, message) = fixtures();
        let (conversation_id, message_id) = (conversation.id.clone(), message.id().clone());
        seed_mapped(&store, &conversation, &message);
        let mut subscriber = service.hub.subscribe();

        let missing_id =
            MessageId::new(TransportId::new("openbubbles").unwrap(), "missing").unwrap();
        assert!(!service
            .persist(&edit_event(conversation_id.clone(), missing_id, 10))
            .unwrap());

        let wrong_sender = edit_event_from(
            Handle::new(HandleKind::Opaque, "other@example.invalid").unwrap(),
            conversation_id,
            message_id.clone(),
            12,
        );
        assert!(!service.persist(&wrong_sender).unwrap());

        let stored = {
            let guard = store.lock().unwrap();
            store::get_message(&guard, &message_id).unwrap().unwrap()
        };
        assert_eq!(stored, message);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "rejected mutations must not publish"
        );
    }

    #[tokio::test]
    async fn edit_via_an_alias_conversation_publishes_the_canonical_id() {
        let (_directory, store, service) = harness();
        let transport = TransportId::new("openbubbles").unwrap();
        let alias = ConversationId::new(transport.clone(), "alias").unwrap();
        let canonical = ConversationId::new(transport.clone(), "canonical").unwrap();
        let alias_row = conversation_with_id(alias.clone());
        let mut canonical_conversation = conversation_with_id(canonical.clone());
        canonical_conversation.aliases = vec![alias.clone()];
        let message = message_in(
            &canonical_conversation,
            MessageId::new(transport, "message-canonical").unwrap(),
        );
        let message_id = message.id().clone();
        seed_mapped(&store, &canonical_conversation, &message);
        {
            let mut guard = store.lock().unwrap();
            store::upsert_conversation(&mut guard, &alias_row).unwrap();
        }
        let mut subscriber = service.hub.subscribe();

        assert!(service
            .persist(&edit_event(alias, message_id.clone(), 10))
            .unwrap());
        let (published_conversation, published) = next_update(&mut subscriber).await;
        assert_eq!(published_conversation, canonical);
        assert_eq!(published.id(), &message_id);
        assert_eq!(published.parts, vec![MessagePart::Text("edited".into())]);
        let stored = {
            let guard = store.lock().unwrap();
            store::get_message(&guard, &message_id).unwrap().unwrap()
        };
        assert_eq!(stored.edited_at, Some(10));
    }

    #[tokio::test]
    async fn applied_reaction_publishes_one_update_and_is_idempotent() {
        let (_directory, store, service) = harness();
        let (conversation, message) = fixtures();
        let (conversation_id, message_id) = (conversation.id.clone(), message.id().clone());
        {
            let mut guard = store.lock().unwrap();
            store::upsert_conversation(&mut guard, &conversation).unwrap();
            store::upsert_message(&mut guard, &message).unwrap();
        }
        let mut subscriber = service.hub.subscribe();
        let event = reaction_event(conversation_id.clone(), message_id.clone());

        assert!(service.persist(&event).unwrap());
        let published = tokio::time::timeout(Duration::from_millis(100), subscriber.recv())
            .await
            .expect("update event published")
            .unwrap();
        match published.event {
            EventKind::Update {
                conversation_id: published_conversation,
                message: published_message,
            } => {
                assert_eq!(published_conversation, conversation_id);
                assert_eq!(published_message.id(), &message_id);
                assert!(published_message.reactions.iter().any(|reaction| {
                    reaction.sender
                        == Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap()
                        && reaction.kind == "like"
                        && reaction.part == Some(0)
                }));
            }
            _ => panic!("unexpected event kind"),
        }
        let stored = {
            let guard = store.lock().unwrap();
            store::get_message(&guard, &message_id).unwrap().unwrap()
        };
        assert_eq!(stored.reactions.len(), 1);

        assert!(!service.persist(&event).unwrap());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "duplicate reaction must not publish another event"
        );
    }

    fn conversation_with_id(id: ConversationId) -> Conversation {
        let sender = Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap();
        Conversation {
            id,
            remote_id: "chat".into(),
            kind: ConversationKind::Direct,
            title: None,
            participants: vec![Participant {
                handle: sender,
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        }
    }

    fn message_in(conversation: &Conversation, id: MessageId) -> Message {
        Message::new(
            id,
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Opaque, "sender@example.invalid").unwrap(),
            vec![MessagePart::Text("hello".into())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn reaction_for_a_missing_message_publishes_nothing() {
        let (_directory, store, service) = harness();
        let transport = TransportId::new("openbubbles").unwrap();
        let conversation =
            conversation_with_id(ConversationId::new(transport.clone(), "c1").unwrap());
        let conversation_id = conversation.id.clone();
        {
            let mut guard = store.lock().unwrap();
            store::upsert_conversation(&mut guard, &conversation).unwrap();
        }
        let message_id = MessageId::new(transport, "missing").unwrap();
        let mut subscriber = service.hub.subscribe();

        let changed = service
            .persist(&reaction_event(conversation_id, message_id.clone()))
            .unwrap();
        assert!(!changed);
        {
            let guard = store.lock().unwrap();
            assert_eq!(store::list_conversations(&guard).unwrap().len(), 1);
            assert!(store::get_message(&guard, &message_id).unwrap().is_none());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "missing message must not publish an event"
        );
    }

    #[tokio::test]
    async fn reaction_for_a_mismatched_conversation_publishes_nothing() {
        let (_directory, store, service) = harness();
        let transport = TransportId::new("openbubbles").unwrap();
        let conversation_a =
            conversation_with_id(ConversationId::new(transport.clone(), "a").unwrap());
        let conversation_b =
            conversation_with_id(ConversationId::new(transport.clone(), "b").unwrap());
        let message = message_in(
            &conversation_b,
            MessageId::new(transport, "message-b").unwrap(),
        );
        let (conversation_a_id, message_id) = (conversation_a.id.clone(), message.id().clone());
        {
            let mut guard = store.lock().unwrap();
            store::upsert_conversation(&mut guard, &conversation_a).unwrap();
            store::upsert_conversation(&mut guard, &conversation_b).unwrap();
            store::upsert_message(&mut guard, &message).unwrap();
        }
        let mut subscriber = service.hub.subscribe();

        let changed = service
            .persist(&reaction_event(conversation_a_id, message_id.clone()))
            .unwrap();
        assert!(!changed);
        {
            let guard = store.lock().unwrap();
            let stored = store::get_message(&guard, &message_id).unwrap().unwrap();
            assert!(stored.reactions.is_empty());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "mismatched conversation must not publish an event"
        );
    }

    #[tokio::test]
    async fn reaction_via_an_alias_conversation_applies_to_the_canonical_message() {
        let (_directory, store, service) = harness();
        let transport = TransportId::new("openbubbles").unwrap();
        let alias = ConversationId::new(transport.clone(), "alias").unwrap();
        let canonical = ConversationId::new(transport.clone(), "canonical").unwrap();
        let alias_row = conversation_with_id(alias.clone());
        let mut canonical_conversation = conversation_with_id(canonical.clone());
        canonical_conversation.aliases = vec![alias.clone()];
        let message = message_in(
            &canonical_conversation,
            MessageId::new(transport, "message-canonical").unwrap(),
        );
        let message_id = message.id().clone();
        {
            let mut guard = store.lock().unwrap();
            store::upsert_conversation(&mut guard, &alias_row).unwrap();
            store::upsert_conversation(&mut guard, &canonical_conversation).unwrap();
            store::upsert_message(&mut guard, &message).unwrap();
        }
        let mut subscriber = service.hub.subscribe();

        let changed = service
            .persist(&reaction_event(alias, message_id.clone()))
            .unwrap();
        assert!(changed);
        let published = tokio::time::timeout(Duration::from_millis(100), subscriber.recv())
            .await
            .expect("update event published")
            .unwrap();
        match published.event {
            EventKind::Update {
                conversation_id: published_conversation,
                message: published_message,
            } => {
                assert_eq!(published_conversation, canonical);
                assert_eq!(published_message.id(), &message_id);
                assert!(published_message
                    .reactions
                    .iter()
                    .any(|reaction| reaction.kind == "like" && reaction.part == Some(0)));
            }
            _ => panic!("unexpected event kind"),
        }
        let stored = {
            let guard = store.lock().unwrap();
            store::get_message(&guard, &message_id).unwrap().unwrap()
        };
        assert_eq!(stored.reactions.len(), 1);
    }

    #[tokio::test]
    async fn reaction_for_missing_conversation_changes_nothing() {
        let (_directory, store, service) = harness();
        let (_conversation, message) = fixtures();
        let (conversation_id, message_id) =
            (message.conversation_id().clone(), message.id().clone());
        let mut subscriber = service.hub.subscribe();

        let changed = service
            .persist(&reaction_event(conversation_id, message_id.clone()))
            .unwrap();
        assert!(!changed);
        {
            let guard = store.lock().unwrap();
            assert!(store::list_conversations(&guard).unwrap().is_empty());
            assert!(store::get_message(&guard, &message_id).unwrap().is_none());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscriber.recv())
                .await
                .is_err(),
            "missing conversation must not publish an event"
        );
    }
}

#[cfg(test)]
mod outgoing_attachment_tests {
    use super::*;

    fn file_with(directory: &std::path::Path, name: &str, bytes: usize) -> std::path::PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    #[test]
    fn accepts_a_regular_file_and_reports_its_name_and_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = file_with(directory.path(), "report.txt", 5);
        assert_eq!(
            outgoing_attachment_file(path.to_str().unwrap()).unwrap(),
            ("report.txt".to_owned(), 5)
        );
    }

    #[test]
    fn rejects_relative_parent_missing_directory_symlink_and_empty_paths() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let empty = file_with(directory.path(), "empty", 0);
        let target = file_with(directory.path(), "target", 1);
        // Exists so the interior-dot case fails only because of the raw segment
        // check, not because the file is missing.
        file_with(directory.path(), "report.txt", 5);
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        for path in [
            "relative.txt".to_owned(),
            format!("{}/../escape", directory.path().display()),
            format!("{}/./report.txt", directory.path().display()),
            folder.to_str().unwrap().to_owned(),
            link.to_str().unwrap().to_owned(),
            empty.to_str().unwrap().to_owned(),
            directory
                .path()
                .join("missing")
                .to_str()
                .unwrap()
                .to_owned(),
            "/".to_owned(),
        ] {
            assert_eq!(
                outgoing_attachment_file(&path),
                Err(ProtocolError::InvalidRequest),
                "expected rejection for {path}"
            );
        }
    }

    #[test]
    fn rejects_a_file_over_the_outbound_bound() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.bin");
        let handle = std::fs::File::create(&path).unwrap();
        handle.set_len(MAX_OUTBOUND_ATTACHMENT_BYTES + 1).unwrap();
        assert_eq!(
            outgoing_attachment_file(path.to_str().unwrap()),
            Err(ProtocolError::InvalidRequest)
        );
    }
}

#[cfg(test)]
mod readiness_gate_tests {
    use super::*;
    use crate::{
        engine::{engine_slot, EngineConfig},
        ipc::RequestHandler,
        readiness::Readiness,
    };
    use bloom_domain::{MessagePart, TransportId};
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    fn harness(ready: bool) -> (tempfile::TempDir, Arc<Service>) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_with_key(directory.path().join("store.db"), &[7; 32]).unwrap(),
        ));
        let engine = Arc::new(Engine::new(EngineConfig {
            dart_executable: PathBuf::from("dart"),
            script: PathBuf::from("engine.dart"),
            state_dir: directory.path().join("state"),
            library: PathBuf::from("engine.dylib"),
            sync_chats: false,
        }));
        let slot = engine_slot(engine);
        let hub = SubscriptionHub::new(4);
        let readiness = if ready {
            Readiness::new_ready(1, hub.clone())
        } else {
            Readiness::new_starting(hub.clone())
        };
        let service = Arc::new(Service::with_engine_slot_and_readiness(
            store, slot, hub, readiness,
        ));
        (directory, service)
    }

    fn body() -> OutgoingBody {
        OutgoingBody {
            parts: vec![MessagePart::Text("synthetic body".into())],
        }
    }

    #[tokio::test]
    async fn closed_gate_rejects_all_three_before_preparation_or_store_lookup() {
        let (_directory, service) = harness(false);
        let conversation =
            ConversationId::new(TransportId::new("openbubbles").unwrap(), "missing").unwrap();

        assert_eq!(
            service.send(conversation.clone(), body()).await,
            Err(ProtocolError::NotReady),
            "send must reject before the store lookup or engine call"
        );
        assert_eq!(
            service
                .start_conversation("not a valid address".into(), body())
                .await,
            Err(ProtocolError::NotReady),
            "new chat must reject before address preparation"
        );
        assert_eq!(
            service
                .send_attachment(conversation, "/definitely/not/a/file".into())
                .await,
            Err(ProtocolError::NotReady),
            "attachment must reject before path validation"
        );
        assert!(
            service.list_conversations().await.unwrap().is_empty(),
            "a gated request must not mutate the store"
        );
    }

    #[tokio::test]
    async fn manual_sync_closes_the_gate_and_reports_unavailable() {
        let (_directory, service) = harness(true);
        assert!(service.readiness().can_send());
        let result = service.sync_conversations().await;
        assert_eq!(result, Err(ProtocolError::TransportUnavailable));
        let snapshot = service.readiness_snapshot();
        assert!(!snapshot.can_send);
        assert_eq!(snapshot.phase, bloom_ipc::ReadinessPhase::Failed);
        assert_eq!(
            snapshot.outcome,
            Some(bloom_ipc::ReadinessOutcome::Unavailable)
        );
    }

    #[tokio::test]
    async fn inbound_and_local_events_do_not_close_an_open_gate() {
        let (_directory, service) = harness(true);
        service
            .persist(&TransportEvent::ConversationsChanged)
            .unwrap();
        service
            .persist(&TransportEvent::Session {
                generation: 1,
                state: SessionState::Active,
                reason: None,
            })
            .unwrap();
        assert!(service.readiness().can_send());
    }

    #[tokio::test]
    async fn disconnected_session_closes_the_gate() {
        let (_directory, service) = harness(true);
        service
            .persist(&TransportEvent::Session {
                generation: 1,
                state: SessionState::Disconnected,
                reason: None,
            })
            .unwrap();
        assert!(!service.readiness().can_send());
    }

    #[tokio::test]
    async fn stale_session_from_the_previous_generation_cannot_close_the_gate() {
        let (_directory, service) = harness(true);
        let token = service.readiness().begin(2).unwrap();
        service.readiness().mark_engine_ready(token);
        assert!(service.readiness().finish(token, Finish::Complete(0)));
        assert!(service.readiness().can_send());

        service
            .persist(&TransportEvent::Session {
                generation: 1,
                state: SessionState::Disconnected,
                reason: None,
            })
            .unwrap();

        assert!(
            service.readiness().can_send(),
            "a session event from the replaced engine generation must be ignored"
        );
    }

    #[tokio::test]
    async fn stale_active_does_not_consume_a_reconnect_refresh_slot() {
        let (_directory, service) = harness(true);
        let token = service.readiness().begin(2).unwrap();
        service.readiness().mark_engine_ready(token);
        assert!(service.readiness().finish(token, Finish::Complete(0)));
        service.readiness().session_lost();

        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(service.clone().persist_events(receiver));
        sender
            .send(TransportEvent::Session {
                generation: 1,
                state: SessionState::Active,
                reason: None,
            })
            .await
            .unwrap();
        drop(sender);
        task.await.unwrap();

        assert_eq!(
            service.readiness().snapshot().bounded_error,
            Some(bloom_ipc::ReadinessError::Disconnected)
        );
        assert!(
            service.readiness().begin_reconnect(),
            "a stale Active must not reserve the reconnect slot"
        );
    }

    #[tokio::test]
    async fn readiness_query_returns_the_shared_snapshot() {
        let (_directory, service) = harness(false);
        let snapshot = RequestHandler::readiness(&*service).await.unwrap();
        assert_eq!(snapshot.phase, bloom_ipc::ReadinessPhase::Starting);
        assert!(!snapshot.can_send);
    }
}

#[cfg(test)]
mod demo_backend_tests {
    use super::*;
    use bloom_domain::{MessagePart, TransportId};
    use std::os::unix::fs::PermissionsExt;

    fn harness() -> (tempfile::TempDir, Arc<Mutex<EncryptedStore>>, Arc<Service>) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(Mutex::new(
            EncryptedStore::open_demo(directory.path().join("store.db")).unwrap(),
        ));
        let service = Arc::new(Service::demo(store.clone(), SubscriptionHub::new(4)));
        (directory, store, service)
    }

    fn seed(store: &Arc<Mutex<EncryptedStore>>) -> (ConversationId, MessageId) {
        let transport = TransportId::new("openbubbles").unwrap();
        let sender = Handle::new(HandleKind::Opaque, "demo@example.invalid").unwrap();
        let conversation = Conversation {
            id: ConversationId::new(transport.clone(), "demo-chat").unwrap(),
            remote_id: "demo-chat".into(),
            kind: ConversationKind::Direct,
            title: Some("Demo chat".into()),
            participants: vec![Participant {
                handle: sender.clone(),
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: Some(1),
            aliases: Vec::new(),
            preview: None,
        };
        let message = Message::new(
            MessageId::new(transport, "demo-message").unwrap(),
            conversation.id.clone(),
            None,
            sender,
            vec![MessagePart::Text("synthetic hello".into())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        let mut guard = store.lock().unwrap();
        store::upsert_conversation(&mut guard, &conversation).unwrap();
        store::ingest_inbound_with_parts(&mut guard, &conversation, &message, &[Some(0)]).unwrap();
        (conversation.id, message.id().clone())
    }

    #[tokio::test]
    async fn store_backed_reads_work_without_an_engine() {
        let (_directory, store, service) = harness();
        let (conversation_id, message_id) = seed(&store);

        let conversations = service.list_conversations().await.unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].id, conversation_id);

        let history = service.history(conversation_id.clone(), 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id(), &message_id);

        let search = service
            .search_messages("synthetic".into(), 10)
            .await
            .unwrap();
        assert_eq!(search.len(), 1);

        let context = service
            .message_context(conversation_id, message_id, 10)
            .await
            .unwrap();
        assert_eq!(context.len(), 1);
    }

    #[tokio::test]
    async fn drafts_round_trip_in_the_demo_store() {
        let (_directory, _store, service) = harness();
        let key = IpcDraftKey::NewChat {
            transport: TransportId::new("openbubbles").unwrap(),
            address: "demo@example.invalid".into(),
        };
        let draft = IpcDurableDraft {
            body: "parked demo draft".into(),
            cursor: 4,
            pasted_body: false,
            uncertain: false,
        };
        assert_eq!(
            service.draft_save(key.clone(), draft, 1).await.unwrap(),
            IpcDraftWriteOutcome::Applied
        );
        let loaded = service.draft_load(key).await.unwrap();
        assert_eq!(loaded.draft.unwrap().body, "parked demo draft");
    }

    #[tokio::test]
    async fn engine_facing_operations_are_unsupported_in_demo() {
        let (_directory, _store, service) = harness();
        let conversation_id =
            ConversationId::new(TransportId::new("openbubbles").unwrap(), "demo-chat").unwrap();
        let body = OutgoingBody {
            parts: vec![MessagePart::Text("hello".into())],
        };

        assert_eq!(
            service
                .start_conversation("demo@example.invalid".into(), body.clone())
                .await
                .unwrap_err(),
            ProtocolError::Unsupported
        );
        assert_eq!(
            service.send(conversation_id, body).await.unwrap_err(),
            ProtocolError::Unsupported
        );
        assert_eq!(
            service
                .send_attachment(
                    ConversationId::new(TransportId::new("openbubbles").unwrap(), "demo-chat")
                        .unwrap(),
                    "/etc/passwd".into(),
                )
                .await
                .unwrap_err(),
            ProtocolError::Unsupported
        );
        assert_eq!(
            service
                .fetch_attachment("0123456789abcdef0123456789abcdef".into())
                .await
                .unwrap_err(),
            ProtocolError::Unsupported
        );
        assert_eq!(
            service.sync_conversations().await.unwrap_err(),
            ProtocolError::Unsupported
        );
    }

    #[test]
    fn demo_service_has_no_engine_slot() {
        let (_directory, _store, service) = harness();
        assert!(matches!(&service.backend, Backend::Demo));
        assert!(service.current_engine().is_none());
    }
}
