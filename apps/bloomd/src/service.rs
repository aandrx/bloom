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
    Conversation, ConversationId, DeliveryState, Handle, HandleKind, Message, MessageId,
    OutgoingBody,
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
            TransportEvent::ConversationsChanged => Ok(false),
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
