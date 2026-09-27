//! UI state for the Bloom TUI. Rendering lives in `ui.rs`.

use crate::daemon::Snapshot;
use bloom_domain::{Conversation, ConversationId, Message};
use bloom_ipc::EventKind;
use std::collections::HashMap;

#[derive(Default)]
pub struct App {
    pub conversations: Vec<Conversation>,
    pub selected: Option<ConversationId>,
    pub messages: HashMap<ConversationId, Vec<Message>>,
    pub connection: String,
    pub session: String,
    pub draft: String,
    pub composing: bool,
    /// Recipient address captured before the first message of a new conversation.
    pub pending_address: Option<String>,
    pub new_conversation: bool,
    pub error: Option<String>,
}

impl App {
    /// Apply a store snapshot. History is replaced, not merged, because the store is
    /// authoritative. The current selection is kept while it still exists.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        if let Some(id) = snapshot.visible.clone() {
            self.messages.insert(id.clone(), snapshot.history);
            // Adopt the snapshot's conversation only when nothing is selected yet;
            // a deliberate user selection must not be pulled back.
            if self.selected.is_none() {
                self.selected = Some(id);
            }
        }
        self.set_conversations(snapshot.conversations);
        self.error = None;
    }

    pub fn set_conversations(&mut self, mut conversations: Vec<Conversation>) {
        conversations.sort_by_key(|c| std::cmp::Reverse(c.last_message_at));
        self.conversations = conversations;
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| self.conversations.iter().any(|c| &c.id == id))
        {
            self.selected = self.conversations.first().map(|c| c.id.clone());
        }
    }

    /// Start a new conversation: first the recipient address, then the message.
    pub fn begin_new_conversation(&mut self) {
        self.new_conversation = true;
        self.pending_address = None;
        self.composing = true;
        self.draft.clear();
    }

    pub fn cancel_composition(&mut self) {
        self.new_conversation = false;
        self.pending_address = None;
        self.composing = false;
        self.draft.clear();
    }

    /// Advance the composer one step. Address entry does not send anything; it
    /// moves on to the body so a new conversation cannot be started with a
    /// placeholder message.
    pub fn submit_intent(&mut self) -> CompositionIntent {
        let value = self.draft.trim().to_owned();
        self.draft.clear();
        if self.new_conversation && self.pending_address.is_none() {
            if value.is_empty() {
                self.cancel_composition();
                return CompositionIntent::None;
            }
            self.pending_address = Some(value);
            self.composing = true;
            return CompositionIntent::None;
        }
        self.composing = false;
        if let Some(address) = self.pending_address.take() {
            self.new_conversation = false;
            if value.is_empty() {
                return CompositionIntent::None;
            }
            return CompositionIntent::StartConversation {
                address,
                body: value,
            };
        }
        CompositionIntent::SendMessage(value)
    }

    /// What the composer is currently asking for, if anything.
    pub fn composer_prompt(&self) -> ComposerPrompt {
        if self.new_conversation && self.pending_address.is_none() {
            ComposerPrompt::Recipient
        } else if let Some(address) = &self.pending_address {
            ComposerPrompt::Body(address.clone())
        } else if self.composing {
            ComposerPrompt::Message
        } else {
            ComposerPrompt::Idle
        }
    }

    pub fn select_step(&mut self, step: isize) {
        if self.conversations.is_empty() {
            self.selected = None;
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|id| self.conversations.iter().position(|c| &c.id == id))
            .unwrap_or(0);
        let next = (at as isize + step).clamp(0, self.conversations.len() as isize - 1) as usize;
        self.selected = Some(self.conversations[next].id.clone());
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations
            .iter()
            .find(|c| Some(&c.id) == self.selected.as_ref())
    }

    pub fn apply_event(&mut self, event: EventKind) {
        match event {
            EventKind::Message {
                conversation_id,
                message,
            }
            | EventKind::Update {
                conversation_id,
                message,
            } => {
                self.upsert(conversation_id.clone(), message);
                if let Some(c) = self
                    .conversations
                    .iter_mut()
                    .find(|c| c.id == conversation_id)
                {
                    c.last_message_at = self
                        .messages
                        .get(&conversation_id)
                        .and_then(|m| m.last())
                        .map(|m| m.created_at);
                }
                self.conversations
                    .sort_by_key(|c| std::cmp::Reverse(c.last_message_at));
            }
            EventKind::Delivery {
                conversation_id,
                message_id,
                state,
            } => {
                if let Some(m) = self
                    .messages
                    .get_mut(&conversation_id)
                    .and_then(|v| v.iter_mut().find(|m| m.id() == &message_id))
                {
                    m.delivery = state;
                }
            }
            EventKind::Session { state } => self.session = format!("{state:?}"),
        }
    }

    pub fn upsert(&mut self, id: ConversationId, message: Message) {
        let list = self.messages.entry(id).or_default();
        if let Some(old) = list.iter_mut().find(|m| m.id() == message.id()) {
            *old = message;
        } else {
            list.push(message);
        }
        list.sort_by_key(|m| m.created_at);
    }

    pub fn preview(&self, id: &ConversationId) -> String {
        self.messages
            .get(id)
            .and_then(|v| v.last())
            .map(message_text)
            .unwrap_or_default()
    }
}

