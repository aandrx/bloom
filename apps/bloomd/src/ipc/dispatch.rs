//! Request/response handling on an authenticated local socket.
use std::{future::Future, pin::Pin, time::Duration};

use bloom_domain::{Conversation, ConversationId, Message, MessageId, OutgoingBody};
use bloom_ipc::{
    DraftKey, DraftLoad, DraftWriteOutcome, DurableDraft, HistoryCursor, MessageSearchHit,
    ParticipantFilter, ProtocolError, ReadinessSnapshot, RequestKind, Response, ResponseKind,
};
use tokio::{io::AsyncWrite, time::timeout};

use super::{
    listener::{AuthenticatedStream, MAX_HANDSHAKE_IDLE},
    subscription::{serve_subscriber, SubscriptionHub},
};

pub const REQUEST_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub type HandlerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProtocolError>> + Send + 'a>>;

pub trait RequestHandler: Send + Sync {
    fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>>;
    fn sync_conversations(&self) -> HandlerFuture<'_, (bloom_ipc::ConversationSyncStatus, u32)>;
    fn history(
        &self,
        conversation_id: ConversationId,
        limit: u16,
    ) -> HandlerFuture<'_, Vec<Message>>;
    fn history_before(
        &self,
        conversation_id: ConversationId,
        limit: u16,
        before: HistoryCursor,
    ) -> HandlerFuture<'_, Vec<Message>>;
    fn search_messages(
        &self,
        query: String,
        limit: u16,
    ) -> HandlerFuture<'_, Vec<MessageSearchHit>>;
    fn search_messages_with_participant(
        &self,
        query: String,
        limit: u16,
        participant: Option<ParticipantFilter>,
    ) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
        if participant.is_some() {
            Box::pin(async { Err(ProtocolError::Unsupported) })
        } else {
            self.search_messages(query, limit)
        }
    }
    fn message_context(
        &self,
        _conversation_id: ConversationId,
        _message_id: MessageId,
        _limit: u16,
    ) -> HandlerFuture<'_, Vec<Message>> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn send(
        &self,
        conversation_id: ConversationId,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId>;
    fn send_attachment(
        &self,
        _conversation_id: ConversationId,
        _path: String,
    ) -> HandlerFuture<'_, MessageId> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn start_conversation(
        &self,
        address: String,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId>;
    fn draft_save(
        &self,
        _key: DraftKey,
        _draft: DurableDraft,
        _revision: u64,
    ) -> HandlerFuture<'_, DraftWriteOutcome> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn draft_load(&self, _key: DraftKey) -> HandlerFuture<'_, DraftLoad> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn draft_delete(&self, _key: DraftKey, _revision: u64) -> HandlerFuture<'_, DraftWriteOutcome> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn fetch_attachment(&self, _token: String) -> HandlerFuture<'_, (String, u64)> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
    fn readiness(&self) -> HandlerFuture<'_, ReadinessSnapshot> {
        Box::pin(async { Err(ProtocolError::Unsupported) })
    }
}

/// Serve sequential requests. Codec errors and idle timeouts close this connection.
pub async fn serve<H: RequestHandler>(connection: AuthenticatedStream, handler: &H) {
    serve_inner(connection, handler, None, MAX_HANDSHAKE_IDLE).await;
}

/// Serve requests with a dedicated subscription path when Subscribe is the first frame.
pub async fn serve_with_subscriptions<H: RequestHandler>(
    connection: AuthenticatedStream,
    handler: &H,
    hub: &SubscriptionHub,
) {
    serve_inner(connection, handler, Some(hub), MAX_HANDSHAKE_IDLE).await;
}

#[cfg(test)]
async fn serve_with_first_timeout<H: RequestHandler>(
    connection: AuthenticatedStream,
    handler: &H,
    first_timeout: Duration,
) {
    serve_inner(connection, handler, None, first_timeout).await;
}

