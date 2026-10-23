//! Versioned JSON framing and message types for Bloom's Unix-socket IPC.
//!
//! # Wire framing
//! Every frame is one version byte, a four-byte unsigned big-endian length, then
//! exactly that many UTF-8 JSON bytes. Lengths must be nonzero and no greater
//! than [`MAX_FRAME_LEN`]. The limit is checked before allocating payload memory.
//! A stream may contain consecutive frames. Every [`CodecError`] is fatal to the
//! current connection: callers MUST close it and MUST NOT attempt resynchronization.
//! Subscription uses a dedicated socket: server sends `Subscribed` once, then
//! only [`Event`] frames.

pub mod client;

use bloom_domain::{
    Conversation, ConversationId, DeliveryState, Handle, Message, MessageId, OutgoingBody,
    SessionState, TransportId,
};
use serde::{Deserialize, Serialize};
use std::{fmt, io};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped to 6 because local search now covers both the stored message body
/// and the reported filename. Searches never read file contents from disk.
pub const PROTOCOL_VERSION: u16 = 6;
/// Largest accepted `message_context` limit. Dispatch rejects larger limits
/// instead of clamping them.
pub const MAX_MESSAGE_CONTEXT_LIMIT: u32 = 200;
/// Default daemon socket, shared by the daemon and its clients so both sides
/// agree without configuration.
pub fn default_socket_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            #[cfg(unix)]
            {
                Some(std::path::PathBuf::from(format!("/run/user/{}", unsafe {
                    libc::geteuid()
                })))
            }
            #[cfg(not(unix))]
            {
                None
            }
        })
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("bloom/bloomd.sock")
}

pub const MAX_FRAME_LEN: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub request_id: u64,
    pub request: RequestKind,
}
/// Keyset position for history pagination. Messages older than this cursor are
/// returned under the store ordering `(created_at DESC, id DESC)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub created_at: i64,
    pub id: MessageId,
}
/// One message matched by a local content search. The snippet is a short window
/// around the matched run, with ellipses when it was truncated. Snippets of the
/// stored body are plain presentation text; filename excerpts are escaped and
/// are not a raw match-offset map into either the body or the filename.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSearchHit {
    pub conversation_id: ConversationId,
    pub message_id: MessageId,
    pub created_at: i64,
    pub snippet: String,
}
/// Exact conversation-membership scope for a local content search. The handle is
/// not transport-qualified on its own, so transport is part of the identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantFilter {
    pub transport: TransportId,
    pub handle: Handle,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestKind {
    ListConversations,
    SyncConversations,
    Subscribe,
    History {
        conversation_id: ConversationId,
        limit: u32,
        /// Absent from a version 2 request that wants the newest page; a version 1
        /// peer is rejected when the frame version byte is read, so it never sees
        /// this field.
        #[serde(default)]
        before: Option<HistoryCursor>,
    },
    Send {
        conversation_id: ConversationId,
        body: OutgoingBody,
    },
    /// Start (or reuse) a conversation with a bare address and send into it. On a
    /// fresh store this is how the first message gets sent, because no conversation
    /// exists yet.
    StartConversation {
        address: String,
        body: OutgoingBody,
    },
    /// Local content search over stored message text and reported filenames.
    /// `limit == 0` and limits above `u16::MAX` are invalid.
    SearchMessages {
        query: String,
        limit: u32,
        /// Scope matches to conversations whose current stored participants
        /// include this exact transport and handle.
        #[serde(default)]
        participant: Option<ParticipantFilter>,
    },
    /// Read a bounded window centred on one exact stored message. The target is
    /// always included, even when it lies far outside the newest history page.
    MessageContext {
        conversation_id: ConversationId,
        message_id: MessageId,
        limit: u32,
    },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub request_id: u64,
    pub response: ResponseKind,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ResponseKind {
    Subscribed,
    Conversations {
        conversations: Vec<Conversation>,
    },
    ConversationSync {
        status: ConversationSyncStatus,
        conversations: u32,
    },
    History {
        messages: Vec<Message>,
    },
    Send {
        message_id: MessageId,
    },
    SearchResults {
        hits: Vec<MessageSearchHit>,
    },
    MessageContext {
        messages: Vec<Message>,
    },
    Error {
        error: ProtocolError,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSyncStatus {
    Complete,
    Limited,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolError {
    InvalidRequest,
    NotFound,
    SessionExpired,
    AccountThrottled,
    RecipientUnreachable,
    NotRegistered,
    TransportUnavailable,
    Unsupported,
    Protocol,
    Internal,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub event: EventKind,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    ConversationsChanged,
    Message {
        conversation_id: ConversationId,
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        push_sequence: Option<u64>,
    },
    Update {
        conversation_id: ConversationId,
        message: Message,
    },
    Delivery {
        conversation_id: ConversationId,
        message_id: MessageId,
        state: DeliveryState,
    },
    Session {
        state: SessionState,
    },
}

// Deliberately avoid derived Debug on envelopes and payload enums: they may carry body/contact data.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame<T>(pub T);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    Io,
    Truncated,
    Empty,
    TooLarge,
    InvalidJson,
    WrongVersion,
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Io => "IPC frame I/O error",
            Self::Truncated => "truncated IPC frame",
            Self::Empty => "empty IPC frame",
            Self::TooLarge => "IPC frame exceeds size limit",
            Self::InvalidJson => "invalid IPC JSON frame",
            Self::WrongVersion => "unsupported IPC protocol version",
        })
    }
}
impl std::error::Error for CodecError {}

