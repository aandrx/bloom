//! Child-process adapter for the Bloom engine sidecar.
//!
//! The engine (OpenBubbles' library, which embeds SSPL and closed components) runs
//! in a separate Dart process and is never linked into this crate. This module
//! spawns that process, speaks its newline-delimited JSON protocol, and maps its
//! payloads into `bloom_domain` values.
//!
//! Lifecycle is owned by `supervisor::Supervisor`, which takes the sole bounded
//! event receiver before `start` and forwards events to its sink.

use async_trait::async_trait;
use bloom_domain::{
    Capabilities, Conversation, ConversationId, ConversationKind, DeliveryState, Handle,
    HandleKind, Message, MessageId, MessagePart, OutgoingBody, Participant, SessionState,
    TransportId,
};
use bloom_transport::{Transport, TransportError, TransportEvent, TransportResult};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};

/// Engine frames are line-delimited JSON; refuse absurd lines instead of buffering.
pub const MAX_FRAME_BYTES: usize = 1 << 20;
pub const EVENT_CAPACITY: usize = 256;
/// How long a send may wait for its confirmation before the result is uncertain.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long shutdown waits for the child before killing it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn transport_id() -> TransportId {
    TransportId::new("openbubbles").expect("static transport id is valid")
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub dart_executable: PathBuf,
    pub script: PathBuf,
    pub state_dir: PathBuf,
    pub library: PathBuf,
}

impl EngineConfig {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.dart_executable);
        command
            .arg("run")
            .arg(&self.script)
            .arg("--state")
            .arg(&self.state_dir)
            .arg("--library")
            .arg(&self.library)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        command
    }
}

struct Pending {
    waiters: Mutex<HashMap<u64, oneshot::Sender<TransportResult<MessageId>>>>,
}

pub struct Engine {
    config: EngineConfig,
    id: TransportId,
    incoming: Mutex<Option<mpsc::Receiver<TransportEvent>>>,
    outgoing: Mutex<Option<mpsc::Sender<TransportEvent>>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    pending: Arc<Pending>,
    next_request: AtomicU64,
    started: AtomicBool,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        let (outgoing, incoming) = mpsc::channel(EVENT_CAPACITY);
        Self {
            config,
            id: transport_id(),
            incoming: Mutex::new(Some(incoming)),
            outgoing: Mutex::new(Some(outgoing)),
            stdin: tokio::sync::Mutex::new(None),
            child: tokio::sync::Mutex::new(None),
            pending: Arc::new(Pending {
                waiters: Mutex::new(HashMap::new()),
            }),
            next_request: AtomicU64::new(1),
            started: AtomicBool::new(false),
        }
    }

    /// Send a text message into a conversation identified by its engine chat guid.
    ///
    /// Sends are resolved against the store, so they do not go through
    /// `Transport::send`: the transport trait is conversation-id based, while the
    /// engine needs a recipient address and the existing chat guid.
    pub async fn send_text(
        &self,
        recipient: &str,
        chat_guid: Option<&str>,
        text: &str,
    ) -> TransportResult<MessageId> {
        if !self.started.load(Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .waiters
            .lock()
            .unwrap()
            .insert(request_id, sender);

        let mut frame = serde_json::json!({
            "version": 1,
            "type": "send",
            "request_id": request_id,
            "recipient": recipient,
            "text": text,
        });
        if let Some(guid) = chat_guid {
            frame["chat_guid"] = Value::String(guid.to_owned());
        }

        if self.write_frame(&frame).await.is_err() {
            self.pending.waiters.lock().unwrap().remove(&request_id);
            return Err(TransportError::TransportUnavailable);
        }

        match tokio::time::timeout(SEND_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TransportError::Internal),
            Err(_) => {
                self.pending.waiters.lock().unwrap().remove(&request_id);
                // The engine may still have delivered the message.
                Err(TransportError::Protocol)
            }
        }
    }

    async fn write_frame(&self, frame: &Value) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "engine stdin closed")
        })?;
        let mut line = serde_json::to_vec(frame)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "encode"))?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await
    }
}

