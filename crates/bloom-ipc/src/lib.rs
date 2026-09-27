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
    Conversation, ConversationId, DeliveryState, Message, MessageId, OutgoingBody, SessionState,
};
use serde::{Deserialize, Serialize};
use std::{fmt, io};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u16 = 1;
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestKind {
    ListConversations,
    Subscribe,
    History {
        conversation_id: ConversationId,
        limit: u32,
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
    Conversations { conversations: Vec<Conversation> },
    History { messages: Vec<Message> },
    Send { message_id: MessageId },
    Error { error: ProtocolError },
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
    Message {
        conversation_id: ConversationId,
        message: Message,
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
