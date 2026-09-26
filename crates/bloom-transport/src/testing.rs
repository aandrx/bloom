use crate::{validate_outgoing, Transport, TransportError, TransportEvent, TransportResult};
use async_trait::async_trait;
use bloom_domain::{
    Capabilities, Conversation, ConversationId, DeliveryState, Handle, HandleKind, Message,
    MessageId, OutgoingBody, TransportId,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};

/// Deterministic in-memory transport for daemon tests. Public so tests can depend on
/// bloom-transport directly without enabling a feature; it never contacts real users.
pub struct FakeTransport {
    id: TransportId,
    capabilities: Capabilities,
    conversations: Vec<Conversation>,
    receiver_taken: Mutex<bool>,
    sender: Mutex<Option<mpsc::Sender<TransportEvent>>>,
    started: Mutex<bool>,
    stopped: Mutex<bool>,
    history: Mutex<Vec<Message>>,
    next_id: AtomicU64,
    capacity: usize,
    shutdown: watch::Sender<bool>,
    emit_gate: AsyncMutex<()>,
}
impl FakeTransport {
    pub fn new(
        id: TransportId,
        capabilities: Capabilities,
        conversations: Vec<Conversation>,
        capacity: usize,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            id,
            capabilities,
            conversations,
            receiver_taken: Mutex::new(false),
            sender: Mutex::new(None),
            started: Mutex::new(false),
            stopped: Mutex::new(false),
            history: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            capacity: capacity.max(1),
            shutdown,
            emit_gate: AsyncMutex::new(()),
        }
    }
    pub async fn emit(&self, event: TransportEvent) -> TransportResult<()> {
        let _gate = self.emit_gate.lock().await;
        if !*self.started.lock().unwrap() || *self.stopped.lock().unwrap() {
            return Err(TransportError::TransportUnavailable);
        }
        let sender = self
            .sender
            .lock()
            .unwrap()
            .clone()
            .ok_or(TransportError::TransportUnavailable)?;
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(TransportError::TransportUnavailable);
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => Err(TransportError::TransportUnavailable),
            result = sender.send(event) => {
                if *shutdown.borrow() {
                    Err(TransportError::TransportUnavailable)
                } else {
                    result.map_err(|_| TransportError::TransportUnavailable)
                }
            },
        }
    }
}
#[async_trait]
impl Transport for FakeTransport {
    fn id(&self) -> TransportId {
        self.id.clone()
    }
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }
    async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
        let mut taken = self.receiver_taken.lock().unwrap();
        if *taken {
            return Err(TransportError::Internal);
        }
        *taken = true;
        let (tx, rx) = mpsc::channel(self.capacity);
        *self.sender.lock().unwrap() = Some(tx);
        Ok(rx)
    }
    async fn start(&self) -> TransportResult<()> {
        if self.sender.lock().unwrap().is_none() || *self.stopped.lock().unwrap() {
            return Err(TransportError::Internal);
        }
        *self.started.lock().unwrap() = true;
        Ok(())
    }
    async fn stop(&self) -> TransportResult<()> {
        *self.stopped.lock().unwrap() = true;
        self.shutdown.send_replace(true);
        // Wait until an in-flight send completes or observes shutdown.
        let _gate = self.emit_gate.lock().await;
        self.sender.lock().unwrap().take();
        Ok(())
    }
    async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
        Ok(self.conversations.clone())
    }
    async fn history(
        &self,
        conversation: &ConversationId,
        limit: u16,
    ) -> TransportResult<Vec<Message>> {
        let mut messages: Vec<_> = self
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.conversation_id() == conversation)
            .cloned()
            .collect();
        let start = messages.len().saturating_sub(limit as usize);
        messages.drain(..start);
        Ok(messages)
    }
    async fn send(
        &self,
        conversation: &ConversationId,
        body: OutgoingBody,
    ) -> TransportResult<MessageId> {
        if !*self.started.lock().unwrap() || *self.stopped.lock().unwrap() {
            return Err(TransportError::TransportUnavailable);
        }
        let Some(target) = self.conversations.iter().find(|c| &c.id == conversation) else {
            return Err(TransportError::RecipientUnreachable);
        };
        if target.kind == bloom_domain::ConversationKind::Group && !self.capabilities.groups {
            return Err(TransportError::Unsupported);
        }
        validate_outgoing(&self.capabilities, &body)?;
        let id = MessageId::new(
            self.id.clone(),
            format!("fake-{}", self.next_id.fetch_add(1, Ordering::Relaxed)),
        )
        .map_err(|_| TransportError::Internal)?;
        let sender =
            Handle::new(HandleKind::Opaque, "fake-self").map_err(|_| TransportError::Internal)?;
        let message = Message::new(
            id.clone(),
            conversation.clone(),
            None,
            sender,
            body.parts,
            0,
            None,
            DeliveryState::Queued,
            true,
            None,
            vec![],
            None,
        )
        .map_err(|_| TransportError::Internal)?;
        self.history.lock().unwrap().push(message);
        Ok(id)
    }
}
