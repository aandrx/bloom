//! Request handling over the encrypted store and the engine transport.
//!
//! Ordering rule: an event is persisted before it is published. A client must never
//! observe an event that was not committed, because a subscription client can
//! reconnect and re-read from the store.

use crate::{
    engine::{outgoing_text, Engine},
    ipc::{HandlerFuture, RequestHandler, SubscriptionHub},
    store::{self, EncryptedStore},
};
use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, OutgoingBody, Participant,
};
use bloom_ipc::{Event, EventKind, ProtocolError};
use bloom_transport::{TransportError, TransportEvent};
use std::sync::{Arc, Mutex};

pub struct Service {
    store: Arc<Mutex<EncryptedStore>>,
    engine: Arc<Engine>,
    hub: SubscriptionHub,
}

impl Service {
    pub fn new(
        store: Arc<Mutex<EncryptedStore>>,
        engine: Arc<Engine>,
        hub: SubscriptionHub,
    ) -> Self {
        Self { store, engine, hub }
    }

    /// Persist a transport event, then fan it out. Returns whether anything changed.
    pub fn persist(&self, event: &TransportEvent) -> Result<bool, store::StoreError> {
        match event {
            TransportEvent::Message {
                conversation,
                message,
            }
            | TransportEvent::MessageUpdated {
                conversation,
                message,
            } => {
                let (outcome, _created) = {
                    let mut store = self.store.lock().unwrap();
                    store::ingest_inbound(&mut store, conversation, message)?
                };
                match outcome {
                    store::MessageUpsertOutcome::Unchanged => return Ok(false),
                    store::MessageUpsertOutcome::Inserted => {}
                    store::MessageUpsertOutcome::Updated => {}
                }
                self.hub.publish(Event {
                    event: EventKind::Message {
                        conversation_id: conversation.id.clone(),
                        message: message.clone(),
                    },
                });
                Ok(true)
            }
            TransportEvent::DeliveryChanged {
                conversation,
                message,
                delivery,
            } => {
                let updated = {
                    let mut store = self.store.lock().unwrap();
                    store::update_delivery(&mut store, conversation, message, delivery.clone())?
                };
                if let Some(message) = updated {
                    self.hub.publish(Event {
                        event: EventKind::Delivery {
                            conversation_id: conversation.clone(),
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
                store::upsert_conversation(&mut store, conversation)?;
                Ok(false)
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
        }
    }

    fn send_blocking_prepare(
        &self,
        conversation_id: &ConversationId,
        body: &OutgoingBody,
    ) -> Result<(String, String, Conversation), ProtocolError> {
        let text = outgoing_text(body).map_err(|_| ProtocolError::Unsupported)?;
        let store = self.store.lock().unwrap();
        let conversation = store::get_conversation(&store, conversation_id)
            .map_err(|_| ProtocolError::Internal)?
            .ok_or(ProtocolError::NotFound)?;
        let recipient = conversation
            .participants
            .iter()
            .find(|participant| !participant.is_me)
            .map(|participant| rust_handle(&participant.handle))
            .ok_or(ProtocolError::NotFound)?;
        Ok((text, recipient, conversation))
    }
}

impl RequestHandler for Service {
    fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            store::list_conversations(&store).map_err(|_| ProtocolError::Internal)
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

    fn start_conversation(
        &self,
        address: String,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId> {
        Box::pin(async move {
            let text = outgoing_text(&body).map_err(|_| ProtocolError::Unsupported)?;
            let (handle, recipient) = normalize_address(&address)?;
            let (conversation, existed) = {
                let store = self.store.lock().unwrap();
                let found = store::list_conversations(&store)
                    .map_err(|_| ProtocolError::Internal)?
                    .into_iter()
                    .find(|c| {
                        c.participants
                            .iter()
                            .any(|p| !p.is_me && p.handle == handle)
                    });
                match found {
                    Some(conversation) => (conversation, true),
                    None => {
                        let remote_id = format!("iMessage;-;{}", recipient);
                        let id =
                            ConversationId::new(crate::engine::transport_id(), remote_id.clone())
                                .map_err(|_| ProtocolError::InvalidRequest)?;
                        let conversation = Conversation {
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
                                if let Some(me) = self.engine.primary_handle() {
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
                        };
                        (conversation, false)
                    }
                }
            };
            let message_id = self
                .engine
                .send_text(&recipient, Some(&conversation.remote_id), &text)
                .await
                .map_err(map_transport_error)?;
            let sender = conversation
                .participants
                .iter()
                .find(|p| p.is_me)
                .map(|p| p.handle.clone())
                .unwrap_or_else(|| {
                    Handle::new(HandleKind::Opaque, "me").expect("static handle is valid")
                });
            let message = Message::new(
                message_id.clone(),
                conversation.id.clone(),
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
            {
                let mut store = self.store.lock().unwrap();
                if !existed {
                    store::upsert_conversation(&mut store, &conversation)
                        .map_err(|_| ProtocolError::Internal)?;
                }
                store::ingest_inbound(&mut store, &conversation, &message)
                    .map_err(|_| ProtocolError::Internal)?;
            }
            self.hub.publish(Event {
                event: EventKind::Message {
                    conversation_id: conversation.id,
                    message,
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
            let (text, recipient, conversation) =
                self.send_blocking_prepare(&conversation_id, &body)?;

            let message_id = self
                .engine
                .send_text(&recipient, Some(&conversation.remote_id), &text)
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
                conversation_id.clone(),
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
                    conversation_id,
                    message,
                },
            });
            Ok(message_id)
        })
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
