//! Transport-neutral asynchronous interface shared by transport implementations.
use async_trait::async_trait;
use bloom_domain::{
    Capabilities, Conversation, ConversationId, DeliveryState, Message, MessageId, MessagePart,
    OutgoingBody, SessionState, TransportId,
};
use std::{error::Error, fmt};
use tokio::sync::mpsc;

pub mod conformance;
pub mod testing;

/// Typed failures. Variants intentionally carry no protocol payload or identifying data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    SessionExpired,
    AccountThrottled,
    RecipientUnreachable,
    NotRegistered,
    TransportUnavailable,
    Unsupported,
    Protocol,
    Internal,
}
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SessionExpired => "session expired",
            Self::AccountThrottled => "account throttled",
            Self::RecipientUnreachable => "recipient unreachable",
            Self::NotRegistered => "not registered",
            Self::TransportUnavailable => "transport unavailable",
            Self::Unsupported => "operation unsupported",
            Self::Protocol => "protocol error",
            Self::Internal => "internal transport error",
        })
    }
}
impl Error for TransportError {}
pub type TransportResult<T> = Result<T, TransportError>;

/// Non-sensitive explanation of a session state transition.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionReason {
    Network,
    Authentication,
    Renewal,
    Protocol,
    Throttling,
    Registration,
    Unknown,
}
impl fmt::Debug for SessionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Network => "Network",
            Self::Authentication => "Authentication",
            Self::Renewal => "Renewal",
            Self::Protocol => "Protocol",
            Self::Throttling => "Throttling",
            Self::Registration => "Registration",
            Self::Unknown => "Unknown",
        })
    }
}

/// Transport events. Debug output intentionally includes only variant names.
#[derive(Clone)]
pub enum TransportEvent {
    ConversationsChanged,
    Message {
        conversation: Conversation,
        message: Message,
    },
    MessageUpdated {
        conversation: Conversation,
        message: Message,
    },
    DeliveryChanged {
        conversation: ConversationId,
        message: MessageId,
        delivery: DeliveryState,
    },
    Session {
        state: SessionState,
        reason: Option<SessionReason>,
    },
}
impl fmt::Debug for TransportEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConversationsChanged => "TransportEvent::ConversationsChanged",
            Self::Message { .. } => "TransportEvent::Message([REDACTED])",
            Self::MessageUpdated { .. } => "TransportEvent::MessageUpdated([REDACTED])",
            Self::DeliveryChanged { .. } => "TransportEvent::DeliveryChanged([REDACTED])",
            Self::Session { .. } => "TransportEvent::Session([REDACTED])",
        })
    }
}

/// Validate body features against declared transport capabilities. Conversation-specific
/// policy such as group support must be checked by the adapter.
pub fn validate_outgoing(capabilities: &Capabilities, body: &OutgoingBody) -> TransportResult<()> {
    for part in &body.parts {
        match part {
            MessagePart::Attachment(_) if !capabilities.attachments => {
                return Err(TransportError::Unsupported)
            }
            MessagePart::Unsupported { .. } => return Err(TransportError::Unsupported),
            _ => {}
        }
    }
    Ok(())
}

/// Implementations must return `Unsupported` for undeclared operations, never silently degrade.
#[async_trait]
pub trait Transport: Send + Sync {
    fn id(&self) -> TransportId;
    fn capabilities(&self) -> Capabilities;
    /// Take the sole bounded event receiver before calling `start`; a second take fails.
    async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>>;
    async fn start(&self) -> TransportResult<()>;
    /// Stop idempotently: await producer shutdown and send no new events after return.
    /// Previously buffered events may still be drained by the receiver.
    async fn stop(&self) -> TransportResult<()>;
    async fn conversations(&self) -> TransportResult<Vec<Conversation>>;
    /// Return at most `limit` most recent messages for the conversation.
    async fn history(
        &self,
        conversation: &ConversationId,
        limit: u16,
    ) -> TransportResult<Vec<Message>>;
    /// Success means accepted by the transport, not delivered. Shutdown may cancel
    /// an in-flight call. Before integrating a side-effecting adapter, extend this
    /// interface with a caller-owned idempotency key or equivalent recovery for an
    /// uncertain send result; the fake has no external network side effects.
    async fn send(
        &self,
        conversation: &ConversationId,
        body: OutgoingBody,
    ) -> TransportResult<MessageId>;
}