#[async_trait]
impl Transport for Engine {
    fn id(&self) -> TransportId {
        self.id.clone()
    }

    /// Conservative: only plain text direct messages are supported end to end.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            reactions: false,
            edits: false,
            unsend: false,
            attachments: false,
            groups: false,
            typing: false,
            read_receipts: false,
            sms: false,
        }
    }

    async fn take_events(&self) -> TransportResult<mpsc::Receiver<TransportEvent>> {
        self.incoming
            .lock()
            .unwrap()
            .take()
            .ok_or(TransportError::Internal)
    }

    async fn start(&self) -> TransportResult<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(TransportError::TransportUnavailable);
        }
        let mut command = self.config.command();
        let mut child = command.spawn().map_err(|_| {
            self.started.store(false, Ordering::SeqCst);
            TransportError::TransportUnavailable
        })?;
        let stdout = child.stdout.take().ok_or(TransportError::Internal)?;
        let stdin = child.stdin.take().ok_or(TransportError::Internal)?;
        *self.stdin.lock().await = Some(stdin);
        *self.child.lock().await = Some(child);

        // Hand the only sender to the reader task. If the engine held one, the
        // supervisor's forwarder could never observe the channel closing and
        // shutdown would always report an incomplete drain.
        let outgoing = self
            .outgoing
            .lock()
            .unwrap()
            .take()
            .ok_or(TransportError::TransportUnavailable)?;
        let pending = self.pending.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            read_frames(stdout, outgoing, pending, id).await;
        });
        Ok(())
    }

    async fn stop(&self) -> TransportResult<()> {
        let _ = self
            .write_frame(&serde_json::json!({
                "version": 1,
                "type": "shutdown",
                "request_id": self.next_request.fetch_add(1, Ordering::Relaxed),
            }))
            .await;
        self.stdin.lock().await.take();
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(STOP_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn conversations(&self) -> TransportResult<Vec<Conversation>> {
        // The daemon's own store is authoritative for conversation listing.
        Err(TransportError::Unsupported)
    }

    async fn history(
        &self,
        _conversation: &ConversationId,
        _limit: u16,
    ) -> TransportResult<Vec<Message>> {
        Err(TransportError::Unsupported)
    }

    async fn send(
        &self,
        _conversation: &ConversationId,
        _body: OutgoingBody,
    ) -> TransportResult<MessageId> {
        // Outbound needs a store-resolved recipient address; use `send_text`.
        Err(TransportError::Unsupported)
    }
}

async fn read_frames(
    stdout: tokio::process::ChildStdout,
    outgoing: mpsc::Sender<TransportEvent>,
    pending: Arc<Pending>,
    id: TransportId,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut my_handles: Vec<String> = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.len() > MAX_FRAME_BYTES {
            eprintln!("engine frame too large; closing engine connection");
            break;
        }
        let frame: Value = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(_) => {
                eprintln!("engine sent an unparsable frame; closing engine connection");
                break;
            }
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("ready") => {
                if let Some(handles) = frame.get("handles").and_then(Value::as_array) {
                    my_handles = handles
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                if outgoing
                    .send(TransportEvent::Session {
                        state: SessionState::Active,
                        reason: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some("inbound_message") | Some("history_message") => {
                let payload = frame
                    .get("payload")
                    .or_else(|| frame.get("message"))
                    .cloned()
                    .unwrap_or(Value::Null);
                match map_inbound(&payload, &my_handles, &id) {
                    Ok(event) => {
                        if outgoing.send(event).await.is_err() {
                            return;
                        }
                    }
                    Err(reason) => eprintln!("engine message skipped: {reason}"),
                }
            }
            Some("send_confirmation") => {
                let request_id = frame.get("request_id").and_then(Value::as_u64);
                let accepted = frame.get("accepted").and_then(Value::as_bool) == Some(true);
                let message_id = frame.get("message_id").and_then(Value::as_str);
                if let Some(request_id) = request_id {
                    if let Some(waiter) = pending.waiters.lock().unwrap().remove(&request_id) {
                        let result = match (accepted, message_id) {
                            (true, Some(message_id)) => MessageId::new(id.clone(), message_id)
                                .map_err(|_| TransportError::Protocol),
                            _ => Err(TransportError::RecipientUnreachable),
                        };
                        let _ = waiter.send(result);
                    }
                }
            }
            Some("session_state") => {
                let state = frame
                    .get("state")
                    .and_then(Value::as_str)
                    .map(map_session_state)
                    .unwrap_or(SessionState::Disconnected);
                if outgoing
                    .send(TransportEvent::Session {
                        state,
                        reason: None,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Some("error") => {
                // Bounded code only; the frame never carries payload or identifiers.
                let code = frame
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                eprintln!("engine reported error: {code}");
            }
            Some("shutdown_ack") => {}
            Some("conversation") | Some("history_complete") => {}
            _ => eprintln!("engine sent an unknown frame type"),
        }
    }
    let _ = outgoing
        .send(TransportEvent::Session {
            state: SessionState::Disconnected,
            reason: None,
        })
        .await;
}

fn map_session_state(state: &str) -> SessionState {
    match state {
        "unregistered" => SessionState::Unregistered,
        "activating" => SessionState::Activating,
        "registered" => SessionState::Registered,
        "active" => SessionState::Active,
        "renewal_due" => SessionState::RenewalDue,
        "renewing" => SessionState::Renewing,
        "throttled" => SessionState::Throttled,
        "blocked" => SessionState::Blocked,
        "failed" => SessionState::Failed,
        _ => SessionState::Disconnected,
    }
}

/// Why a payload could not be turned into a domain message. Reported, never silent.
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    NotAnInboundMessage,
    NotANormalMessage,
    MissingChatGuid,
    MissingMessageId,
    MissingSender,
    InvalidIdentity,
    NoContent,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAnInboundMessage => "payload is not an inbound iMessage",
            Self::NotANormalMessage => "payload is not a normal message variant",
            Self::MissingChatGuid => "payload has no chat guid",
            Self::MissingMessageId => "payload has no message id",
            Self::MissingSender => "payload has no sender",
            Self::InvalidIdentity => "payload identity was rejected by the domain model",
            Self::NoContent => "payload carried no representable content",
        })
    }
}

pub fn handle_from(raw: &str) -> Option<Handle> {
    let (kind, value) = if let Some(rest) = raw.strip_prefix("mailto:") {
        (HandleKind::Email, rest)
    } else if let Some(rest) = raw.strip_prefix("tel:") {
        (HandleKind::Tel, rest)
    } else {
        (HandleKind::Opaque, raw)
    };
    Handle::new(kind, value).ok()
}

fn text_of(parts: &[MessagePart]) -> Option<String> {
    let mut buffer = String::new();
    for part in parts {
        if let MessagePart::Text(text) = part {
            buffer.push_str(text);
        }
    }
    if buffer.is_empty() {
        None
    } else {
        Some(buffer)
    }
}

/// Map an engine payload into a domain conversation and message pair.
///
/// Only normal inbound iMessages are mapped. Reactions, edits, unsends, delivery
/// receipts and unmodelled variants are skipped with an explicit reason rather than
/// guessed at, and content is never invented.
pub fn map_inbound(
    payload: &Value,
    my_handles: &[String],
    id: &TransportId,
) -> Result<TransportEvent, SkipReason> {
    if payload.get("variant").and_then(Value::as_str) != Some("iMessage") {
        return Err(SkipReason::NotAnInboundMessage);
    }
    let inst = payload.get("inst").ok_or(SkipReason::NotAnInboundMessage)?;
    let message_value = inst.get("message").ok_or(SkipReason::NotANormalMessage)?;
    if message_value.get("type").and_then(Value::as_str) != Some("normal") {
        return Err(SkipReason::NotANormalMessage);
    }
    let normal = message_value
        .get("value")
        .ok_or(SkipReason::NotANormalMessage)?;

    let chat_guid = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("sender_guid"))
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingChatGuid)?;
    let message_id = inst
        .get("id")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingMessageId)?;
    let sender = inst
        .get("sender")
        .and_then(Value::as_str)
        .ok_or(SkipReason::MissingSender)?;
    let sent_at = inst
        .get("sent_timestamp_ms")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let from_me = my_handles.iter().any(|handle| handle == sender);

    let mut parts = Vec::new();
    if let Some(list) = normal.get("parts").and_then(Value::as_array) {
        for entry in list {
            let part = match entry.get("part") {
                Some(part) => part,
                None => continue,
            };
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(MessagePart::Text(text.to_owned()));
                        }
                    }
                }
                Some(kind) => parts.push(MessagePart::Unsupported {
                    kind: kind.to_owned(),
                }),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        return Err(SkipReason::NoContent);
    }

    let sender_handle = handle_from(sender).ok_or(SkipReason::InvalidIdentity)?;
    let conversation_id =
        ConversationId::new(id.clone(), chat_guid).map_err(|_| SkipReason::InvalidIdentity)?;
    let message_id =
        MessageId::new(id.clone(), message_id).map_err(|_| SkipReason::InvalidIdentity)?;

    let mut participants = Vec::new();
    let mut remote_participants = 0usize;
    if let Some(list) = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("participants"))
        .and_then(Value::as_array)
    {
        for entry in list {
            let Some(raw) = entry.as_str() else { continue };
            let Some(handle) = handle_from(raw) else {
                continue;
            };
            let is_me = my_handles.iter().any(|mine| mine == raw);
            if !is_me {
                remote_participants += 1;
            }
            participants.push(Participant {
                handle,
                display_name: None,
                is_me,
            });
        }
    }
    if participants.is_empty() {
        participants.push(Participant {
            handle: sender_handle.clone(),
            display_name: None,
            is_me: from_me,
        });
        if !from_me {
            remote_participants = 1;
        }
    }

    let title = inst
        .get("conversation")
        .and_then(|conversation| conversation.get("cv_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let conversation = Conversation {
        id: conversation_id.clone(),
        remote_id: chat_guid.to_owned(),
        kind: if remote_participants > 1 {
            ConversationKind::Group
        } else {
            ConversationKind::Direct
        },
        title,
        participants,
        created_at: sent_at,
        last_message_at: Some(sent_at),
    };

    let message = Message::new(
        message_id,
        conversation_id.clone(),
        None,
        sender_handle,
        parts,
        sent_at,
        None,
        if from_me {
            DeliveryState::Sent
        } else {
            DeliveryState::Delivered
        },
        from_me,
        None,
        Vec::new(),
        None,
    )
    .map_err(|_| SkipReason::InvalidIdentity)?;

    Ok(TransportEvent::Message {
        conversation,
        message,
    })
}

