use bloom_domain::{Conversation, ConversationId, Message, MessageId, OutgoingBody, TransportId};
use bloom_transport::{Transport, TransportError, TransportEvent, TransportResult};
use std::{future::Future, sync::Arc};

/// A single-use transport session. Construct a new session with a fresh transport to retry.
pub struct Session {
    transport: Arc<dyn Transport>,
    lifecycle: tokio::sync::Mutex<State>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    Running,
    Stopped,
}

impl Session {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            lifecycle: tokio::sync::Mutex::new(State::New),
            shutdown: tokio::sync::watch::channel(false).0,
        }
    }

    pub fn id(&self) -> TransportId {
        self.transport.id()
    }

    /// Takes the sole event receiver before starting the transport.
    pub async fn start(&self) -> TransportResult<tokio::sync::mpsc::Receiver<TransportEvent>> {
        let mut state = self.lifecycle.lock().await;
        if *state != State::New {
            return Err(TransportError::TransportUnavailable);
        }
        let events = self.transport.take_events().await?;
        if let Err(error) = self.transport.start().await {
            *state = State::Stopped;
            let _ = self.transport.stop().await;
            return Err(error);
        }
        *state = State::Running;
        Ok(events)
    }

    // Do not hold the lifecycle lock across transport I/O. Shutdown must cancel a stuck call.
    async fn while_active<T>(
        &self,
        operation: impl Future<Output = TransportResult<T>>,
    ) -> TransportResult<T> {
        let mut shutdown = self.shutdown.subscribe();
        if *self.lifecycle.lock().await != State::Running {
            return Err(TransportError::TransportUnavailable);
        }
        tokio::select! {
            biased;
            _ = async {
                if !*shutdown.borrow_and_update() {
                    let _ = shutdown.changed().await;
                }
            } => Err(TransportError::TransportUnavailable),
            result = operation => result,
        }
    }

    pub async fn send(
        &self,
        conversation: &ConversationId,
        body: OutgoingBody,
    ) -> TransportResult<MessageId> {
        self.while_active(self.transport.send(conversation, body))
            .await
    }

    pub async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
        self.while_active(self.transport.conversations()).await
    }

    pub async fn history(
        &self,
        conversation: &ConversationId,
        limit: u16,
    ) -> TransportResult<Vec<Message>> {
        self.while_active(self.transport.history(conversation, limit))
            .await
    }

    pub(crate) async fn stop_transport(&self) -> TransportResult<()> {
        let mut state = self.lifecycle.lock().await;
        if *state == State::Stopped {
            return Ok(());
        }
        *state = State::Stopped;
        self.shutdown.send_replace(true);
        self.transport.stop().await
    }
}

pub(crate) use Session as ManagedSession;
