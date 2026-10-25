//! Request handling over the encrypted store and the engine transport.
//!
//! Ordering rule: an event is persisted before it is published. A client must never
//! observe an event that was not committed, because a subscription client can
//! reconnect and re-read from the store.

use crate::{
    engine::{engine_slot, outgoing_text, Engine, EngineSlot},
    ipc::{HandlerFuture, RequestHandler, SubscriptionHub},
    store::{self, EncryptedStore},
};
use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, OutgoingBody, Participant,
};
use bloom_ipc::{
    Event, EventKind, HistoryCursor, MessageSearchHit, ParticipantFilter, ProtocolError,
};
use bloom_transport::{TransportError, TransportEvent};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

pub struct Service {
    store: Arc<Mutex<EncryptedStore>>,
    engine: EngineSlot,
    hub: SubscriptionHub,
}

impl Service {
    pub fn new(
        store: Arc<Mutex<EncryptedStore>>,
        engine: Arc<Engine>,
        hub: SubscriptionHub,
    ) -> Self {
        Self {
            store,
            engine: engine_slot(engine),
            hub,
        }
    }

    /// Build a service over a caller-owned slot, so a future generation can replace
    /// the engine without rebuilding the service.
    pub fn with_engine_slot(
        store: Arc<Mutex<EncryptedStore>>,
        engine: EngineSlot,
        hub: SubscriptionHub,
    ) -> Self {
        Self { store, engine, hub }
    }

    /// Clone the current engine out of the slot. The lock is released here, so no
    /// caller holds it across an await.
    fn current_engine(&self) -> Arc<Engine> {
        let engine = self.engine.read().unwrap();
        Arc::clone(&engine)
    }

    /// Persist a transport event, then fan it out. Returns whether anything changed.
    pub fn persist(&self, event: &TransportEvent) -> Result<bool, store::StoreError> {
        match event {
            TransportEvent::Message {
                conversation,
                message,
                push_sequence,
            } => self.persist_message(conversation, message, *push_sequence),
            TransportEvent::MessageUpdated {
                conversation,
                message,
            } => self.persist_message(conversation, message, None),
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
            TransportEvent::Session { state, .. } => {
                self.hub.publish(Event {
                    event: EventKind::Session {
                        state: state.clone(),
                    },
                });
                Ok(true)
            }
            // Reaction application lands in the next unit. Until then the event
            // is accepted without inventing a message or rewriting content, and
            // no subscriber sees a fabricated update.
            TransportEvent::ReactionChanged { .. } => Ok(false),
        }
    }

    fn persist_message(
        &self,
        conversation: &Conversation,
        message: &Message,
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
            let (outcome, _) = store::ingest_inbound(&mut store, &canonical, &message)?;
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
            if let Err(error) = self.persist(&event) {
                if is_conversation {
                    conversation_persistence_failed = true;
                }
                eprintln!("bloomd: dropping event that failed to persist: {error}");
            }
        }
    }

    fn send_blocking_prepare(
        &self,
        conversation_id: &ConversationId,
        body: &OutgoingBody,
    ) -> Result<(String, Vec<String>, Conversation), ProtocolError> {
        let text = outgoing_text(body).map_err(|_| ProtocolError::Unsupported)?;
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
        Ok((text, recipients, conversation))
    }
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
            let engine = self.current_engine();
            let (limited, count) = engine
                .sync_conversations()
                .await
                .map_err(map_transport_error)?;
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
            let text = outgoing_text(&body).map_err(|_| ProtocolError::Unsupported)?;
            let (handle, recipient) = normalize_address(&address)?;
            let engine_transport = crate::engine::transport_id();
            // Resolve one engine for the whole operation: a swap between building
            // the participants and sending must not mix two generations.
            let engine = self.current_engine();
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
            let (text, recipients, conversation) =
                self.send_blocking_prepare(&conversation_id, &body)?;

            let engine = self.current_engine();
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
        assert!(Arc::ptr_eq(&engine, &service.engine.read().unwrap()));
    }

    #[test]
    fn with_engine_slot_shares_the_callers_slot() {
        let directory = tempfile::tempdir().unwrap();
        let slot = engine_slot(engine(&directory));
        let service =
            Service::with_engine_slot(store(&directory), slot.clone(), SubscriptionHub::new(4));
        assert!(Arc::ptr_eq(&slot, &service.engine));
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