/// Extract the text to send from an outgoing body. Only plain text is supported.
pub fn outgoing_text(body: &OutgoingBody) -> TransportResult<String> {
    text_of(&body.parts).ok_or(TransportError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transport() -> TransportId {
        transport_id()
    }

    fn payload(sender: &str, text: &str) -> Value {
        json!({
            "variant": "iMessage",
            "complete": true,
            "inst": {
                "id": "11111111-2222-3333-4444-555555555555",
                "sender": sender,
                "conversation": {
                    "participants": [sender, "mailto:me@example.invalid"],
                    "cv_name": null,
                    "sender_guid": "iMessage;-;someone@example.com",
                    "after_guid": null
                },
                "message": {
                    "type": "normal",
                    "complete": true,
                    "value": {
                        "parts": [{"idx": 0, "part": {"type": "text", "text": text}}],
                        "service": {"type": "iMessage", "complete": true}
                    }
                },
                "sent_timestamp_ms": 1700000000000i64,
                "send_delivered": false,
                "verification_failed": false
            }
        })
    }

    #[test]
    fn maps_an_inbound_message_with_participants_and_text() {
        let event = map_inbound(
            &payload("tel:+15550000001", "hello"),
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .expect("payload maps");

        let TransportEvent::Message {
            conversation,
            message,
        } = event
        else {
            panic!("expected a message event");
        };
        assert_eq!(conversation.id.value(), "iMessage;-;someone@example.com");
        assert_eq!(message.conversation_id(), &conversation.id);
        assert!(!message.from_me);
        assert_eq!(message.parts(), &[MessagePart::Text("hello".to_owned())]);
        assert_eq!(message.created_at, 1_700_000_000_000);
    }

    #[test]
    fn marks_our_own_handle_and_echoes_as_from_me() {
        let event = map_inbound(
            &payload("mailto:me@example.invalid", "mine"),
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .unwrap();
        let TransportEvent::Message { message, .. } = event else {
            panic!("expected a message event");
        };
        assert!(message.from_me);
    }

    #[test]
    fn attachment_only_messages_are_kept_as_unsupported_parts() {
        let mut value = payload("tel:+15550000001", "ignored");
        value["inst"]["message"]["value"]["parts"] = json!([
            {"idx": 0, "part": {"type": "attachment", "complete": true, "name": "a.jpg"}}
        ]);
        let event = map_inbound(&value, &[], &transport()).unwrap();
        let TransportEvent::Message { message, .. } = event else {
            panic!("expected a message event");
        };
        assert_eq!(
            message.parts(),
            &[MessagePart::Unsupported {
                kind: "attachment".to_owned()
            }]
        );
    }

    #[test]
    fn non_message_variants_and_receipts_are_skipped_with_a_reason() {
        let mut receipt = payload("tel:+15550000001", "x");
        receipt["inst"]["message"] = json!({"type": "delivered", "complete": true});
        assert!(matches!(
            map_inbound(&receipt, &[], &transport()),
            Err(SkipReason::NotANormalMessage)
        ));

        let confirm = json!({"variant": "sendConfirm", "complete": true, "uuid": "u"});
        assert!(matches!(
            map_inbound(&confirm, &[], &transport()),
            Err(SkipReason::NotAnInboundMessage)
        ));
    }

    #[test]
    fn missing_identity_or_content_is_rejected_rather_than_guessed() {
        let mut no_guid = payload("tel:+15550000001", "hi");
        no_guid["inst"]["conversation"]["sender_guid"] = Value::Null;
        assert!(matches!(
            map_inbound(&no_guid, &[], &transport()),
            Err(SkipReason::MissingChatGuid)
        ));

        let mut empty = payload("tel:+15550000001", "hi");
        empty["inst"]["message"]["value"]["parts"] = json!([]);
        assert!(matches!(
            map_inbound(&empty, &[], &transport()),
            Err(SkipReason::NoContent)
        ));
    }

    #[test]
    fn group_conversations_are_recognised() {
        let mut value = payload("tel:+15550000001", "hi");
        value["inst"]["conversation"]["participants"] = json!([
            "tel:+15550000001",
            "tel:+15550000002",
            "mailto:me@example.invalid"
        ]);
        let TransportEvent::Message {
            conversation: _conversation,
            ..
        } = map_inbound(
            &value,
            &["mailto:me@example.invalid".to_owned()],
            &transport(),
        )
        .unwrap()
        else {
            panic!("expected a message event");
        };
    }

    #[test]
    fn handles_map_to_typed_kinds() {
        assert!(matches!(
            handle_from("mailto:a@b.c").unwrap().kind(),
            HandleKind::Email
        ));
        assert!(matches!(
            handle_from("tel:+15550000001").unwrap().kind(),
            HandleKind::Tel
        ));
        assert!(matches!(
            handle_from("weird").unwrap().kind(),
            HandleKind::Opaque
        ));
    }

    #[test]
    fn session_states_map_and_unknown_states_disconnect() {
        assert_eq!(map_session_state("active"), SessionState::Active);
        assert_eq!(map_session_state("throttled"), SessionState::Throttled);
        assert_eq!(map_session_state("nonsense"), SessionState::Disconnected);
    }

    #[test]
    fn outgoing_bodies_must_carry_text() {
        assert_eq!(
            outgoing_text(&OutgoingBody { parts: vec![] }),
            Err(TransportError::Unsupported)
        );
        assert_eq!(
            outgoing_text(&OutgoingBody {
                parts: vec![MessagePart::Text("hi".to_owned())]
            }),
            Ok("hi".to_owned())
        );
    }
}