pub async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Request, CodecError> {
    read_frame(reader).await
}
pub async fn read_response<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Response, CodecError> {
    read_frame(reader).await
}
pub async fn read_event<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Event, CodecError> {
    read_frame(reader).await
}
pub async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Request,
) -> Result<(), CodecError> {
    write_frame(writer, value).await
}
pub async fn write_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Response,
) -> Result<(), CodecError> {
    write_frame(writer, value).await
}
pub async fn write_event<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Event,
) -> Result<(), CodecError> {
    write_frame(writer, value).await
}

#[derive(Default)]
struct CappedBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}
impl std::io::Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_FRAME_LEN {
            self.exceeded = true;
            return Err(io::Error::other("frame size exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> Result<T, CodecError> {
    let mut version = [0; 1];
    read_exact_redacted(r, &mut version).await?;
    if version[0] != PROTOCOL_VERSION as u8 {
        return Err(CodecError::WrongVersion);
    }
    let mut header = [0; 4];
    read_exact_redacted(r, &mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Err(CodecError::Empty);
    }
    if len > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge);
    }
    let mut payload = vec![0; len];
    read_exact_redacted(r, &mut payload).await?;
    let value: T = serde_json::from_slice(&payload).map_err(|_| CodecError::InvalidJson)?;
    Ok(value)
}
async fn read_exact_redacted<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut [u8],
) -> Result<(), CodecError> {
    match r.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(CodecError::Truncated),
        Err(_) => Err(CodecError::Io),
    }
}
async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    value: &T,
) -> Result<(), CodecError> {
    let mut payload = CappedBuffer::default();
    if serde_json::to_writer(&mut payload, value).is_err() {
        return Err(if payload.exceeded {
            CodecError::TooLarge
        } else {
            CodecError::InvalidJson
        });
    }
    if payload.bytes.is_empty() {
        return Err(CodecError::Empty);
    }
    w.write_all(&[PROTOCOL_VERSION as u8])
        .await
        .map_err(|_| CodecError::Io)?;
    w.write_all(&(payload.bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|_| CodecError::Io)?;
    w.write_all(&payload.bytes)
        .await
        .map_err(|_| CodecError::Io)?;
    w.flush().await.map_err(|_| CodecError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;
    fn req() -> Request {
        Request {
            request_id: 7,
            request: RequestKind::ListConversations,
        }
    }
    #[test]
    fn message_push_sequence_is_optional_and_omitted_when_absent() {
        let transport = bloom_domain::TransportId::new("test").unwrap();
        let conversation_id = ConversationId::new(transport.clone(), "conversation").unwrap();
        let message = Message::new(
            MessageId::new(transport, "message").unwrap(),
            conversation_id.clone(),
            None,
            bloom_domain::Handle::new(bloom_domain::HandleKind::Opaque, "sender").unwrap(),
            vec![bloom_domain::MessagePart::Text("body".into())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        let encoded = serde_json::to_value(EventKind::Message {
            conversation_id: conversation_id.clone(),
            message: message.clone(),
            push_sequence: Some(17),
        })
        .unwrap();
        assert_eq!(encoded["push_sequence"], 17);

        let mut legacy = encoded;
        legacy.as_object_mut().unwrap().remove("push_sequence");
        match serde_json::from_value::<EventKind>(legacy).unwrap() {
            EventKind::Message { push_sequence, .. } => assert_eq!(push_sequence, None),
            _ => panic!("expected a message event"),
        }

        let without_sequence = serde_json::to_value(EventKind::Message {
            conversation_id,
            message,
            push_sequence: None,
        })
        .unwrap();
        assert!(without_sequence.get("push_sequence").is_none());
    }
    #[tokio::test]
    async fn history_before_cursor_round_trips_with_and_without_cursor() {
        let transport = bloom_domain::TransportId::new("test").unwrap();
        let conversation_id = ConversationId::new(transport.clone(), "conversation").unwrap();
        for before in [
            None,
            Some(HistoryCursor {
                created_at: 1_700_000_000_000,
                id: MessageId::new(transport.clone(), "message").unwrap(),
            }),
        ] {
            let expected = Request {
                request_id: 9,
                request: RequestKind::History {
                    conversation_id: conversation_id.clone(),
                    limit: 50,
                    before: before.clone(),
                },
            };
            let bytes = serde_json::to_vec(&expected).unwrap();
            let decoded: Request = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(decoded.request_id, expected.request_id);
            assert!(decoded.request == expected.request);
        }

        let without_field = serde_json::json!({
            "request_id": 9,
            "request": {
                "op": "history",
                "conversation_id": conversation_id,
                "limit": 50,
            },
        });
        match serde_json::from_value::<Request>(without_field)
            .unwrap()
            .request
        {
            RequestKind::History { before, .. } => assert_eq!(before, None),
            _ => panic!("expected a history request"),
        }
    }

    #[test]
    fn search_request_and_results_round_trip() {
        let transport = bloom_domain::TransportId::new("test").unwrap();
        let request = Request {
            request_id: 11,
            request: RequestKind::SearchMessages {
                query: "coast".into(),
                limit: 25,
                participant: Some(ParticipantFilter {
                    transport: transport.clone(),
                    handle: Handle::new(bloom_domain::HandleKind::Opaque, "friend").unwrap(),
                }),
            },
        };
        let bytes = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded.request == request.request);

        let hit = MessageSearchHit {
            conversation_id: ConversationId::new(transport.clone(), "conversation").unwrap(),
            message_id: MessageId::new(transport, "message").unwrap(),
            created_at: 1_700_000_000_000,
            snippet: "…early train to the coast…".into(),
        };
        let response = Response {
            request_id: 12,
            response: ResponseKind::SearchResults {
                hits: vec![hit.clone()],
            },
        };
        let bytes = serde_json::to_vec(&response).unwrap();
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        match decoded.response {
            ResponseKind::SearchResults { hits } => assert_eq!(hits, vec![hit]),
            _ => panic!("expected search results"),
        }
    }

    #[test]
    fn message_context_request_and_response_round_trip() {
        let transport = bloom_domain::TransportId::new("test").unwrap();
        let conversation_id = ConversationId::new(transport.clone(), "conversation").unwrap();
        let message_id = MessageId::new(transport.clone(), "target").unwrap();
        let request = Request {
            request_id: 13,
            request: RequestKind::MessageContext {
                conversation_id: conversation_id.clone(),
                message_id: message_id.clone(),
                limit: MAX_MESSAGE_CONTEXT_LIMIT,
            },
        };
        let decoded: Request =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert!(decoded.request == request.request);

        let message = Message::new(
            message_id.clone(),
            conversation_id,
            None,
            bloom_domain::Handle::new(bloom_domain::HandleKind::Opaque, "sender").unwrap(),
            vec![bloom_domain::MessagePart::Text("body".into())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        let response = Response {
            request_id: 13,
            response: ResponseKind::MessageContext {
                messages: vec![message.clone()],
            },
        };
        let decoded: Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        match decoded.response {
            ResponseKind::MessageContext { messages } => assert_eq!(messages, vec![message]),
            _ => panic!("expected message context"),
        }
        assert_eq!(PROTOCOL_VERSION, 6);
    }

    #[tokio::test]
    async fn search_participant_defaults_to_none_and_rejects_old_versions() {
        let without_field = serde_json::json!({
            "request_id": 14,
            "request": {
                "op": "search_messages",
                "query": "coast",
                "limit": 25,
            },
        });
        match serde_json::from_value::<Request>(without_field.clone())
            .unwrap()
            .request
        {
            RequestKind::SearchMessages { participant, .. } => assert_eq!(participant, None),
            _ => panic!("expected a search request"),
        }

        let mut explicit_null = without_field;
        explicit_null["request"]["participant"] = serde_json::Value::Null;
        assert!(matches!(
            serde_json::from_value::<Request>(explicit_null)
                .unwrap()
                .request,
            RequestKind::SearchMessages {
                participant: None,
                ..
            }
        ));

        let old_frame = [4_u8, 0, 0, 0, 2, b'{', b'}'];
        let mut input = old_frame.as_slice();
        assert_eq!(
            read_request(&mut input).await.err().unwrap(),
            CodecError::WrongVersion
        );
    }

    #[tokio::test]
    async fn rejects_v5_request_and_response_before_header_or_payload() {
        let old = [5u8];
        let mut request_input = old.as_slice();
        assert_eq!(
            read_request(&mut request_input).await.err().unwrap(),
            CodecError::WrongVersion
        );
        let mut response_input = old.as_slice();
        assert_eq!(
            read_response(&mut response_input).await.err().unwrap(),
            CodecError::WrongVersion
        );
    }

    #[tokio::test]
    async fn round_trip_and_fragmented_frame() {
        let (mut a, mut b) = duplex(2);
        let value = req();
        let writer = tokio::spawn(async move {
            write_request(&mut a, &value).await.unwrap();
        });
        let decoded = read_request(&mut b).await.unwrap();
        assert_eq!(decoded.request_id, req().request_id);
        assert!(matches!(decoded.request, RequestKind::ListConversations));
        writer.await.unwrap();
    }
    #[tokio::test]
    async fn reject_oversize_before_body_read() {
        let mut bytes = vec![PROTOCOL_VERSION as u8];
        bytes.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());
        bytes.extend_from_slice(b"SECRET");
        let mut input = bytes.as_slice();
        assert_eq!(
            read_request(&mut input).await.err().unwrap(),
            CodecError::TooLarge
        );
    }
    #[tokio::test]
    async fn rejects_wrong_version_and_malformed_without_leak() {
        let wrong = br#"garbage"#;
        let wrong_frame = [&[99][..], &(u32::MAX).to_be_bytes()[..], wrong].concat();
        let mut input = wrong_frame.as_slice();
        assert_eq!(
            read_request(&mut input).await.err().unwrap(),
            CodecError::WrongVersion
        );
        let secret = br#"{"SECRET_BODY":!}"#;
        let secret_frame = [
            &[PROTOCOL_VERSION as u8][..],
            &(secret.len() as u32).to_be_bytes()[..],
            secret,
        ]
        .concat();
        let mut input = secret_frame.as_slice();
        let error = read_request(&mut input).await.err().unwrap();
        assert_eq!(error, CodecError::InvalidJson);
        assert!(!error.to_string().contains("SECRET_BODY"));
    }
    #[tokio::test]
    async fn response_request_ids_correlate_out_of_order() {
        let first = Response {
            request_id: 2,
            response: ResponseKind::Subscribed,
        };
        let second = Response {
            request_id: 1,
            response: ResponseKind::Subscribed,
        };
        let (mut tx, mut rx) = duplex(128);
        write_response(&mut tx, &first).await.unwrap();
        write_response(&mut tx, &second).await.unwrap();
        assert_eq!(read_response(&mut rx).await.unwrap().request_id, 2);
        assert_eq!(read_response(&mut rx).await.unwrap().request_id, 1);
    }
    #[tokio::test]
    async fn writer_caps_serialized_size() {
        let (mut tx, _rx) = duplex(1);
        let oversized = Request {
            request_id: 1,
            request: RequestKind::Send {
                conversation_id: ConversationId::new(
                    bloom_domain::TransportId::new("direct").unwrap(),
                    "c",
                )
                .unwrap(),
                body: OutgoingBody {
                    parts: vec![bloom_domain::MessagePart::Text("x".repeat(MAX_FRAME_LEN))],
                },
            },
        };
        assert_eq!(
            write_request(&mut tx, &oversized).await.err().unwrap(),
            CodecError::TooLarge
        );
    }
    #[tokio::test]
    async fn detects_truncated() {
        let mut input = [PROTOCOL_VERSION as u8, 0, 0, 0, 20, b'{'].as_slice();
        assert_eq!(
            read_request(&mut input).await.err().unwrap(),
            CodecError::Truncated
        );
    }
}
