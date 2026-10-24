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

    /// Ends the current forwarding generation without stopping the transport, so a
    /// replacement session can take over the same sink. Aborts forwarding if the
    /// producer does not close the event channel within `drain_timeout`.
    ///
    /// A second call after a drained generation is a no-op returning zero counts.
    /// An aborted drain reports `drain_incomplete` only for the call that aborted it;
    /// that incomplete result is not retained for later calls.
    pub async fn finish_generation(&mut self, drain_timeout: Duration) -> ShutdownReport {
        let mut drain_incomplete = false;
        let mut discarded_events = 0;
        if let Some(mut task) = self.forwarder.take() {
            match tokio::time::timeout(drain_timeout, &mut task).await {
                Ok(Ok(count)) => discarded_events = count,
                Ok(Err(_)) => drain_incomplete = true,
                Err(_) => {
                    drain_incomplete = true;
                    task.abort();
                    // Joining drops the sole transport receiver and leaves no live task.
                    let _ = task.await;
                }
            }
        }
        ShutdownReport {
            discarded_events,
            drain_incomplete,
        }
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
    use std::sync::atomic::{AtomicBool, Ordering};
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

    /// FakeTransport cannot close its event channel without stopping, so this variant
    /// exposes explicit channel closure and records whether `stop` was invoked.
    struct ScriptedTransport {
        id: TransportId,
        capabilities: Capabilities,
        events: std::sync::Mutex<Option<mpsc::Sender<TransportEvent>>>,
        capacity: usize,
        stop_called: Arc<AtomicBool>,
    }

    impl ScriptedTransport {
        fn new(name: &str) -> Self {
            Self {
                id: TransportId::new(name).unwrap(),
                capabilities: Capabilities::default(),
                events: std::sync::Mutex::new(None),
                capacity: 8,
                stop_called: Arc::new(AtomicBool::new(false)),
            }
        }

        async fn emit(&self, event: TransportEvent) -> TransportResult<()> {
            let sender = self
                .events
                .lock()
                .unwrap()
                .clone()
                .ok_or(TransportError::TransportUnavailable)?;
            sender
                .send(event)
                .await
                .map_err(|_| TransportError::TransportUnavailable)
        }

        fn close(&self) {
            self.events.lock().unwrap().take();
        }

        async fn receiver_closed(&self) {
            let sender = self
                .events
                .lock()
                .unwrap()
                .clone()
                .expect("event channel was opened");
            sender.closed().await;
        }

        fn events_waiting(&self) -> usize {
            let sender = self
                .events
                .lock()
                .unwrap()
                .clone()
                .expect("event channel was opened");
            sender.max_capacity() - sender.capacity()
        }

        /// Yield until the forwarder has taken the emitted event, which with a full
        /// sink means it is now blocked inside `sink.send`.
        async fn wait_until_forwarder_attempts(&self) {
            for _ in 0..200 {
                if self.events_waiting() == 0 {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("forwarder did not take the emitted event");
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        fn id(&self) -> TransportId {
            self.id.clone()
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }
        async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
            let mut events = self.events.lock().unwrap();
            if events.is_some() {
                return Err(TransportError::Internal);
            }
            let (sender, receiver) = mpsc::channel(self.capacity);
            *events = Some(sender);
            Ok(receiver)
        }
        async fn start(&self) -> TransportResult<()> {
            if self.events.lock().unwrap().is_none() {
                return Err(TransportError::Internal);
            }
            Ok(())
        }
        async fn stop(&self) -> TransportResult<()> {
            self.stop_called.store(true, Ordering::SeqCst);
            self.close();
            Ok(())
        }
        async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
            Ok(vec![])
        }
        async fn history(&self, _: &ConversationId, _: u16) -> TransportResult<Vec<Message>> {
            Ok(vec![])
        }
        async fn send(&self, _: &ConversationId, _: OutgoingBody) -> TransportResult<MessageId> {
            Err(TransportError::TransportUnavailable)
        }
    }

    #[tokio::test]
    async fn finish_generation_waits_for_a_pending_cross_generation_send() {
        let (sink, mut received) = mpsc::channel(1);
        sink.send(TransportEvent::ConversationsChanged)
            .await
            .unwrap();

        let first_transport = Arc::new(ScriptedTransport::new("first"));
        let mut first = Supervisor::new(first_transport.clone(), sink.clone());
        first.start().await.unwrap();
        first_transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        first_transport.wait_until_forwarder_attempts().await;

        let finish = first.finish_generation(Duration::from_secs(2));
        tokio::pin!(finish);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut finish)
                .await
                .is_err(),
            "finish_generation returned while a cross-generation send was still pending"
        );

        let sentinel = received.recv().await.expect("sink stayed open");
        let event_a = received.recv().await.expect("sink stayed open");
        first_transport.close();

        let report = tokio::time::timeout(Duration::from_secs(1), finish)
            .await
            .unwrap();
        assert_eq!(
            report,
            ShutdownReport {
                discarded_events: 0,
                drain_incomplete: false,
            }
        );
        assert!(!first_transport.stop_called.load(Ordering::SeqCst));

        let second_transport = Arc::new(ScriptedTransport::new("second"));
        let mut second = Supervisor::new(second_transport.clone(), sink.clone());
        second.start().await.unwrap();
        second_transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        second_transport.close();

        let report = second.finish_generation(Duration::from_secs(1)).await;
        assert_eq!(
            report,
            ShutdownReport {
                discarded_events: 0,
                drain_incomplete: false,
            }
        );
        assert!(!second_transport.stop_called.load(Ordering::SeqCst));

        let event_b = received.recv().await.expect("sink stayed open");
        assert!(received.try_recv().is_err(), "sink received extra events");
        assert!(matches!(sentinel, TransportEvent::ConversationsChanged));
        assert!(matches!(event_a, TransportEvent::ConversationsChanged));
        assert!(matches!(event_b, TransportEvent::ConversationsChanged));

        let idle = second.finish_generation(Duration::from_millis(50)).await;
        assert_eq!(
            idle,
            ShutdownReport {
                discarded_events: 0,
                drain_incomplete: false,
            }
        );
        assert!(!second_transport.stop_called.load(Ordering::SeqCst));
        assert!(!first_transport.stop_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn finish_generation_times_out_and_aborts_an_open_forwarder() {
        let transport = Arc::new(ScriptedTransport::new("hanging"));
        let (sink, _received) = mpsc::channel(1);
        let mut supervisor = Supervisor::new(transport.clone(), sink);
        supervisor.start().await.unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();

        let report = tokio::time::timeout(
            Duration::from_millis(250),
            supervisor.finish_generation(Duration::from_millis(20)),
        )
        .await
        .unwrap();
        assert!(report.drain_incomplete);
        assert_eq!(report.discarded_events, 0);
        assert!(!transport.stop_called.load(Ordering::SeqCst));

        tokio::time::timeout(Duration::from_millis(250), transport.receiver_closed())
            .await
            .expect("aborted forwarder still holds the event receiver");
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