// Producers must await `send` on their bounded queue. A closed receiver is a failure;
// adapters must not silently drop messages. Daemon-side fanout to UI clients is separate.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    struct MemoryTransport {
        receiver_taken: Mutex<bool>,
        sender: Mutex<Option<mpsc::Sender<TransportEvent>>>,
        started: Mutex<bool>,
        stopped: Mutex<bool>,
    }
    impl MemoryTransport {
        fn new() -> Self {
            Self {
                receiver_taken: Mutex::new(false),
                sender: Mutex::new(None),
                started: Mutex::new(false),
                stopped: Mutex::new(false),
            }
        }
        async fn emit(&self, event: TransportEvent) -> TransportResult<()> {
            let sender = self
                .sender
                .lock()
                .unwrap()
                .clone()
                .ok_or(TransportError::TransportUnavailable)?;
            sender
                .send(event)
                .await
                .map_err(|_| TransportError::TransportUnavailable)
        }
    }
    #[async_trait]
    impl Transport for MemoryTransport {
        fn id(&self) -> TransportId {
            TransportId::new("test").unwrap()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
            let mut taken = self.receiver_taken.lock().unwrap();
            if *taken {
                return Err(TransportError::Internal);
            }
            *taken = true;
            let (tx, rx) = mpsc::channel(1);
            *self.sender.lock().unwrap() = Some(tx);
            Ok(rx)
        }
        async fn start(&self) -> TransportResult<()> {
            if self.sender.lock().unwrap().is_none() {
                return Err(TransportError::Internal);
            }
            *self.started.lock().unwrap() = true;
            Ok(())
        }
        async fn stop(&self) -> TransportResult<()> {
            if !*self.stopped.lock().unwrap() {
                *self.stopped.lock().unwrap() = true;
                self.sender.lock().unwrap().take();
            }
            Ok(())
        }
        async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
            Ok(vec![])
        }
        async fn history(&self, _: &ConversationId, _: u16) -> TransportResult<Vec<Message>> {
            Ok(vec![])
        }
        async fn send(&self, _: &ConversationId, _: OutgoingBody) -> TransportResult<MessageId> {
            Err(TransportError::Unsupported)
        }
    }

    #[tokio::test]
    async fn bounded_channel_backpressures_and_closed_receiver_fails() {
        let transport = MemoryTransport::new();
        assert!(transport.start().await.is_err());
        let mut events = transport.take_events().await.unwrap();
        assert!(transport.take_events().await.is_err());
        transport.start().await.unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        let tx = transport.sender.lock().unwrap().clone().unwrap();
        let blocked =
            tokio::spawn(async move { tx.send(TransportEvent::ConversationsChanged).await });
        assert!(!blocked.is_finished());
        events.recv().await.unwrap();
        blocked.await.unwrap().unwrap();
        drop(events);
        assert_eq!(
            transport
                .emit(TransportEvent::ConversationsChanged)
                .await
                .unwrap_err(),
            TransportError::TransportUnavailable
        );
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_closes_producers() {
        let transport = Arc::new(MemoryTransport::new());
        let mut events = transport.take_events().await.unwrap();
        transport.start().await.unwrap();
        transport.stop().await.unwrap();
        transport.stop().await.unwrap();
        assert!(events.recv().await.is_none());
        assert_eq!(
            transport
                .emit(TransportEvent::ConversationsChanged)
                .await
                .unwrap_err(),
            TransportError::TransportUnavailable
        );
    }

    #[test]
    fn typed_errors_reasons_and_debug_are_safe() {
        assert_eq!(format!("{:?}", SessionReason::Protocol), "Protocol");
        let event = TransportEvent::Session {
            state: SessionState::Failed,
            reason: Some(SessionReason::Authentication),
        };
        assert!(!format!("{event:?}").contains("Authentication"));
        assert!(!TransportError::Internal.to_string().contains("secret"));
    }

    #[test]
    fn validator_rejects_undeclared_parts() {
        let body = OutgoingBody {
            parts: vec![MessagePart::Unsupported {
                kind: "future".into(),
            }],
        };
        assert_eq!(
            validate_outgoing(&Capabilities::default(), &body),
            Err(TransportError::Unsupported)
        );
    }
}
