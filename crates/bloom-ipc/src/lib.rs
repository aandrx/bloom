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
use unicode_segmentation::UnicodeSegmentation;

/// Bumped to 8 for typed attachment fetch. Version 7 peers are rejected at
/// the frame byte, so they cannot silently ignore attachment requests.
pub const PROTOCOL_VERSION: u16 = 8;
/// Largest accepted `message_context` limit. Dispatch rejects larger limits
/// instead of clamping them.
pub const MAX_MESSAGE_CONTEXT_LIMIT: u32 = 200;
/// Largest accepted draft body, measured in UTF-8 bytes.
pub const MAX_DRAFT_BYTES: usize = 65_536;
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

/// Which environment setting supplied the resolved daemon socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketSource {
    /// `BLOOM_SOCKET_PATH` supplied the path.
    ExplicitPath,
    /// The legacy `BLOOM_SOCKET` alias supplied the path.
    LegacyAlias,
    /// Neither variable supplied a path, so [`default_socket_path`] applies.
    Default,
}

/// Resolve the daemon socket: `BLOOM_SOCKET_PATH`, then the legacy
/// `BLOOM_SOCKET` alias, then [`default_socket_path`].
pub fn socket_path_from_env() -> std::path::PathBuf {
    socket_path_from_env_with_source().0
}

/// Resolve the daemon socket and report which source supplied it, so callers
/// that print the path cannot disagree with the shared rule.
pub fn socket_path_from_env_with_source() -> (std::path::PathBuf, SocketSource) {
    socket_path_from(
        std::env::var_os("BLOOM_SOCKET_PATH").as_deref(),
        std::env::var_os("BLOOM_SOCKET").as_deref(),
        default_socket_path(),
    )
}

fn socket_path_from(
    explicit: Option<&std::ffi::OsStr>,
    alias: Option<&std::ffi::OsStr>,
    default: std::path::PathBuf,
) -> (std::path::PathBuf, SocketSource) {
    // Blank values mean "unset", which is how the daemon always treated them.
    fn usable(value: Option<&std::ffi::OsStr>) -> Option<&std::ffi::OsStr> {
        value.filter(|value| !value.to_string_lossy().trim().is_empty())
    }
    if let Some(value) = usable(explicit) {
        (std::path::PathBuf::from(value), SocketSource::ExplicitPath)
    } else if let Some(value) = usable(alias) {
        (std::path::PathBuf::from(value), SocketSource::LegacyAlias)
    } else {
        (default, SocketSource::Default)
    }
}

pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Which candidate supplied the resolved sidecar script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarSource {
    /// `BLOOM_ENGINE_SCRIPT` supplied the path.
    Environment,
    /// The installed prefix layout supplied the path.
    Prefix,
    /// The legacy sibling `bloom-engine/` directory supplied the path.
    LegacySibling,
    /// The compiled-in repository checkout supplied the path.
    Checkout,
}

impl SidecarSource {
    /// Short plain-text label for diagnostics, matching the daemon's source names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "BLOOM_ENGINE_SCRIPT",
            Self::Prefix => "installed prefix",
            Self::LegacySibling => "next to the daemon",
            Self::Checkout => "repository checkout",
        }
    }
}

/// Resolve the engine sidecar script for a daemon at `bloomd_exe`.
///
/// Order: a usable `BLOOM_ENGINE_SCRIPT` value, then the installed prefix
/// (`<bloomd_exe>/../share/bloom/engine/bin/bloom_engine.dart`), then the legacy
/// sibling (`<bloomd_exe>/bloom-engine/bloom_engine.dart`), then `checkout`.
/// Only the filesystem candidates must exist as files; an explicit environment
/// value always wins, including one that does not exist, so configuration errors
/// surface at engine start rather than silently falling back.
///
/// "Usable" follows the daemon rule: trimmed non-empty and valid UTF-8. Other
/// values fall through. The checkout path is a parameter so this crate never
/// reads the environment or depends on a compiled-in layout.
pub fn sidecar_script(
    bloomd_exe: &std::path::Path,
    env_value: Option<&std::ffi::OsStr>,
    checkout: &std::path::Path,
) -> Option<std::path::PathBuf> {
    sidecar_script_with_source(bloomd_exe, env_value, checkout).map(|(path, _)| path)
}

