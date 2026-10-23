use crate::{
    read_event, read_response, write_request, CodecError, ConversationSyncStatus, Event,
    HistoryCursor, MessageSearchHit, Request, RequestKind, ResponseKind,
};
use bloom_domain::{Conversation, ConversationId, Message, MessageId, OutgoingBody};
use std::{
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::net::UnixStream;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct BloomClient {
    socket_path: PathBuf,
    request_timeout: Duration,
    handshake_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    Connect,
    Io,
    Timeout,
    WrongPeer,
    Codec(CodecError),
    WrongRequestId,
    UnexpectedResponse,
    Server(crate::ProtocolError),
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "IPC connection failed",
            Self::Io => "IPC request failed",
            Self::Timeout => "IPC request timed out",
            Self::WrongPeer => "IPC peer rejected",
            Self::Codec(e) => return e.fmt(f),
            Self::WrongRequestId => "IPC response ID mismatch",
            Self::UnexpectedResponse => "unexpected IPC response",
            Self::Server(_) => "IPC server rejected request",
        })
    }
}
impl std::error::Error for ClientError {}

impl BloomClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            request_timeout: REQUEST_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeouts(mut self, request: Duration, handshake: Duration) -> Self {
        self.request_timeout = request;
        self.handshake_timeout = handshake;
        self
    }

    fn id() -> u64 {
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }

    async fn connect(&self) -> Result<UnixStream, ClientError> {
        let stream = tokio::net::UnixStream::connect(&self.socket_path)
            .await
            .map_err(|_| ClientError::Connect)?;
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
            return Err(ClientError::Connect);
        }
        check_peer_uid(credentials.uid, unsafe { libc::geteuid() })
            .map_err(|_| ClientError::WrongPeer)?;
        Ok(stream)
    }

    async fn request(&self, kind: RequestKind) -> Result<ResponseKind, ClientError> {
        self.request_with_timeout(kind, self.request_timeout).await
    }

    async fn request_with_timeout(
        &self,
        kind: RequestKind,
        timeout: Duration,
    ) -> Result<ResponseKind, ClientError> {
        let id = Self::id();
        let mut stream = self.connect().await?;
        tokio::time::timeout(timeout, async {
            write_request(
                &mut stream,
                &Request {
                    request_id: id,
                    request: kind,
                },
            )
            .await
            .map_err(ClientError::Codec)?;
            let response = read_response(&mut stream)
                .await
                .map_err(ClientError::Codec)?;
            if response.request_id != id {
                return Err(ClientError::WrongRequestId);
            }
            match response.response {
                ResponseKind::Error { error } => Err(ClientError::Server(error)),
                other => Ok(other),
            }
        })
        .await
        .map_err(|_| ClientError::Timeout)?
    }

    pub async fn list_conversations(&self) -> Result<Vec<Conversation>, ClientError> {
        match self.request(RequestKind::ListConversations).await? {
            ResponseKind::Conversations { conversations } => Ok(conversations),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Force a conversation-list metadata sync. This does not fetch message history.
    pub async fn sync_conversations(&self) -> Result<(ConversationSyncStatus, u32), ClientError> {
        match self
            .request_with_timeout(RequestKind::SyncConversations, SYNC_REQUEST_TIMEOUT)
            .await?
        {
            ResponseKind::ConversationSync {
                status,
                conversations,
            } => Ok((status, conversations)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn history(
        &self,
        conversation_id: ConversationId,
        limit: u32,
    ) -> Result<Vec<Message>, ClientError> {
        match self
            .request(RequestKind::History {
                conversation_id,
                limit,
                before: None,
            })
            .await?
        {
            ResponseKind::History { messages } => Ok(messages),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Page history strictly older than `before`, oldest-first, as with [`Self::history`].
    pub async fn history_before(
        &self,
        conversation_id: ConversationId,
        limit: u32,
        before: HistoryCursor,
    ) -> Result<Vec<Message>, ClientError> {
        match self
            .request(RequestKind::History {
                conversation_id,
                limit,
                before: Some(before),
            })
            .await?
        {
            ResponseKind::History { messages } => Ok(messages),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Search stored message text locally. Results are newest first.
    pub async fn search_messages(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<MessageSearchHit>, ClientError> {
        match self
            .request(RequestKind::SearchMessages {
                query: query.to_owned(),
                limit,
            })
            .await?
        {
            ResponseKind::SearchResults { hits } => Ok(hits),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read a bounded window centred on `message_id`, oldest-first. The exact
    /// target is always present. The older side receives `(limit - 1) / 2` slots
    /// and the newer side receives the rest, so an even limit gives the newer side
    /// the extra slot.
    pub async fn message_context(
        &self,
        conversation_id: ConversationId,
        message_id: MessageId,
        limit: u32,
    ) -> Result<Vec<Message>, ClientError> {
        match self
            .request(RequestKind::MessageContext {
                conversation_id,
                message_id,
                limit,
            })
            .await?
        {
            ResponseKind::MessageContext { messages } => Ok(messages),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn send(
        &self,
        conversation_id: ConversationId,
        body: OutgoingBody,
    ) -> Result<MessageId, ClientError> {
        match self
            .request(RequestKind::Send {
                conversation_id,
                body,
            })
            .await?
        {
            ResponseKind::Send { message_id } => Ok(message_id),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Send to a bare address, creating the conversation if it does not exist yet.
    /// This is how the first message is sent into an empty store.
    pub async fn start_conversation(
        &self,
        address: String,
        body: OutgoingBody,
    ) -> Result<MessageId, ClientError> {
        match self
            .request(RequestKind::StartConversation { address, body })
            .await?
        {
            ResponseKind::Send { message_id } => Ok(message_id),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn subscribe(&self) -> Result<EventStream, ClientError> {
        let id = Self::id();
        let mut stream = self.connect().await?;
        tokio::time::timeout(self.handshake_timeout, async {
            write_request(
                &mut stream,
                &Request {
                    request_id: id,
                    request: RequestKind::Subscribe,
                },
            )
            .await
            .map_err(ClientError::Codec)?;
            let response = read_response(&mut stream)
                .await
                .map_err(ClientError::Codec)?;
            if response.request_id != id {
                return Err(ClientError::WrongRequestId);
            }
            match response.response {
                ResponseKind::Subscribed => Ok(EventStream { stream }),
                ResponseKind::Error { error } => Err(ClientError::Server(error)),
                _ => Err(ClientError::UnexpectedResponse),
            }
        })
        .await
        .map_err(|_| ClientError::Timeout)?
    }
}

fn check_peer_uid(peer_uid: u32, effective_uid: u32) -> Result<(), ()> {
    if peer_uid == effective_uid {
        Ok(())
    } else {
        Err(())
    }
}

pub struct EventStream {
    stream: UnixStream,
}
impl EventStream {
    pub async fn next(&mut self) -> Result<Event, ClientError> {
        read_event(&mut self.stream)
            .await
            .map_err(ClientError::Codec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{write_event, write_response, EventKind, Response};
    use bloom_domain::TransportId;
    use tokio::net::UnixListener;

    #[test]
    fn peer_uid_policy() {
        assert!(check_peer_uid(1000, 1000).is_ok());
        assert!(check_peer_uid(1001, 1000).is_err());
    }

    #[tokio::test]
    async fn history_before_puts_the_cursor_on_the_wire() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("request");
        let listener = UnixListener::bind(&p).unwrap();
        type Seen = std::sync::Arc<std::sync::Mutex<Vec<(u32, Option<HistoryCursor>)>>>;
        let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_seen = seen.clone();
        let server = tokio::spawn(async move {
            // Each client request opens its own connection.
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = crate::read_request(&mut stream).await.unwrap();
                let Some((_, limit, before)) = (match request.request {
                    RequestKind::History {
                        conversation_id,
                        limit,
                        before,
                    } => Some((conversation_id, limit, before)),
                    _ => None,
                }) else {
                    unreachable!("both calls are history reads");
                };
                server_seen.lock().unwrap().push((limit, before));
                crate::write_response(
                    &mut stream,
                    &crate::Response {
                        request_id: request.request_id,
                        response: ResponseKind::History { messages: vec![] },
                    },
                )
                .await
                .unwrap();
            }
        });

        let client = BloomClient::new(&p);
        let conversation = ConversationId::new(TransportId::new("fake").unwrap(), "c").unwrap();
        client
            .history(conversation.clone(), 5)
            .await
            .expect("newest page");
        let (plain_limit, plain_before) = seen.lock().unwrap()[0].clone();
        assert_eq!(plain_limit, 5);
        assert_eq!(plain_before, None, "a plain read carries no cursor");

        let cursor = HistoryCursor {
            created_at: 1_700_000_000_000,
            id: MessageId::new(TransportId::new("fake").unwrap(), "m").unwrap(),
        };
        client
            .history_before(conversation, 7, cursor.clone())
            .await
            .expect("older page");
        let (paged_limit, paged_before) = seen.lock().unwrap()[1].clone();
        assert_eq!(paged_limit, 7);
        assert_eq!(
            paged_before,
            Some(cursor),
            "the cursor reaches the daemon unchanged"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn message_context_puts_the_target_and_limit_on_the_wire() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("context");
        let listener = UnixListener::bind(&p).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = crate::read_request(&mut stream).await.unwrap();
            match request.request {
                RequestKind::MessageContext {
                    conversation_id,
                    message_id,
                    limit,
                } => {
                    assert_eq!(conversation_id.value(), "c");
                    assert_eq!(message_id.value(), "target");
                    assert_eq!(limit, 200);
                }
                _ => panic!("expected a message context request"),
            }
            crate::write_response(
                &mut stream,
                &crate::Response {
                    request_id: request.request_id,
                    response: ResponseKind::MessageContext { messages: vec![] },
                },
            )
            .await
            .unwrap();
        });
        let client = BloomClient::new(&p);
        let conversation = ConversationId::new(TransportId::new("fake").unwrap(), "c").unwrap();
        let target = MessageId::new(TransportId::new("fake").unwrap(), "target").unwrap();
        assert!(client
            .message_context(conversation, target, 200)
            .await
            .unwrap()
            .is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_servers_time_out() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("request");
        let listener = UnixListener::bind(&p).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = BloomClient::new(&p)
            .with_timeouts(Duration::from_millis(20), Duration::from_millis(20));
        assert_eq!(
            client.list_conversations().await.unwrap_err(),
            ClientError::Timeout
        );
        server.abort();

        let p = d.path().join("subscribe");
        let listener = UnixListener::bind(&p).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = BloomClient::new(&p)
            .with_timeouts(Duration::from_millis(20), Duration::from_millis(20));
        assert!(matches!(
            client.subscribe().await,
            Err(ClientError::Timeout)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn request_and_wrong_id() {
        for wrong in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("s");
            let l = UnixListener::bind(&p).unwrap();
            let task = tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let r = crate::read_request(&mut s).await.unwrap();
                write_response(
                    &mut s,
                    &Response {
                        request_id: if wrong {
                            r.request_id + 1
                        } else {
                            r.request_id
                        },
                        response: ResponseKind::Conversations {
                            conversations: vec![],
                        },
                    },
                )
                .await
                .unwrap();
            });
            let result = BloomClient::new(p).list_conversations().await;
            if wrong {
                assert_eq!(result.unwrap_err(), ClientError::WrongRequestId);
            } else {
                assert!(result.unwrap().is_empty());
            }
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sync_request_returns_server_status_and_count() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sync");
        let listener = UnixListener::bind(&p).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = crate::read_request(&mut stream).await.unwrap();
            assert!(matches!(request.request, RequestKind::SyncConversations));
            write_response(
                &mut stream,
                &Response {
                    request_id: request.request_id,
                    response: ResponseKind::ConversationSync {
                        status: ConversationSyncStatus::Limited,
                        conversations: 42,
                    },
                },
            )
            .await
            .unwrap();
        });

        assert_eq!(
            BloomClient::new(p).sync_conversations().await.unwrap(),
            (ConversationSyncStatus::Limited, 42)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscription_event() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let l = UnixListener::bind(&p).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let r = crate::read_request(&mut s).await.unwrap();
            write_response(
                &mut s,
                &Response {
                    request_id: r.request_id,
                    response: ResponseKind::Subscribed,
                },
            )
            .await
            .unwrap();
            let e = Event {
                event: EventKind::Session {
                    state: bloom_domain::SessionState::Disconnected,
                },
            };
            write_event(&mut s, &e).await.unwrap();
        });
        let mut events = BloomClient::new(p).subscribe().await.unwrap();
        assert!(matches!(
            events.next().await.unwrap().event,
            EventKind::Session {
                state: bloom_domain::SessionState::Disconnected
            }
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn truncated_frame() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        let l = UnixListener::bind(&p).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let _ = crate::read_request(&mut s).await;
            use tokio::io::AsyncWriteExt;
            s.write_all(&[crate::PROTOCOL_VERSION as u8, 0, 0])
                .await
                .unwrap();
        });
        let result = BloomClient::new(p).list_conversations().await;
        assert!(matches!(
            result,
            Err(ClientError::Codec(CodecError::Truncated))
        ));
        task.await.unwrap();
    }
}
