//! Reusable assertions for transport integration tests.
use crate::{testing::FakeTransport, Transport, TransportError, TransportEvent};

/// Verify the single-consumer, pre-start receiver and idempotent shutdown contract.
pub async fn assert_lifecycle<T: Transport>(transport: &T) {
    assert!(
        transport.start().await.is_err(),
        "start before take_events must fail"
    );
    let mut events = transport.take_events().await.expect("first receiver");
    assert_eq!(
        transport.take_events().await.unwrap_err(),
        TransportError::Internal
    );
    transport.start().await.unwrap();
    transport.start().await.unwrap();
    transport.stop().await.unwrap();
    transport.stop().await.unwrap();
    assert!(
        events.recv().await.is_none(),
        "shutdown must close event stream"
    );
}

/// Verify a producer cannot silently lose events when its sole consumer is dropped.
pub async fn assert_receiver_close(transport: &FakeTransport) {
    let receiver = transport.take_events().await.expect("receiver");
    transport.start().await.unwrap();
    drop(receiver);
    assert_eq!(
        transport.emit(TransportEvent::ConversationsChanged).await,
        Err(TransportError::TransportUnavailable),
        "a closed receiver must fail the producer, not silently discard an event"
    );
    transport.stop().await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeTransport;
    use bloom_domain::{Capabilities, TransportId};

    fn fake() -> FakeTransport {
        FakeTransport::new(
            TransportId::new("conformance").unwrap(),
            Capabilities::default(),
            vec![],
            1,
        )
    }

    #[tokio::test]
    async fn fake_satisfies_lifecycle_contract() {
        assert_lifecycle(&fake()).await;
    }

    #[tokio::test]
    async fn fake_send_requires_an_active_session() {
        let transport = fake();
        let conversation = bloom_domain::ConversationId::new(transport.id(), "c").unwrap();
        let body = bloom_domain::OutgoingBody { parts: vec![] };
        assert_eq!(
            transport.send(&conversation, body.clone()).await,
            Err(TransportError::TransportUnavailable)
        );
        let _events = transport.take_events().await.unwrap();
        transport.start().await.unwrap();
        transport.stop().await.unwrap();
        assert_eq!(
            transport.send(&conversation, body).await,
            Err(TransportError::TransportUnavailable)
        );
    }

    #[tokio::test]
    async fn fake_rejects_emit_after_receiver_close() {
        assert_receiver_close(&fake()).await;
    }

    #[tokio::test]
    async fn stop_cancels_blocked_emit_without_drain() {
        let transport = std::sync::Arc::new(fake());
        let receiver = transport.take_events().await.unwrap();
        transport.start().await.unwrap();
        transport
            .emit(TransportEvent::ConversationsChanged)
            .await
            .unwrap();
        let emitting = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.emit(TransportEvent::ConversationsChanged).await })
        };
        tokio::task::yield_now().await;
        transport.stop().await.unwrap();
        assert_eq!(
            emitting.await.unwrap(),
            Err(TransportError::TransportUnavailable)
        );
        drop(receiver);
        assert_eq!(
            transport.emit(TransportEvent::ConversationsChanged).await,
            Err(TransportError::TransportUnavailable)
        );
    }
}
