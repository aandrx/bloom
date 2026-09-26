use crate::session::ManagedSession;
use bloom_transport::{Transport, TransportEvent, TransportResult};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

pub struct Supervisor {
    session: Arc<ManagedSession>,
    sink: mpsc::Sender<TransportEvent>,
    forwarder: Option<tokio::task::JoinHandle<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Events observed to be discarded because the sink was closed.
    pub discarded_events: usize,
    /// True if forwarding was cancelled before the event queue drained completely.
    pub drain_incomplete: bool,
}

impl Supervisor {
    pub fn new(transport: Arc<dyn Transport>, sink: mpsc::Sender<TransportEvent>) -> Self {
        Self {
            session: Arc::new(ManagedSession::new(transport)),
            sink,
            forwarder: None,
        }
    }

    /// Clone a session handle so requests can run while the supervisor owns shutdown.
    pub fn session(&self) -> Arc<crate::session::Session> {
        self.session.clone()
    }

    pub async fn start(&mut self) -> TransportResult<()> {
        let mut events = self.session.start().await?;
        let sink = self.sink.clone();
        self.forwarder = Some(tokio::spawn(async move {
            let mut discarded = 0;
            while let Some(event) = events.recv().await {
                if sink.send(event).await.is_err() {
                    discarded += 1 + events.len();
                    break;
                }
            }
            discarded
        }));
        Ok(())
    }

    pub async fn send(
        &self,
        conversation: &bloom_domain::ConversationId,
        body: bloom_domain::OutgoingBody,
    ) -> TransportResult<bloom_domain::MessageId> {
        self.session.send(conversation, body).await
    }

    /// Stops the producer while allowing forwarding a bounded drain window. Cancelling the
    /// forwarding receiver releases a producer blocked on its bounded event channel.
    pub async fn shutdown(&mut self, drain_timeout: Duration) -> TransportResult<ShutdownReport> {
        let stop = self.session.stop_transport();
        tokio::pin!(stop);
        let mut drain_incomplete = false;
        let mut discarded_events = 0;
        if let Some(mut task) = self.forwarder.take() {
            let mut stop_result = None;
            let mut forward_result = None;
            let drained = tokio::time::timeout(drain_timeout, async {
                tokio::join!(async { stop_result = Some(stop.as_mut().await) }, async {
                    forward_result = Some((&mut task).await)
                },);
            })
            .await;
            if drained.is_err() {
                drain_incomplete = true;
                if forward_result.is_none() {
                    task.abort();
                    // Joining drops the sole transport receiver and unblocks a producer send.
                    let _ = task.await;
                }
                if stop_result.is_none() {
                    stop_result = Some(stop.as_mut().await);
                }
            }
            stop_result.expect("stop was awaited")?;
            if let Some(result) = forward_result {
                match result {
                    Ok(count) => discarded_events = count,
                    Err(_) => drain_incomplete = true,
                }
            }
        } else {
            stop.await?;
        }
        Ok(ShutdownReport {
            discarded_events,
            drain_incomplete,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bloom_domain::{
        Capabilities, Conversation, ConversationId, Message, MessageId, OutgoingBody, TransportId,
    };
    use bloom_transport::{testing::FakeTransport, TransportError};
    use std::time::Duration;
    use tokio::sync::Notify;

    fn fixture(
        transport_capacity: usize,
        sink_capacity: usize,
    ) -> (
        Arc<FakeTransport>,
        Supervisor,
        mpsc::Receiver<TransportEvent>,
    ) {
        let transport = Arc::new(FakeTransport::new(
            TransportId::new("test").unwrap(),
            Capabilities::default(),
            vec![],
            transport_capacity,
        ));
        let (sink, receiver) = mpsc::channel(sink_capacity);
        (
            transport.clone(),
            Supervisor::new(transport, sink),
            receiver,
        )
    }

    struct HangingSend {
        inner: FakeTransport,
        entered: Arc<Notify>,
    }

    #[async_trait]
    impl Transport for HangingSend {
        fn id(&self) -> TransportId {
            self.inner.id()
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
            self.inner.take_events().await
        }
        async fn start(&self) -> TransportResult<()> {
            self.inner.start().await
        }
        async fn stop(&self) -> TransportResult<()> {
            self.inner.stop().await
        }
        async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
            Ok(vec![])
        }
        async fn history(&self, _: &ConversationId, _: u16) -> TransportResult<Vec<Message>> {
            Ok(vec![])
        }
        async fn send(&self, _: &ConversationId, _: OutgoingBody) -> TransportResult<MessageId> {
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_stuck_send() {
        let entered = Arc::new(Notify::new());
        let transport = Arc::new(HangingSend {
            inner: FakeTransport::new(
                TransportId::new("stuck").unwrap(),
                Capabilities::default(),
                vec![],
                1,
            ),
            entered: entered.clone(),
        });
        let (sink, _receiver) = mpsc::channel(1);
        let mut supervisor = Supervisor::new(transport, sink);
        supervisor.start().await.unwrap();
        let session = supervisor.session();
        let conversation =
            ConversationId::new(TransportId::new("stuck").unwrap(), "thread").unwrap();
        let send_task = tokio::spawn(async move {
            session
                .send(&conversation, OutgoingBody { parts: vec![] })
                .await
        });
        entered.notified().await;
        let report = tokio::time::timeout(
            Duration::from_millis(250),
            supervisor.shutdown(Duration::from_millis(30)),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!report.drain_incomplete);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), send_task)
                .await
                .unwrap()
                .unwrap(),
            Err(TransportError::TransportUnavailable)
        );
    }

    #[tokio::test]
    async fn forwards_events_in_order_and_shutdown_is_idempotent() {
        let (transport, mut supervisor, mut sink) = fixture(2, 2);
        supervisor.start().await.unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        assert!(matches!(
            sink.recv().await,
            Some(TransportEvent::ConversationsChanged)
        ));
        assert!(matches!(
            sink.recv().await,
            Some(TransportEvent::ConversationsChanged)
        ));
        let report = supervisor.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(!report.drain_incomplete);
        assert_eq!(
            supervisor.shutdown(Duration::from_secs(1)).await.unwrap(),
            report
        );
    }

    #[tokio::test]
    async fn send_rejected_before_start_and_after_shutdown() {
        let (_, mut supervisor, _) = fixture(1, 1);
        let conversation = ConversationId::new(TransportId::new("test").unwrap(), "c").unwrap();
        assert_eq!(
            supervisor
                .send(&conversation, OutgoingBody { parts: vec![] })
                .await
                .unwrap_err(),
            TransportError::TransportUnavailable
        );
        supervisor.start().await.unwrap();
        supervisor.shutdown(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            supervisor
                .send(&conversation, OutgoingBody { parts: vec![] })
                .await
                .unwrap_err(),
            TransportError::TransportUnavailable
        );
    }

    #[tokio::test]
    async fn stalled_sink_does_not_block_shutdown() {
        let (transport, mut supervisor, _sink) = fixture(2, 1);
        supervisor.start().await.unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        let producer = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.emit(TransportEvent::ConversationsChanged).await })
        };
        tokio::task::yield_now().await;
        let shutdown = supervisor.shutdown(Duration::from_millis(30));
        let report = tokio::time::timeout(Duration::from_millis(250), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(report.drain_incomplete);
        let _producer_result = tokio::time::timeout(Duration::from_millis(250), producer)
            .await
            .unwrap()
            .unwrap();
    }
}