pub fn chat_name(conversation: &Conversation) -> &str {
    conversation
        .title
        .as_deref()
        .or_else(|| {
            conversation
                .participants
                .first()
                .and_then(|p| p.display_name.as_deref())
        })
        .unwrap_or("Conversation")
}

pub fn participant_count(count: usize) -> String {
    format!("{count} {}", if count == 1 { "person" } else { "people" })
}

pub fn timestamp(created_at: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let time = UNIX_EPOCH
        .checked_add(Duration::from_millis(created_at.max(0) as u64))
        .unwrap_or(UNIX_EPOCH);
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let local = unsafe { libc::localtime_r(&seconds, &mut std::mem::zeroed()) };
    if local.is_null() {
        return "--:--".into();
    }
    let tm = unsafe { *local };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ComposerPrompt {
    Idle,
    Message,
    Recipient,
    Body(String),
}

pub enum CompositionIntent {
    /// The composer advanced a step without dispatching anything.
    None,
    StartConversation {
        address: String,
        body: String,
    },
    SendMessage(String),
}

pub fn message_text(message: &Message) -> String {
    message
        .parts()
        .iter()
        .map(|part| match part {
            bloom_domain::MessagePart::Text(s) => s.clone(),
            bloom_domain::MessagePart::Attachment(_) => "[attachment]".into(),
            bloom_domain::MessagePart::Unsupported { .. } => "[attachment]".into(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloom_domain::{
        ConversationKind, DeliveryState, Handle, HandleKind, MessageId, Participant, TransportId,
    };

    fn conversation(value: &str, last: Option<i64>) -> Conversation {
        let transport = TransportId::new("openbubbles").unwrap();
        Conversation {
            id: ConversationId::new(transport, value).unwrap(),
            remote_id: format!("iMessage;-;{value}"),
            kind: ConversationKind::Direct,
            title: None,
            participants: vec![Participant {
                handle: Handle::new(HandleKind::Opaque, value).unwrap(),
                display_name: None,
                is_me: false,
            }],
            created_at: 1,
            last_message_at: last,
        }
    }

    fn message(conversation: &Conversation, id: &str, at: i64, text: &str) -> Message {
        Message::new(
            MessageId::new(conversation.id.transport().clone(), id).unwrap(),
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Opaque, "someone").unwrap(),
            vec![bloom_domain::MessagePart::Text(text.to_owned())],
            at,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn helper_text_and_timestamp_formatting() {
        let chat = conversation("a", None);
        assert_eq!(chat_name(&chat), "Conversation");
        assert_eq!(participant_count(1), "1 person");
        assert_eq!(participant_count(3), "3 people");
        assert_eq!(timestamp(0).len(), 5);
        assert!(timestamp(0).contains(':'));
    }

    #[test]
    fn new_conversation_asks_for_the_recipient_before_the_body() {
        let mut app = App::default();
        app.begin_new_conversation();
        assert!(app.composing && app.new_conversation);
        assert!(matches!(app.composer_prompt(), ComposerPrompt::Recipient));

        // Entering the address only advances the composer; nothing is dispatched.
        app.draft = " someone@example.com ".into();
        assert!(matches!(app.submit_intent(), CompositionIntent::None));
        assert_eq!(app.pending_address.as_deref(), Some("someone@example.com"));
        assert!(matches!(app.composer_prompt(), ComposerPrompt::Body(_)));

        // The body is what actually starts the conversation.
        app.draft = "hello".into();
        match app.submit_intent() {
            CompositionIntent::StartConversation { address, body } => {
                assert_eq!(address, "someone@example.com");
                assert_eq!(body, "hello");
            }
            _ => panic!("expected a start-conversation intent"),
        }
        assert_eq!(app.pending_address, None);
        assert!(!app.new_conversation);

        // Cancelling clears everything.
        app.begin_new_conversation();
        app.draft = "someone@example.com".into();
        let _ = app.submit_intent();
        app.cancel_composition();
        assert_eq!(app.pending_address, None);
        assert!(!app.composing && !app.new_conversation);

        // An empty address aborts instead of starting a conversation.
        app.begin_new_conversation();
        app.draft = "   ".into();
        assert!(matches!(app.submit_intent(), CompositionIntent::None));
        assert!(!app.composing);
        assert_eq!(app.pending_address, None);
    }

    #[test]
    fn normal_messages_still_submit_as_a_single_step() {
        let mut app = App {
            composing: true,
            draft: "hello".into(),
            ..Default::default()
        };
        match app.submit_intent() {
            CompositionIntent::SendMessage(body) => assert_eq!(body, "hello"),
            _ => panic!("expected a send intent"),
        }
    }

    #[test]
    fn conversations_are_ordered_newest_first_and_selection_survives() {
        let mut app = App::default();
        let a = conversation("a", Some(10));
        let b = conversation("b", Some(30));
        let c = conversation("c", None);
        app.set_conversations(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(
            app.conversations
                .iter()
                .map(|c| c.id.value())
                .collect::<Vec<_>>(),
            vec!["b", "a", "c"],
            "newest first, unknown timestamps last"
        );
        assert_eq!(app.selected.as_ref().unwrap().value(), "b");

        app.select_step(1);
        assert_eq!(app.selected.as_ref().unwrap().value(), "a");
        // A later refresh must not move the user's selection.
        app.set_conversations(vec![c.clone(), a.clone(), b.clone()]);
        assert_eq!(app.selected.as_ref().unwrap().value(), "a");

        // A selection that disappears falls back to the first conversation.
        app.set_conversations(vec![c.clone()]);
        assert_eq!(app.selected.as_ref().unwrap().value(), "c");
    }

    #[test]
    fn snapshot_replaces_history_for_the_visible_conversation_only() {
        let mut app = App::default();
        let a = conversation("a", Some(10));
        let b = conversation("b", Some(20));
        app.upsert(a.id.clone(), message(&a, "stale", 1, "stale"));
        app.apply_snapshot(Snapshot {
            visible: Some(a.id.clone()),
            conversations: vec![a.clone(), b.clone()],
            history: vec![message(&a, "fresh", 2, "fresh")],
        });
        assert_eq!(
            app.preview(&a.id),
            "fresh",
            "authoritative history replaces"
        );
        assert_eq!(app.selected.as_ref().unwrap().value(), "a");
    }

    #[test]
    fn inbound_events_upsert_and_reorder() {
        let mut app = App::default();
        let a = conversation("a", Some(10));
        let b = conversation("b", Some(20));
        app.set_conversations(vec![a.clone(), b.clone()]);
        app.apply_event(EventKind::Message {
            conversation_id: a.id.clone(),
            message: message(&a, "m1", 50, "newest"),
        });
        assert_eq!(
            app.conversations[0].id.value(),
            "a",
            "latest message reorders"
        );
        assert_eq!(app.preview(&a.id), "newest");

        // A re-delivered message id must update in place, not duplicate.
        app.apply_event(EventKind::Message {
            conversation_id: a.id.clone(),
            message: message(&a, "m1", 50, "edited"),
        });
        assert_eq!(app.messages.get(&a.id).unwrap().len(), 1);
        assert_eq!(app.preview(&a.id), "edited");
    }

    #[test]
    fn delivery_and_session_events_update_state() {
        let mut app = App::default();
        let a = conversation("a", Some(1));
        app.set_conversations(vec![a.clone()]);
        app.upsert(a.id.clone(), message(&a, "m1", 5, "hi"));
        let id = app.messages[&a.id][0].id().clone();

        app.apply_event(EventKind::Delivery {
            conversation_id: a.id.clone(),
            message_id: id,
            state: DeliveryState::Read,
        });
        assert_eq!(app.messages[&a.id][0].delivery, DeliveryState::Read);

        app.apply_event(EventKind::Session {
            state: bloom_domain::SessionState::Active,
        });
        assert_eq!(app.session, "Active");
    }

    #[test]
    fn empty_state_is_selectable_without_panicking() {
        let mut app = App::default();
        app.select_step(1);
        assert!(app.selected.is_none());
        app.apply_event(EventKind::Session {
            state: bloom_domain::SessionState::Disconnected,
        });
        assert!(app.selected_conversation().is_none());
    }

    #[test]
    fn preview_renders_attachments_and_unknown_parts() {
        let a = conversation("a", Some(1));
        let message = Message::new(
            MessageId::new(a.id.transport().clone(), "m").unwrap(),
            a.id.clone(),
            None,
            Handle::new(HandleKind::Opaque, "someone").unwrap(),
            vec![
                bloom_domain::MessagePart::Unsupported {
                    kind: "attachment".to_owned(),
                },
                bloom_domain::MessagePart::Text("caption".to_owned()),
            ],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        let mut app = App::default();
        app.set_conversations(vec![a.clone()]);
        app.upsert(a.id.clone(), message);
        assert_eq!(app.preview(&a.id), "[attachment] caption");
    }
}