/// Like [`sidecar_script`], but also report which candidate won, so callers that
/// print a source label cannot disagree with the shared rule.
pub fn sidecar_script_with_source(
    bloomd_exe: &std::path::Path,
    env_value: Option<&std::ffi::OsStr>,
    checkout: &std::path::Path,
) -> Option<(std::path::PathBuf, SidecarSource)> {
    if let Some(value) = env_value.filter(|value| is_usable_script(value)) {
        return Some((std::path::PathBuf::from(value), SidecarSource::Environment));
    }
    if let Some(directory) = bloomd_exe
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        if let Some(prefix) = directory.parent() {
            let candidate = prefix.join("share/bloom/engine/bin/bloom_engine.dart");
            if candidate.is_file() {
                return Some((candidate, SidecarSource::Prefix));
            }
        }
        let candidate = directory.join("bloom-engine/bloom_engine.dart");
        if candidate.is_file() {
            return Some((candidate, SidecarSource::LegacySibling));
        }
    }
    checkout
        .is_file()
        .then(|| (checkout.to_path_buf(), SidecarSource::Checkout))
}

fn is_usable_script(value: &std::ffi::OsStr) -> bool {
    value.to_str().is_some_and(|text| !text.trim().is_empty())
}

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
/// Exact owner of a draft. This mirrors the daemon store type but is serialized
/// on the wire. The `owner` conversation is never normalized or aliased.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DraftKey {
    Conversation {
        owner: ConversationId,
    },
    NewChat {
        transport: TransportId,
        address: String,
    },
}
impl fmt::Debug for DraftKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversation { .. } => f.write_str("DraftKey::Conversation([REDACTED])"),
            Self::NewChat { .. } => f.write_str("DraftKey::NewChat([REDACTED])"),
        }
    }
}
impl DraftKey {
    /// Whether this key can be persisted. A blank new-chat address is session-only.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Conversation { .. } => true,
            Self::NewChat { transport, address } => {
                !transport.as_str().trim().is_empty() && !address.trim().is_empty()
            }
        }
    }
}
/// A draft body plus the composer state needed to restore it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableDraft {
    pub body: String,
    /// Grapheme index into `body`.
    pub cursor: usize,
    pub pasted_body: bool,
    pub uncertain: bool,
}
impl fmt::Debug for DurableDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableDraft")
            .field("body", &"[REDACTED]")
            .field("cursor", &self.cursor)
            .field("pasted_body", &self.pasted_body)
            .field("uncertain", &self.uncertain)
            .finish()
    }
}
impl DurableDraft {
    /// Whether the body size and grapheme cursor are acceptable to the store.
    pub fn is_valid(&self) -> bool {
        self.body.len() <= MAX_DRAFT_BYTES && self.cursor <= self.body.graphemes(true).count()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftWriteOutcome {
    Applied,
    Stale,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftLoad {
    pub draft: Option<DurableDraft>,
    pub revision_floor: u64,
    pub corrupt_deleted: bool,
}
/// Revisions travel as unsigned integers but the store stores a signed value.
pub fn draft_revision_is_valid(revision: u64) -> bool {
    revision <= i64::MAX as u64
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
    DraftSave {
        key: DraftKey,
        draft: DurableDraft,
        revision: u64,
    },
    DraftLoad {
        key: DraftKey,
    },
    DraftDelete {
        key: DraftKey,
        revision: u64,
    },
    /// Fetch one previously advertised opaque attachment token through the
    /// sidecar. The token is exactly 32 lowercase hex characters.
    FetchAttachment {
        token: String,
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
    DraftSave {
        outcome: DraftWriteOutcome,
    },
    DraftLoad {
        draft: Option<DurableDraft>,
        revision_floor: u64,
        corrupt_deleted: bool,
    },
    DraftDelete {
        outcome: DraftWriteOutcome,
    },
    AttachmentFetched {
        path: String,
        bytes: u64,
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
    use std::ffi::OsStr;
    use tokio::io::duplex;
    #[test]
    fn socket_explicit_path_wins_and_reports_explicit_source() {
        assert_eq!(
            socket_path_from(
                Some(OsStr::new("/explicit.sock")),
                Some(OsStr::new("/alias.sock")),
                default_socket_path()
            ),
            (
                std::path::PathBuf::from("/explicit.sock"),
                SocketSource::ExplicitPath
            )
        );
    }
    #[test]
    fn socket_alias_reports_legacy_alias_source() {
        assert_eq!(
            socket_path_from(None, Some(OsStr::new("/alias.sock")), default_socket_path()),
            (
                std::path::PathBuf::from("/alias.sock"),
                SocketSource::LegacyAlias
            )
        );
    }
    #[test]
    fn socket_default_reports_default_source_and_path() {
        assert_eq!(
            socket_path_from(None, None, default_socket_path()),
            (default_socket_path(), SocketSource::Default)
        );
    }
    #[test]
    fn socket_blank_explicit_falls_through_to_alias_source() {
        assert_eq!(
            socket_path_from(
                Some(OsStr::new("   ")),
                Some(OsStr::new("/alias.sock")),
                default_socket_path()
            ),
            (
                std::path::PathBuf::from("/alias.sock"),
                SocketSource::LegacyAlias
            )
        );
    }
    #[test]
    fn socket_blank_both_reports_default_source() {
        assert_eq!(
            socket_path_from(
                Some(OsStr::new("")),
                Some(OsStr::new("\t")),
                default_socket_path()
            ),
            (default_socket_path(), SocketSource::Default)
        );
    }
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
        assert_eq!(PROTOCOL_VERSION, 8);
    }

    #[test]
    fn draft_dtos_use_exact_keys_redacted_debug_and_typed_outcomes() {
        let transport = TransmissionForTests::transport();
        let conversation = ConversationId::new(transport.clone(), "exact-conversation").unwrap();
        let key = DraftKey::Conversation {
            owner: conversation.clone(),
        };
        let encoded = serde_json::to_value(&key).unwrap();
        assert_eq!(encoded["kind"], "conversation");
        assert_eq!(
            encoded["owner"],
            serde_json::to_value(&conversation).unwrap()
        );
        let chat = DraftKey::NewChat {
            transport,
            address: "Exact@Example.test".into(),
        };
        let encoded = serde_json::to_value(&chat).unwrap();
        assert_eq!(encoded["kind"], "new_chat");
        assert_eq!(encoded["address"], "Exact@Example.test");
        assert!(!format!("{key:?}").contains("exact-conversation"));
        assert!(!format!("{chat:?}").contains("Exact@Example.test"));

        let draft = DurableDraft {
            body: "draft body sentinel".into(),
            cursor: 2,
            pasted_body: true,
            uncertain: true,
        };
        let debug = format!("{draft:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("draft body sentinel"));
        let request = Request {
            request_id: 20,
            request: RequestKind::DraftSave {
                key: key.clone(),
                draft: draft.clone(),
                revision: 3,
            },
        };
        assert!(
            serde_json::from_slice::<Request>(&serde_json::to_vec(&request).unwrap())
                .unwrap()
                .request
                == request.request
        );
        let response = Response {
            request_id: 20,
            response: ResponseKind::DraftSave {
                outcome: DraftWriteOutcome::Applied,
            },
        };
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(encoded["response"]["result"], "draft_save");
        assert_eq!(encoded["response"]["outcome"], "applied");
        let loaded = DraftLoad {
            draft: Some(draft.clone()),
            revision_floor: 3,
            corrupt_deleted: true,
        };
        assert_eq!(
            serde_json::from_slice::<DraftLoad>(&serde_json::to_vec(&loaded).unwrap()).unwrap(),
            loaded
        );
        assert!(key.is_valid());
        assert!(!DraftKey::NewChat {
            transport: TransmissionForTests::transport(),
            address: "   ".into(),
        }
        .is_valid());
        let unicode = DurableDraft {
            body: "a\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}e\u{301}".into(),
            cursor: 3,
            pasted_body: false,
            uncertain: false,
        };
        assert!(unicode.is_valid());
        assert!(!DurableDraft {
            cursor: 4,
            ..unicode
        }
        .is_valid());
        assert!(!DurableDraft {
            body: "x".repeat(MAX_DRAFT_BYTES + 1),
            cursor: 0,
            pasted_body: false,
            uncertain: false,
        }
        .is_valid());
        assert!(draft_revision_is_valid(i64::MAX as u64));
        assert!(!draft_revision_is_valid(i64::MAX as u64 + 1));

        let largest = Request {
            request_id: 21,
            request: RequestKind::DraftSave {
                key,
                draft: DurableDraft {
                    body: "\\\"".repeat(MAX_DRAFT_BYTES / 2),
                    cursor: MAX_DRAFT_BYTES,
                    pasted_body: false,
                    uncertain: false,
                },
                revision: 4,
            },
        };
        assert!(serde_json::to_vec(&largest).unwrap().len() <= MAX_FRAME_LEN);
    }

    struct TransmissionForTests;
    impl TransmissionForTests {
        fn transport() -> TransportId {
            TransportId::new("test").unwrap()
        }
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
    async fn rejects_v5_and_v6_request_and_response_before_header_or_payload() {
        for old in [[5u8], [6u8]] {
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

    fn write_file(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "// sidecar\n").unwrap();
    }

    fn prefix_with_daemon(
        root: &std::path::Path,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let daemon = root.join("bin/bloomd");
        write_file(&daemon);
        (
            daemon,
            root.join("share/bloom/engine/bin/bloom_engine.dart"),
            root.join("bin/bloom-engine/bloom_engine.dart"),
        )
    }

    #[test]
    fn sidecar_environment_wins_over_installed_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, prefix_sidecar, _) = prefix_with_daemon(directory.path());
        write_file(&prefix_sidecar);
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);
        let explicit = directory.path().join("explicit/bloom_engine.dart");
        write_file(&explicit);

        let (found, source) =
            sidecar_script_with_source(&daemon, Some(explicit.as_os_str()), &checkout).unwrap();
        assert_eq!(found, explicit);
        assert_eq!(source, SidecarSource::Environment);
        assert_eq!(source.label(), "BLOOM_ENGINE_SCRIPT");
    }

    #[test]
    fn sidecar_prefix_wins_over_legacy_sibling_when_both_exist() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, prefix_sidecar, legacy_sidecar) = prefix_with_daemon(directory.path());
        write_file(&prefix_sidecar);
        write_file(&legacy_sidecar);
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);

        let (found, source) = sidecar_script_with_source(&daemon, None, &checkout).unwrap();
        assert_eq!(found, prefix_sidecar);
        assert_eq!(source, SidecarSource::Prefix);
    }

    #[test]
    fn sidecar_legacy_sibling_wins_when_prefix_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, _prefix_sidecar, legacy_sidecar) = prefix_with_daemon(directory.path());
        write_file(&legacy_sidecar);
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);

        let (found, source) = sidecar_script_with_source(&daemon, None, &checkout).unwrap();
        assert_eq!(found, legacy_sidecar);
        assert_eq!(source, SidecarSource::LegacySibling);
    }

    #[test]
    fn sidecar_falls_back_to_the_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, _prefix_sidecar, _legacy_sidecar) = prefix_with_daemon(directory.path());
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);

        assert_eq!(
            sidecar_script(&daemon, None, &checkout),
            Some(checkout.clone())
        );
        let (found, source) = sidecar_script_with_source(&daemon, None, &checkout).unwrap();
        assert_eq!(found, checkout);
        assert_eq!(source, SidecarSource::Checkout);
    }

    #[test]
    fn sidecar_unusable_environment_values_fall_through() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, prefix_sidecar, _) = prefix_with_daemon(directory.path());
        write_file(&prefix_sidecar);
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);

        for value in [OsStr::new("   "), OsStr::new("\t\n")] {
            let (found, source) =
                sidecar_script_with_source(&daemon, Some(value), &checkout).unwrap();
            assert_eq!(found, prefix_sidecar);
            assert_eq!(source, SidecarSource::Prefix);
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let non_utf8 = OsStr::from_bytes(b"\xff\xfe");
            let (found, source) =
                sidecar_script_with_source(&daemon, Some(non_utf8), &checkout).unwrap();
            assert_eq!(found, prefix_sidecar);
            assert_eq!(source, SidecarSource::Prefix);
        }
    }

    #[test]
    fn sidecar_nonexistent_candidates_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let (daemon, _prefix_sidecar, _legacy_sidecar) = prefix_with_daemon(directory.path());
        // Neither installed candidate exists, so only the checkout can win.
        let absent = directory.path().join("absent/bloom_engine.dart");
        assert!(!absent.exists());
        let checkout = directory.path().join("repo/bloom_engine.dart");
        write_file(&checkout);

        let (found, source) = sidecar_script_with_source(&daemon, None, &checkout).unwrap();
        assert_eq!(found, checkout);
        assert_eq!(source, SidecarSource::Checkout);
        assert_eq!(sidecar_script(&daemon, None, &absent), None);
    }
}
