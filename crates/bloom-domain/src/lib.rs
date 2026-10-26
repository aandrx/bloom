//! Transport-agnostic, serializable Bloom domain model.
use serde::{Deserialize, Serialize};
use std::fmt;

/// Milliseconds since the Unix epoch in UTC. Negative values represent pre-epoch times.
pub type Timestamp = i64;

#[derive(Clone, PartialEq, Eq)]
pub struct InvalidId;
impl fmt::Debug for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InvalidId")
    }
}
impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("identifier must not be empty")
    }
}
impl std::error::Error for InvalidId {}
fn nonempty(s: String) -> Result<String, InvalidId> {
    if s.trim().is_empty() {
        Err(InvalidId)
    } else {
        Ok(s)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TransportId(String);
impl TransportId {
    pub fn new(s: impl Into<String>) -> Result<Self, InvalidId> {
        Ok(Self(nonempty(s.into())?))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for TransportId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransportId([REDACTED])")
    }
}
impl<'de> Deserialize<'de> for TransportId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
macro_rules! local_id {
    ($n:ident) => {
        #[derive(Clone, PartialEq, Eq, Hash, Serialize)]
        pub struct $n {
            transport: TransportId,
            value: String,
        }
        impl $n {
            pub fn new(
                transport: TransportId,
                value: impl Into<String>,
            ) -> Result<Self, InvalidId> {
                Ok(Self {
                    transport,
                    value: nonempty(value.into())?,
                })
            }
            pub fn transport(&self) -> &TransportId {
                &self.transport
            }
            pub fn value(&self) -> &str {
                &self.value
            }
        }
        impl fmt::Debug for $n {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, stringify!($n),).and_then(|_| f.write_str("([REDACTED])"))
            }
        }
        impl<'de> Deserialize<'de> for $n {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                struct W {
                    transport: TransportId,
                    value: String,
                }
                let w = W::deserialize(d)?;
                Self::new(w.transport, w.value).map_err(serde::de::Error::custom)
            }
        }
    };
}
local_id!(ConversationId);
local_id!(MessageId);

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Handle {
    kind: HandleKind,
    value: String,
}
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleKind {
    Tel,
    Email,
    Opaque,
}
impl Handle {
    pub fn new(kind: HandleKind, value: impl Into<String>) -> Result<Self, InvalidId> {
        Ok(Self {
            kind,
            value: nonempty(value.into())?,
        })
    }
    pub fn kind(&self) -> &HandleKind {
        &self.kind
    }
    pub fn value(&self) -> &str {
        &self.value
    }
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handle([REDACTED])")
    }
}
impl<'de> Deserialize<'de> for Handle {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct W {
            kind: HandleKind,
            value: String,
        }
        let w = W::deserialize(d)?;
        Self::new(w.kind, w.value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
}
impl fmt::Debug for ConversationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Direct => "Direct",
            Self::Group => "Group",
        })
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub remote_id: String,
    pub kind: ConversationKind,
    pub title: Option<String>,
    pub participants: Vec<Participant>,
    pub created_at: Timestamp,
    pub last_message_at: Option<Timestamp>,
    /// Engine or legacy identities folded into this stable local conversation.
    /// These aliases preserve references such as local titles and hidden state.
    #[serde(default)]
    pub aliases: Vec<ConversationId>,
    /// A short preview of the newest message, for the sidebar's second line. The
    /// daemon fills it from its store; the engine's own listing carries no message
    /// text, so it is absent until Bloom has seen a message for the conversation.
    #[serde(default)]
    pub preview: Option<String>,
}
impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("id", &self.id)
            .field("remote_id", &"[REDACTED]")
            .field("kind", &self.kind)
            .field("title", &"[REDACTED]")
            .field("participants", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .field("last_message_at", &self.last_message_at)
            .finish()
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub handle: Handle,
    pub display_name: Option<String>,
    pub is_me: bool,
}
impl fmt::Debug for Participant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Participant")
            .field("handle", &self.handle)
            .field("display_name", &"[REDACTED]")
            .field("is_me", &self.is_me)
            .finish()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    Sent,
    Delivered,
    Read,
    Failed,
}