async fn serve_inner<H: RequestHandler>(
    mut connection: AuthenticatedStream,
    handler: &H,
    hub: Option<&SubscriptionHub>,
    first_timeout: Duration,
) {
    let mut first = true;
    loop {
        let idle = if first {
            first_timeout
        } else {
            REQUEST_IDLE_TIMEOUT
        };
        let request = match timeout(idle, bloom_ipc::read_request(connection.stream_mut())).await {
            Ok(Ok(request)) => request,
            _ => return,
        };
        if first && matches!(&request.request, RequestKind::Subscribe) {
            if let Some(hub) = hub {
                let (receiver, initial) = hub.subscribe_with_replay();
                serve_subscriber(connection, request.request_id, receiver, initial).await;
                return;
            }
        }
        first = false;
        let response = match request.request {
            RequestKind::ListConversations => match handler.list_conversations().await {
                Ok(conversations) => ResponseKind::Conversations { conversations },
                Err(error) => ResponseKind::Error { error },
            },
            RequestKind::SyncConversations => match handler.sync_conversations().await {
                Ok((status, conversations)) => ResponseKind::ConversationSync {
                    status,
                    conversations,
                },
                Err(error) => ResponseKind::Error { error },
            },
            RequestKind::History {
                conversation_id,
                limit,
                before,
            } => {
                if limit == 0 || limit > u16::MAX as u32 {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    let result = match before {
                        Some(before) => {
                            handler
                                .history_before(conversation_id, limit as u16, before)
                                .await
                        }
                        None => handler.history(conversation_id, limit as u16).await,
                    };
                    match result {
                        Ok(messages) => ResponseKind::History { messages },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::Send {
                conversation_id,
                body,
            } => match handler.send(conversation_id, body).await {
                Ok(message_id) => ResponseKind::Send { message_id },
                Err(error) => ResponseKind::Error { error },
            },
            RequestKind::SendAttachment {
                conversation_id,
                path,
            } => match handler.send_attachment(conversation_id, path).await {
                Ok(message_id) => ResponseKind::Send { message_id },
                Err(error) => ResponseKind::Error { error },
            },
            RequestKind::StartConversation { address, body } => {
                match handler.start_conversation(address, body).await {
                    Ok(message_id) => ResponseKind::Send { message_id },
                    Err(error) => ResponseKind::Error { error },
                }
            }
            RequestKind::SearchMessages {
                query,
                limit,
                participant,
            } => {
                if limit == 0 || limit > u16::MAX as u32 {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler
                        .search_messages_with_participant(query, limit as u16, participant)
                        .await
                    {
                        Ok(hits) => ResponseKind::SearchResults { hits },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::MessageContext {
                conversation_id,
                message_id,
                limit,
            } => {
                if limit == 0 || limit > bloom_ipc::MAX_MESSAGE_CONTEXT_LIMIT {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler
                        .message_context(conversation_id, message_id, limit as u16)
                        .await
                    {
                        Ok(messages) => ResponseKind::MessageContext { messages },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::DraftSave {
                key,
                draft,
                revision,
            } => {
                if !key.is_valid()
                    || !draft.is_valid()
                    || !bloom_ipc::draft_revision_is_valid(revision)
                {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler.draft_save(key, draft, revision).await {
                        Ok(outcome) => ResponseKind::DraftSave { outcome },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::DraftLoad { key } => {
                if !key.is_valid() {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler.draft_load(key).await {
                        Ok(load) => ResponseKind::DraftLoad {
                            draft: load.draft,
                            revision_floor: load.revision_floor,
                            corrupt_deleted: load.corrupt_deleted,
                        },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::DraftDelete { key, revision } => {
                if !key.is_valid() || !bloom_ipc::draft_revision_is_valid(revision) {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler.draft_delete(key, revision).await {
                        Ok(outcome) => ResponseKind::DraftDelete { outcome },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::FetchAttachment { token } => {
                if !bloom_domain::AttachmentRef::is_valid_fetch_token(&token) {
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest,
                    }
                } else {
                    match handler.fetch_attachment(token).await {
                        Ok((path, bytes)) => ResponseKind::AttachmentFetched { path, bytes },
                        Err(error) => ResponseKind::Error { error },
                    }
                }
            }
            RequestKind::Readiness => match handler.readiness().await {
                Ok(snapshot) => ResponseKind::Readiness { snapshot },
                Err(error) => ResponseKind::Error { error },
            },
            RequestKind::Subscribe => ResponseKind::Error {
                error: if hub.is_some() {
                    ProtocolError::InvalidRequest
                } else {
                    ProtocolError::Unsupported
                },
            },
        };
        let response = Response {
            request_id: request.request_id,
            response,
        };
        if write_response_bounded(connection.stream_mut(), &response, RESPONSE_WRITE_TIMEOUT)
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn write_response_bounded<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Response,
    deadline: Duration,
) -> Result<(), ()> {
    // `write_frame` serializes the whole payload before writing any bytes, so a
    // `TooLarge` failure leaves the stream untouched and a bounded replacement
    // response is safe to write on it. Every other failure stays fatal.
    timeout(deadline, async {
        match bloom_ipc::write_response(writer, response).await {
            Ok(()) => Ok(()),
            Err(bloom_ipc::CodecError::TooLarge) => {
                let fallback = Response {
                    request_id: response.request_id,
                    response: ResponseKind::Error {
                        error: ProtocolError::Internal,
                    },
                };
                bloom_ipc::write_response(writer, &fallback)
                    .await
                    .map_err(|_| ())
            }
            Err(_) => Err(()),
        }
    })
    .await
    .map_err(|_| ())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_domain::TransportId;
    use bloom_ipc::{read_event, read_response, write_request, Event, EventKind, Request};
    use std::{
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
    };

    /// Counts newest-page reads and cursored reads separately, so a test can prove
    /// which handler a request reached.
    struct FakeHandler(Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>);
    impl RequestHandler for FakeHandler {
        fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn sync_conversations(
            &self,
        ) -> HandlerFuture<'_, (bloom_ipc::ConversationSyncStatus, u32)> {
            Box::pin(async { Ok((bloom_ipc::ConversationSyncStatus::Complete, 0)) })
        }
        fn history(&self, _: ConversationId, _: u16) -> HandlerFuture<'_, Vec<Message>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![]) })
        }
        fn history_before(
            &self,
            _: ConversationId,
            _: u16,
            _: HistoryCursor,
        ) -> HandlerFuture<'_, Vec<Message>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![]) })
        }
        fn search_messages(&self, _: String, _: u16) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
            self.2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(vec![MessageSearchHit {
                    conversation_id: ConversationId::new(
                        TransportId::new("fake").unwrap(),
                        "conversation",
                    )
                    .unwrap(),
                    message_id: MessageId::new(TransportId::new("fake").unwrap(), "hit").unwrap(),
                    created_at: 7,
                    snippet: "a hit snippet".into(),
                }])
            })
        }
        fn send(&self, _: ConversationId, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async {
                Ok(MessageId::new(TransportId::new("fake").unwrap(), "message").unwrap())
            })
        }
        fn start_conversation(&self, _: String, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            self.send(
                ConversationId::new(TransportId::new("fake").unwrap(), "unused").unwrap(),
                OutgoingBody { parts: vec![] },
            )
        }
    }

    struct ErrorHandler;
    impl RequestHandler for ErrorHandler {
        fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>> {
            Box::pin(async { Err(ProtocolError::AccountThrottled) })
        }
        fn readiness(&self) -> HandlerFuture<'_, ReadinessSnapshot> {
            Box::pin(async {
                Ok(ReadinessSnapshot {
                    generation: 0,
                    operation: 0,
                    revision: 0,
                    phase: bloom_ipc::ReadinessPhase::Starting,
                    outcome: None,
                    observed_records: 0,
                    can_send: false,
                    engine_ready: false,
                    bounded_error: None,
                })
            })
        }
        fn sync_conversations(
            &self,
        ) -> HandlerFuture<'_, (bloom_ipc::ConversationSyncStatus, u32)> {
            Box::pin(async { Err(ProtocolError::Unsupported) })
        }
        fn history(&self, _: ConversationId, _: u16) -> HandlerFuture<'_, Vec<Message>> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
        fn history_before(
            &self,
            _: ConversationId,
            _: u16,
            _: HistoryCursor,
        ) -> HandlerFuture<'_, Vec<Message>> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
        fn search_messages(&self, _: String, _: u16) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
        fn send(&self, _: ConversationId, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
        fn start_conversation(&self, _: String, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
    }

    struct DraftHandler(Arc<AtomicUsize>);
    impl RequestHandler for DraftHandler {
        fn list_conversations(&self) -> HandlerFuture<'_, Vec<Conversation>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn sync_conversations(
            &self,
        ) -> HandlerFuture<'_, (bloom_ipc::ConversationSyncStatus, u32)> {
            Box::pin(async { Ok((bloom_ipc::ConversationSyncStatus::Complete, 0)) })
        }
        fn history(&self, _: ConversationId, _: u16) -> HandlerFuture<'_, Vec<Message>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn history_before(
            &self,
            _: ConversationId,
            _: u16,
            _: HistoryCursor,
        ) -> HandlerFuture<'_, Vec<Message>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn search_messages(&self, _: String, _: u16) -> HandlerFuture<'_, Vec<MessageSearchHit>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn send(&self, _: ConversationId, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Unsupported) })
        }
        fn start_conversation(&self, _: String, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Unsupported) })
        }
        fn draft_save(
            &self,
            _: DraftKey,
            _: DurableDraft,
            _: u64,
        ) -> HandlerFuture<'_, DraftWriteOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(DraftWriteOutcome::Applied) })
        }
        fn draft_load(&self, _: DraftKey) -> HandlerFuture<'_, DraftLoad> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(DraftLoad {
                    draft: None,
                    revision_floor: 4,
                    corrupt_deleted: true,
                })
            })
        }
        fn draft_delete(&self, _: DraftKey, _: u64) -> HandlerFuture<'_, DraftWriteOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(DraftWriteOutcome::Stale) })
        }
    }

    #[tokio::test]
    async fn draft_requests_are_validated_and_answered_with_typed_results() {
        let (_dir, listener, path) = socket();
        let handled = Arc::new(AtomicUsize::new(0));
        let server_handled = handled.clone();
        let server = tokio::spawn(async move {
            serve(
                listener.accept().await.unwrap(),
                &DraftHandler(server_handled),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        let key = DraftKey::Conversation {
            owner: ConversationId::new(TransportId::new("fake").unwrap(), "exact").unwrap(),
        };
        let draft = DurableDraft {
            body: "body".into(),
            cursor: 4,
            pasted_body: false,
            uncertain: false,
        };
        let requests = [
            (
                31,
                RequestKind::DraftSave {
                    key: key.clone(),
                    draft: draft.clone(),
                    revision: 1,
                },
                ResponseKind::DraftSave {
                    outcome: DraftWriteOutcome::Applied,
                },
            ),
            (
                32,
                RequestKind::DraftSave {
                    key: key.clone(),
                    draft: DurableDraft {
                        body: "x".repeat(bloom_ipc::MAX_DRAFT_BYTES + 1),
                        cursor: 0,
                        pasted_body: false,
                        uncertain: false,
                    },
                    revision: 1,
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
            (
                33,
                RequestKind::DraftSave {
                    key: key.clone(),
                    draft: DurableDraft {
                        body: "abc".into(),
                        cursor: 4,
                        pasted_body: false,
                        uncertain: false,
                    },
                    revision: 1,
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
            (
                34,
                RequestKind::DraftSave {
                    key: DraftKey::NewChat {
                        transport: TransportId::new("fake").unwrap(),
                        address: "   ".into(),
                    },
                    draft: draft.clone(),
                    revision: 1,
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
            (
                35,
                RequestKind::DraftSave {
                    key: key.clone(),
                    draft: draft.clone(),
                    revision: u64::MAX,
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
            (
                36,
                RequestKind::DraftLoad { key: key.clone() },
                ResponseKind::DraftLoad {
                    draft: None,
                    revision_floor: 4,
                    corrupt_deleted: true,
                },
            ),
            (
                37,
                RequestKind::DraftLoad {
                    key: DraftKey::NewChat {
                        transport: TransportId::new("fake").unwrap(),
                        address: " ".into(),
                    },
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
            (
                38,
                RequestKind::DraftDelete {
                    key: key.clone(),
                    revision: 2,
                },
                ResponseKind::DraftDelete {
                    outcome: DraftWriteOutcome::Stale,
                },
            ),
            (
                39,
                RequestKind::DraftDelete {
                    key,
                    revision: u64::MAX,
                },
                ResponseKind::Error {
                    error: ProtocolError::InvalidRequest,
                },
            ),
        ];
        for (request_id, request, expected) in requests {
            write_request(
                &mut client,
                &Request {
                    request_id,
                    request,
                },
            )
            .await
            .unwrap();
            let response = read_response(&mut client).await.unwrap();
            assert_eq!(response.request_id, request_id);
            assert!(response.response == expected);
        }
        drop(client);
        server.await.unwrap();
        assert_eq!(
            handled.load(Ordering::SeqCst),
            3,
            "only valid draft requests reach the handler"
        );
    }

    #[tokio::test]
    async fn readiness_is_queryable_before_the_refresh_finishes() {
        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            serve(listener.accept().await.unwrap(), &ErrorHandler).await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 76,
                request: RequestKind::Readiness,
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert_eq!(response.request_id, 76);
        match response.response {
            ResponseKind::Readiness { snapshot } => {
                assert_eq!(snapshot.phase, bloom_ipc::ReadinessPhase::Starting);
                assert!(!snapshot.can_send);
            }
            _ => panic!("expected readiness response"),
        }
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handler_error_remains_typed() {
        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            serve(listener.accept().await.unwrap(), &ErrorHandler).await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 77,
                request: RequestKind::ListConversations,
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert_eq!(response.request_id, 77);
        assert!(matches!(
            response.response,
            ResponseKind::Error {
                error: ProtocolError::AccountThrottled
            }
        ));
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sync_request_returns_typed_status_and_count() {
        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            serve(
                listener.accept().await.unwrap(),
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                ),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 78,
                request: RequestKind::SyncConversations,
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert_eq!(response.request_id, 78);
        assert!(matches!(
            response.response,
            ResponseKind::ConversationSync {
                status: bloom_ipc::ConversationSyncStatus::Complete,
                conversations: 0
            }
        ));
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_json_and_truncated_body_close_connection() {
        for (length, body) in [(1_u32, b"{".as_slice()), (2_u32, b"{".as_slice())] {
            let (_dir, listener, path) = socket();
            let server = tokio::spawn(async move {
                serve(
                    listener.accept().await.unwrap(),
                    &FakeHandler(
                        Arc::new(AtomicUsize::new(0)),
                        Arc::new(AtomicUsize::new(0)),
                        Arc::new(AtomicUsize::new(0)),
                    ),
                )
                .await;
            });
            let mut client = UnixStream::connect(path).await.unwrap();
            client
                .write_all(&[bloom_ipc::PROTOCOL_VERSION as u8])
                .await
                .unwrap();
            client.write_all(&length.to_be_bytes()).await.unwrap();
            client.write_all(body).await.unwrap();
            client.shutdown().await.unwrap();
            let mut byte = [0];
            match client.read(&mut byte).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("expected closed connection, got {other:?}"),
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn oversized_response_becomes_internal_error_on_the_same_stream() {
        use bloom_domain::{DeliveryState, Handle, HandleKind, MessagePart};

        let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
        let transport = TransportId::new("fake").unwrap();
        let conversation_id = ConversationId::new(transport.clone(), "conversation").unwrap();
        let messages = (0..3)
            .map(|index| {
                Message::new(
                    MessageId::new(transport.clone(), format!("message-{index}")).unwrap(),
                    conversation_id.clone(),
                    None,
                    Handle::new(HandleKind::Opaque, "synthetic-sender").unwrap(),
                    vec![MessagePart::Text("x".repeat(400_000))],
                    1,
                    None,
                    DeliveryState::Sent,
                    false,
                    None,
                    vec![],
                    None,
                )
                .unwrap()
            })
            .collect();
        let oversized = Response {
            request_id: 9,
            response: ResponseKind::MessageContext { messages },
        };
        let normal = Response {
            request_id: 10,
            response: ResponseKind::Conversations {
                conversations: vec![],
            },
        };
        let writer_task = tokio::spawn(async move {
            let first =
                write_response_bounded(&mut writer, &oversized, Duration::from_secs(5)).await;
            let second = write_response_bounded(&mut writer, &normal, Duration::from_secs(5)).await;
            (first, second)
        });

        let fallback = read_response(&mut reader).await.unwrap();
        assert_eq!(fallback.request_id, 9);
        assert!(matches!(
            fallback.response,
            ResponseKind::Error {
                error: ProtocolError::Internal
            }
        ));
        let following = read_response(&mut reader).await.unwrap();
        assert_eq!(following.request_id, 10);
        assert!(matches!(
            following.response,
            ResponseKind::Conversations { .. }
        ));
        assert_eq!(writer_task.await.unwrap(), (Ok(()), Ok(())));
    }

    #[tokio::test]
    async fn slow_reader_cannot_hold_write_forever() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let response = Response {
            request_id: 1,
            response: ResponseKind::Conversations {
                conversations: vec![],
            },
        };
        assert!(
            write_response_bounded(&mut writer, &response, Duration::from_millis(20))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dedicated_subscribe_receives_events_and_late_subscribe_is_rejected() {
        let (_dir, listener, path) = socket();
        let hub = SubscriptionHub::new(4);
        let server_hub = hub.clone();
        let server = tokio::spawn(async move {
            serve_with_subscriptions(
                listener.accept().await.unwrap(),
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                ),
                &server_hub,
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 42,
                request: RequestKind::Subscribe,
            },
        )
        .await
        .unwrap();
        let ack = read_response(&mut client).await.unwrap();
        assert_eq!(ack.request_id, 42);
        assert!(matches!(ack.response, ResponseKind::Subscribed));
        let expected = Event {
            event: EventKind::Session {
                state: bloom_domain::SessionState::Active,
            },
        };
        hub.publish(expected.clone());
        assert!(read_event(&mut client).await.unwrap() == expected);
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();

        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            serve_with_subscriptions(
                listener.accept().await.unwrap(),
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                ),
                &SubscriptionHub::new(4),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 1,
                request: RequestKind::ListConversations,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_response(&mut client).await.unwrap().response,
            ResponseKind::Conversations { .. }
        ));
        write_request(
            &mut client,
            &Request {
                request_id: 2,
                request: RequestKind::Subscribe,
            },
        )
        .await
        .unwrap();
        let rejected = read_response(&mut client).await.unwrap();
        assert_eq!(rejected.request_id, 2);
        assert!(matches!(
            rejected.response,
            ResponseKind::Error {
                error: ProtocolError::InvalidRequest
            }
        ));
        drop(client);
        server.await.unwrap();
    }

    fn socket() -> (
        tempfile::TempDir,
        super::super::Listener,
        std::path::PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = dir.path().join("bloomd.sock");
        let listener = super::super::Listener::bind(&path).unwrap();
        (dir, listener, path)
    }

    #[tokio::test]
    async fn serves_requests_and_echoes_ids_without_invalid_history_call() {
        let (_dir, listener, path) = socket();
        let calls = Arc::new(AtomicUsize::new(0));
        let paged = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let server_paged = paged.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve(
                stream,
                &FakeHandler(server_calls, server_paged, Arc::new(AtomicUsize::new(0))),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        let conversation_id = ConversationId::new(TransportId::new("fake").unwrap(), "c").unwrap();
        for (id, request) in [
            (9, RequestKind::ListConversations),
            (
                10,
                RequestKind::History {
                    conversation_id: conversation_id.clone(),
                    limit: 0,
                    before: None,
                },
            ),
            (11, RequestKind::Subscribe),
            (
                12,
                RequestKind::Send {
                    conversation_id: conversation_id.clone(),
                    body: OutgoingBody { parts: vec![] },
                },
            ),
        ] {
            write_request(
                &mut client,
                &Request {
                    request_id: id,
                    request,
                },
            )
            .await
            .unwrap();
            let response = read_response(&mut client).await.unwrap();
            assert_eq!(response.request_id, id);
            match id {
                9 => assert!(matches!(
                    response.response,
                    ResponseKind::Conversations { .. }
                )),
                10 => assert!(matches!(
                    response.response,
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest
                    }
                )),
                11 => assert!(matches!(
                    response.response,
                    ResponseKind::Error {
                        error: ProtocolError::Unsupported
                    }
                )),
                12 => assert!(matches!(response.response, ResponseKind::Send { .. })),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "invalid limits must be rejected before the handler runs"
        );

        // A cursored read must reach the paging path, not the newest-page one.
        write_request(
            &mut client,
            &Request {
                request_id: 13,
                request: RequestKind::History {
                    conversation_id: conversation_id.clone(),
                    limit: 5,
                    before: Some(HistoryCursor {
                        created_at: 10,
                        id: MessageId::new(TransportId::new("fake").unwrap(), "cursor").unwrap(),
                    }),
                },
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert_eq!(response.request_id, 13);
        assert!(matches!(response.response, ResponseKind::History { .. }));

        // And a cursor-free read must still reach the newest-page handler.
        write_request(
            &mut client,
            &Request {
                request_id: 14,
                request: RequestKind::History {
                    conversation_id,
                    limit: 5,
                    before: None,
                },
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert_eq!(response.request_id, 14);
        assert!(matches!(response.response, ResponseKind::History { .. }));
        client.shutdown().await.unwrap();
        server.await.unwrap();
        assert_eq!(
            paged.load(Ordering::SeqCst),
            1,
            "the cursor is forwarded to the paging handler"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a cursor-free read stays on the newest-page handler"
        );
    }

    #[tokio::test]
    async fn search_reaches_handler_only_for_valid_limits() {
        let (_dir, listener, path) = socket();
        let searches = Arc::new(AtomicUsize::new(0));
        let server_searches = searches.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve(
                stream,
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    server_searches,
                ),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        for (request_id, limit, rejected) in [(21, 5, false), (22, 0, true), (23, 65_536, true)] {
            write_request(
                &mut client,
                &Request {
                    request_id,
                    request: RequestKind::SearchMessages {
                        query: "needle".into(),
                        limit,
                        participant: None,
                    },
                },
            )
            .await
            .unwrap();
            let response = read_response(&mut client).await.unwrap();
            assert_eq!(response.request_id, request_id);
            if rejected {
                assert!(matches!(
                    response.response,
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest
                    }
                ));
            } else {
                match response.response {
                    ResponseKind::SearchResults { hits } => {
                        assert_eq!(hits.len(), 1, "the handler's hit reaches the client");
                        assert_eq!(hits[0].snippet, "a hit snippet");
                        assert_eq!(hits[0].created_at, 7);
                    }
                    _ => panic!("expected search results"),
                }
            }
        }
        drop(client);
        server.await.unwrap();
        assert_eq!(
            searches.load(Ordering::SeqCst),
            1,
            "invalid limits are rejected before the search handler runs"
        );
    }

    #[tokio::test]
    async fn unsupported_participant_filter_is_not_silently_ignored() {
        let (_dir, listener, path) = socket();
        let searches = Arc::new(AtomicUsize::new(0));
        let server_searches = searches.clone();
        let server = tokio::spawn(async move {
            serve(
                listener.accept().await.unwrap(),
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    server_searches,
                ),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        write_request(
            &mut client,
            &Request {
                request_id: 24,
                request: RequestKind::SearchMessages {
                    query: "needle".into(),
                    limit: 5,
                    participant: Some(bloom_ipc::ParticipantFilter {
                        transport: TransportId::new("fake").unwrap(),
                        handle: bloom_domain::Handle::new(
                            bloom_domain::HandleKind::Opaque,
                            "friend",
                        )
                        .unwrap(),
                    }),
                },
            },
        )
        .await
        .unwrap();
        let response = read_response(&mut client).await.unwrap();
        assert!(matches!(
            response.response,
            ResponseKind::Error {
                error: ProtocolError::Unsupported
            }
        ));
        drop(client);
        server.await.unwrap();
        assert_eq!(
            searches.load(Ordering::SeqCst),
            0,
            "a present filter must not fall back to the unfiltered handler"
        );
    }

    #[tokio::test]
    async fn message_context_limits_are_validated_before_the_handler() {
        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            serve(
                listener.accept().await.unwrap(),
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                ),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        let conversation_id = ConversationId::new(TransportId::new("fake").unwrap(), "c").unwrap();
        let message_id = MessageId::new(TransportId::new("fake").unwrap(), "target").unwrap();
        for (id, limit, invalid) in [
            (1, 0, true),
            (2, 201, true),
            (3, u32::MAX, true),
            (4, 1, false),
            (5, 200, false),
        ] {
            write_request(
                &mut client,
                &Request {
                    request_id: id,
                    request: RequestKind::MessageContext {
                        conversation_id: conversation_id.clone(),
                        message_id: message_id.clone(),
                        limit,
                    },
                },
            )
            .await
            .unwrap();
            let response = read_response(&mut client).await.unwrap();
            assert_eq!(response.request_id, id);
            if invalid {
                assert!(matches!(
                    response.response,
                    ResponseKind::Error {
                        error: ProtocolError::InvalidRequest
                    }
                ));
            } else {
                assert!(matches!(
                    response.response,
                    ResponseKind::Error {
                        error: ProtocolError::Unsupported
                    }
                ));
            }
        }
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_frame_and_idle_first_request_close_connection() {
        for version in [6, 99] {
            let (_dir, listener, path) = socket();
            let server = tokio::spawn(async move {
                serve(
                    listener.accept().await.unwrap(),
                    &FakeHandler(
                        Arc::new(AtomicUsize::new(0)),
                        Arc::new(AtomicUsize::new(0)),
                        Arc::new(AtomicUsize::new(0)),
                    ),
                )
                .await;
            });
            let mut client = UnixStream::connect(path).await.unwrap();
            client.write_all(&[version, 0, 0, 0, 0]).await.unwrap();
            let mut byte = [0];
            // Linux may reset when closing a socket with unread malformed frame bytes.
            match client.read(&mut byte).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("expected closed connection, got {other:?}"),
            }
            server.await.unwrap();
        }

        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve_with_first_timeout(
                stream,
                &FakeHandler(
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                    Arc::new(AtomicUsize::new(0)),
                ),
                Duration::from_millis(20),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        server.await.unwrap();
    }
}
