//! Request/response handling on an authenticated local socket.
use std::{future::Future, pin::Pin, time::Duration};

use bloom_domain::{Conversation, ConversationId, Message, MessageId, OutgoingBody};
use bloom_ipc::{HistoryCursor, ProtocolError, RequestKind, Response, ResponseKind};
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
    fn send(
        &self,
        conversation_id: ConversationId,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId>;
    fn start_conversation(
        &self,
        address: String,
        body: OutgoingBody,
    ) -> HandlerFuture<'_, MessageId>;
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
                let receiver = hub.subscribe();
                let initial = hub.latest_session();
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
            RequestKind::StartConversation { address, body } => {
                match handler.start_conversation(address, body).await {
                    Ok(message_id) => ResponseKind::Send { message_id },
                    Err(error) => ResponseKind::Error { error },
                }
            }
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
    timeout(deadline, bloom_ipc::write_response(writer, response))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
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
    struct FakeHandler(Arc<AtomicUsize>, Arc<AtomicUsize>);
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
        fn send(&self, _: ConversationId, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
        fn start_conversation(&self, _: String, _: OutgoingBody) -> HandlerFuture<'_, MessageId> {
            Box::pin(async { Err(ProtocolError::Internal) })
        }
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
                &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
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
                    &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
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
                &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
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
                &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
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
            serve(stream, &FakeHandler(server_calls, server_paged)).await;
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
    async fn malformed_frame_and_idle_first_request_close_connection() {
        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve(
                stream,
                &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
        client.write_all(&[99, 0, 0, 0, 0]).await.unwrap();
        let mut byte = [0];
        // Linux may reset when closing a socket with unread malformed frame bytes.
        match client.read(&mut byte).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("expected closed connection, got {other:?}"),
        }
        server.await.unwrap();

        let (_dir, listener, path) = socket();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            serve_with_first_timeout(
                stream,
                &FakeHandler(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
                Duration::from_millis(20),
            )
            .await;
        });
        let mut client = UnixStream::connect(path).await.unwrap();
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