/// Message ownership is tied to one conversation and transport; use `new` to enforce it.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    id: MessageId,
    conversation_id: ConversationId,
    pub remote_id: Option<String>,
    pub sender: Handle,
    pub parts: Vec<MessagePart>,
    pub created_at: Timestamp,
    pub sent_at: Option<Timestamp>,
    pub delivery: DeliveryState,
    pub from_me: bool,
    pub edited_at: Option<Timestamp>,
    pub reactions: Vec<Reaction>,
    pub unsent_at: Option<Timestamp>,
}
impl Message {
    /// Construct a message while enforcing that its local and conversation identities
    /// belong to the same transport. Use this instead of constructing identity fields directly.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MessageId,
        conversation_id: ConversationId,
        remote_id: Option<String>,
        sender: Handle,
        parts: Vec<MessagePart>,
        created_at: Timestamp,
        sent_at: Option<Timestamp>,
        delivery: DeliveryState,
        from_me: bool,
        edited_at: Option<Timestamp>,
        reactions: Vec<Reaction>,
        unsent_at: Option<Timestamp>,
    ) -> Result<Self, InvalidId> {
        if id.transport() != conversation_id.transport() {
            return Err(InvalidId);
        }
        Ok(Self {
            id,
            conversation_id,
            remote_id,
            sender,
            parts,
            created_at,
            sent_at,
            delivery,
            from_me,
            edited_at,
            reactions,
            unsent_at,
        })
    }
    pub fn id(&self) -> &MessageId {
        &self.id
    }
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }
    pub fn parts(&self) -> &[MessagePart] {
        &self.parts
    }
    pub fn sent_at(&self) -> Option<Timestamp> {
        self.sent_at
    }
}
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("conversation_id", &self.conversation_id)
            .field("remote_id", &"[REDACTED]")
            .field("sender", &self.sender)
            .field("parts", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .field("sent_at", &self.sent_at)
            .field("delivery", &self.delivery)
            .field("from_me", &self.from_me)
            .field("edited_at", &self.edited_at)
            .field("reactions", &"[REDACTED]")
            .field("unsent_at", &self.unsent_at)
            .finish()
    }
}
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct W {
            id: MessageId,
            conversation_id: ConversationId,
            remote_id: Option<String>,
            sender: Handle,
            parts: Vec<MessagePart>,
            created_at: Timestamp,
            sent_at: Option<Timestamp>,
            delivery: DeliveryState,
            from_me: bool,
            edited_at: Option<Timestamp>,
            reactions: Vec<Reaction>,
            unsent_at: Option<Timestamp>,
        }
        let w = W::deserialize(d)?;
        Self::new(
            w.id,
            w.conversation_id,
            w.remote_id,
            w.sender,
            w.parts,
            w.created_at,
            w.sent_at,
            w.delivery,
            w.from_me,
            w.edited_at,
            w.reactions,
            w.unsent_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum MessagePart {
    Text(String),
    Attachment(AttachmentRef),
    Unsupported { kind: String },
}
impl fmt::Debug for MessagePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MessagePart([REDACTED])")
    }
}
impl<'de> Deserialize<'de> for MessagePart {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let ty = v
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("message part missing type"))?;
        match ty {
            "text" => {
                #[derive(Deserialize)]
                struct W {
                    value: String,
                }
                serde_json::from_value::<W>(v)
                    .map(|w| Self::Text(w.value))
                    .map_err(serde::de::Error::custom)
            }
            "attachment" => {
                #[derive(Deserialize)]
                struct W {
                    value: AttachmentRef,
                }
                serde_json::from_value::<W>(v)
                    .map(|w| Self::Attachment(w.value))
                    .map_err(serde::de::Error::custom)
            }
            "unsupported" => {
                #[derive(Deserialize)]
                struct V {
                    kind: String,
                }
                #[derive(Deserialize)]
                struct W {
                    value: V,
                }
                serde_json::from_value::<W>(v)
                    .map(|w| Self::Unsupported { kind: w.value.kind })
                    .map_err(serde::de::Error::custom)
            }
            other => Ok(Self::Unsupported {
                kind: other.to_owned(),
            }),
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct AttachmentRef {
    id: String,
    mime_type: Option<String>,
    file_name: Option<String>,
    byte_len: Option<u64>,
    /// Opaque sidecar fetch handle. Never a local path and never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetch_token: Option<String>,
}
impl AttachmentRef {
    /// Whether `token` is an opaque 32-lowercase-hex attachment fetch handle.
    pub fn is_valid_fetch_token(token: &str) -> bool {
        token.len() == 32
            && token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    }
    pub fn new(
        id: impl Into<String>,
        mime_type: Option<String>,
        file_name: Option<String>,
        byte_len: Option<u64>,
    ) -> Result<Self, InvalidId> {
        Ok(Self {
            id: nonempty(id.into())?,
            mime_type,
            file_name,
            byte_len,
            fetch_token: None,
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Builder that attaches an already-validated fetch token.
    pub fn with_fetch_token(mut self, token: Option<String>) -> Result<Self, InvalidId> {
        self.fetch_token = None;
        if let Some(token) = token {
            if !Self::is_valid_fetch_token(&token) {
                return Err(InvalidId);
            }
            self.fetch_token = Some(token);
        }
        Ok(self)
    }
    /// Opaque sidecar fetch handle, when the transport supplied one.
    pub fn fetch_token(&self) -> Option<&str> {
        self.fetch_token.as_deref()
    }
    /// Reported attachment file name, exactly as supplied by the transport.
    ///
    /// This is display metadata only: it is not a trusted local path, not a
    /// MIME source, and not a download handle. Returns `None` when the
    /// transport did not report a name.
    pub fn file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }
}
impl fmt::Debug for AttachmentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentRef")
            .field("id", &"[REDACTED]")
            .field("mime_type", &self.mime_type)
            .field("file_name", &"[REDACTED]")
            .field("byte_len", &self.byte_len)
            .field(
                "fetch_token",
                &self.fetch_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}
impl<'de> Deserialize<'de> for AttachmentRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct W {
            id: String,
            mime_type: Option<String>,
            file_name: Option<String>,
            byte_len: Option<u64>,
            #[serde(default)]
            fetch_token: Option<String>,
        }
        let w = W::deserialize(d)?;
        Self::new(w.id, w.mime_type, w.file_name, w.byte_len)
            .and_then(|attachment| attachment.with_fetch_token(w.fetch_token))
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub sender: Handle,
    pub kind: String,
    pub remote_id: Option<String>,
    /// Message part this reaction targets, when the transport reports one.
    /// Older stored reactions deserialize with `None`.
    #[serde(default)]
    pub part: Option<u32>,
    pub timestamp: Option<Timestamp>,
}
impl fmt::Debug for Reaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reaction([REDACTED])")
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingBody {
    pub parts: Vec<MessagePart>,
}
impl fmt::Debug for OutgoingBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OutgoingBody([REDACTED])")
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub reactions: bool,
    pub edits: bool,
    pub unsend: bool,
    pub attachments: bool,
    pub groups: bool,
    pub typing: bool,
    pub read_receipts: bool,
    pub sms: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Unregistered,
    Activating,
    Registered,
    Active,
    RenewalDue,
    Renewing,
    Throttled,
    Blocked,
    Failed,
    Disconnected,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tr() -> TransportId {
        TransportId::new("relay").unwrap()
    }
    fn sample(parts: Vec<MessagePart>) -> Message {
        Message::new(
            MessageId::new(tr(), "message-id-secret").unwrap(),
            ConversationId::new(tr(), "conversation-id-secret").unwrap(),
            None,
            Handle::new(HandleKind::Tel, "+15551234567").unwrap(),
            parts,
            1,
            None,
            DeliveryState::Queued,
            true,
            None,
            vec![],
            None,
        )
        .unwrap()
    }
    #[test]
    fn mismatch_rejected_in_constructor_and_json() {
        let mut value = serde_json::to_value(sample(vec![])).unwrap();
        value["id"]["transport"] = serde_json::json!("direct");
        assert!(serde_json::from_value::<Message>(value).is_err());
        assert!(Message::new(
            MessageId::new(TransportId::new("direct").unwrap(), "m").unwrap(),
            ConversationId::new(tr(), "c").unwrap(),
            None,
            Handle::new(HandleKind::Tel, "x").unwrap(),
            vec![],
            1,
            None,
            DeliveryState::Queued,
            true,
            None,
            vec![],
            None
        )
        .is_err());
    }
    #[test]
    fn old_conversation_json_defaults_identity_aliases() {
        let conversation = Conversation {
            id: ConversationId::new(tr(), "chat").unwrap(),
            remote_id: "engine-guid".into(),
            kind: ConversationKind::Direct,
            title: None,
            participants: Vec::new(),
            created_at: 1,
            last_message_at: None,
            aliases: Vec::new(),
            preview: None,
        };
        let mut value = serde_json::to_value(conversation).unwrap();
        value.as_object_mut().unwrap().remove("aliases");
        let restored: Conversation = serde_json::from_value(value).unwrap();
        assert!(restored.aliases.is_empty());
    }

    #[test]
    fn ids_and_handles_validate_and_roundtrip() {
        let id = MessageId::new(tr(), "m-1").unwrap();
        assert_eq!(
            serde_json::from_str::<MessageId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        assert!(serde_json::from_str::<TransportId>("\" \"").is_err());
        assert!(serde_json::from_str::<MessageId>(r#"{"transport":"relay","value":" "}"#).is_err());
        let handle = Handle::new(HandleKind::Tel, " +1 (555) ").unwrap();
        assert_eq!(handle.value(), " +1 (555) ");
        let json = serde_json::to_string(&handle).unwrap();
        assert_eq!(serde_json::from_str::<Handle>(&json).unwrap(), handle);
        assert!(serde_json::from_str::<Handle>(r#"{"kind":"email","value":" "}"#).is_err());
    }
    #[test]
    fn unknown_parts_fallback_and_malformed_known_parts_error() {
        assert_eq!(
            serde_json::from_str::<MessagePart>(r#"{"type":"future_part","value":{}}"#).unwrap(),
            MessagePart::Unsupported {
                kind: "future_part".into()
            }
        );
        assert!(serde_json::from_str::<MessagePart>(r#"{"type":"text","value":{}}"#).is_err());
    }
    #[test]
    fn debug_redacts_message_secrets_and_capabilities_default_off() {
        let mut m = sample(vec![
            MessagePart::Text("SECRET_BODY".into()),
            MessagePart::Attachment(
                AttachmentRef::new("SECRET_ATTACHMENT", None, Some("SECRET_FILE".into()), None)
                    .unwrap(),
            ),
        ]);
        m.remote_id = Some("SECRET_REMOTE".into());
        m.sender = Handle::new(HandleKind::Email, "secret@example.test").unwrap();
        let debug = format!("{m:?}");
        for secret in [
            "SECRET_BODY",
            "SECRET_ATTACHMENT",
            "SECRET_FILE",
            "SECRET_REMOTE",
            "secret@example.test",
            "message-id-secret",
            "conversation-id-secret",
        ] {
            assert!(!debug.contains(secret), "exposed {secret}");
        }
        assert_eq!(
            serde_json::from_str::<Capabilities>("{}").unwrap(),
            Capabilities::default()
        );
        assert!(!Capabilities::default().attachments && !Capabilities::default().reactions);
    }
    #[test]
    fn valid_message_roundtrip_and_attachment_only() {
        let m = sample(vec![MessagePart::Attachment(
            AttachmentRef::new("a", None, None, None).unwrap(),
        )]);
        assert_eq!(
            serde_json::from_str::<Message>(&serde_json::to_string(&m).unwrap()).unwrap(),
            m
        );
    }
    #[test]
    fn attachment_file_name_getter_returns_exact_borrowed_value() {
        let absent = AttachmentRef::new("a", None, None, None).unwrap();
        assert_eq!(absent.file_name(), None);
        for name in ["", "  ", "\t\n", "report.pdf", "часть.txt", "名前 💾"] {
            let attachment = AttachmentRef::new(
                "a",
                Some("application/octet-stream".into()),
                Some(name.into()),
                None,
            )
            .unwrap();
            assert_eq!(attachment.file_name(), Some(name));
            // Borrowed accessor: the returned slice aliases the stored string.
            let borrowed: Option<&str> = attachment.file_name();
            assert_eq!(borrowed, Some(name));
        }
    }
    #[test]
    fn attachment_roundtrip_keeps_file_name_and_wire_shape() {
        for file_name in [None, Some(String::new()), Some("файл.txt".into())] {
            let attachment = AttachmentRef::new(
                "synthetic-id",
                Some("text/plain".into()),
                file_name.clone(),
                Some(42),
            )
            .unwrap();
            let value = serde_json::to_value(&attachment).unwrap();
            let object = value.as_object().unwrap();
            assert_eq!(object.len(), 4);
            assert!(object.contains_key("id"));
            assert!(object.contains_key("mime_type"));
            assert!(object.contains_key("file_name"));
            assert!(object.contains_key("byte_len"));
            let restored: AttachmentRef = serde_json::from_value(value).unwrap();
            assert_eq!(restored, attachment);
            assert_eq!(restored.file_name(), file_name.as_deref());
        }
    }
    #[test]
    fn attachment_json_without_file_name_decodes_to_none() {
        let restored: AttachmentRef = serde_json::from_str(
            r#"{"id":"uuid-like-1234.pdf","mime_type":"application/pdf","byte_len":7}"#,
        )
        .unwrap();
        assert_eq!(restored.file_name(), None);
        assert_eq!(restored.fetch_token(), None);
    }
    #[test]
    fn attachment_fetch_token_is_optional_validated_and_redacted() {
        let token = "0123456789abcdef0123456789abcdef";
        let attachment = AttachmentRef::new("a", None, None, None)
            .unwrap()
            .with_fetch_token(Some(token.into()))
            .unwrap();
        assert_eq!(attachment.fetch_token(), Some(token));
        let value = serde_json::to_value(&attachment).unwrap();
        assert_eq!(value["fetch_token"], token);
        assert_eq!(
            serde_json::from_value::<AttachmentRef>(value).unwrap(),
            attachment
        );
        let debug = format!("{attachment:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(token));

        for invalid in [
            "",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
        ] {
            assert!(!AttachmentRef::is_valid_fetch_token(invalid));
            assert!(AttachmentRef::new("a", None, None, None)
                .unwrap()
                .with_fetch_token(Some(invalid.into()))
                .is_err());
        }
        assert!(
            serde_json::from_str::<AttachmentRef>(r#"{"id":"a","fetch_token":"not-a-token"}"#)
                .is_err()
        );
    }
    #[test]
    fn reaction_part_round_trips_and_defaults_to_none() {
        let sender = Handle::new(HandleKind::Email, "friend@example.invalid").unwrap();
        let reaction = Reaction {
            sender: sender.clone(),
            kind: "like".into(),
            remote_id: None,
            part: Some(2),
            timestamp: Some(1_700_000_000_000),
        };
        let json = serde_json::to_value(&reaction).unwrap();
        assert_eq!(json["part"], 2);
        let restored: Reaction = serde_json::from_value(json).unwrap();
        assert_eq!(restored, reaction);

        // Older stored rows have no part field and must still load.
        let legacy = format!(
            r#"{{"sender":{},"kind":"like","remote_id":null,"timestamp":null}}"#,
            serde_json::to_string(&sender).unwrap()
        );
        let restored: Reaction = serde_json::from_str(&legacy).unwrap();
        assert_eq!(restored.part, None);
        assert_eq!(restored.kind, "like");
    }

    #[test]
    fn empty_attachment_id_rejected_constructor_and_part_json() {
        assert!(AttachmentRef::new(" \t", None, None, None).is_err());
        assert!(
            serde_json::from_str::<MessagePart>(r#"{"type":"attachment","value":{"id":" "}}"#)
                .is_err()
        );
    }
}
