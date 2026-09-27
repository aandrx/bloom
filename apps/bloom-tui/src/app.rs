//! UI state for the Bloom TUI. Rendering lives in `ui.rs`.

use crate::daemon::Snapshot;
use bloom_domain::{Conversation, ConversationId, HandleKind, Message};
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
    /// Set when the daemon reports a conversation listing change; the caller then
    /// re-reads the conversation list without disturbing the open thread.
    pub resync_conversations: bool,
    /// Active filter query. `Some` means the filter prompt is open; an empty query
    /// lists everything.
    pub filter: Option<String>,
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
        let visible = self.visible_ids();
        if visible.is_empty() {
            self.selected = None;
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|id| visible.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        let next = (at as isize + step).clamp(0, visible.len() as isize - 1) as usize;
        self.selected = Some(visible[next].clone());
    }

    /// The conversations the sidebar should show: everything, or the filtered set.
    pub fn visible_conversations(&self) -> Vec<&Conversation> {
        match self.filter_query() {
            None => self.conversations.iter().collect(),
            Some(query) if query.trim().is_empty() => self.conversations.iter().collect(),
            Some(query) => self
                .conversations
                .iter()
                .filter(|conversation| conversation_matches(conversation, query))
                .collect(),
        }
    }

    pub fn visible_ids(&self) -> Vec<ConversationId> {
        self.visible_conversations()
            .into_iter()
            .map(|conversation| conversation.id.clone())
            .collect()
    }

    /// The filter query, or None when the filter is closed.
    pub fn filter_query(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// True when a query is narrowing the list and nothing matches it.
    pub fn filter_has_no_matches(&self) -> bool {
        matches!(self.filter_query(), Some(query) if !query.trim().is_empty())
            && self.visible_conversations().is_empty()
    }

    /// Open the filter without touching an in-progress draft, so `/` cannot lose
    /// what the user was typing.
    pub fn begin_filter(&mut self) {
        self.filter = Some(String::new());
        self.composing = false;
        self.new_conversation = false;
        self.pending_address = None;
    }

    pub fn cancel_filter(&mut self) {
        self.filter = None;
    }

    pub fn filter_input(&mut self, character: char) {
        if let Some(query) = self.filter.as_mut() {
            query.push(character);
            self.reconcile_selection_with_visible();
        }
    }

    pub fn filter_backspace(&mut self) {
        if let Some(query) = self.filter.as_mut() {
            query.pop();
            self.reconcile_selection_with_visible();
        }
    }

    /// Close the filter and return the conversation the user chose.
    pub fn confirm_filter(&mut self) -> Option<ConversationId> {
        let selected = self.selected.clone();
        self.filter = None;
        selected
    }

    /// Keep the selection inside the filtered set as the query changes.
    fn reconcile_selection_with_visible(&mut self) {
        let visible = self.visible_ids();
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| visible.iter().any(|candidate| candidate == id))
        {
            self.selected = visible.first().cloned();
        }
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
            EventKind::ConversationsChanged => self.resync_conversations = true,
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

/// Does a conversation match a filter query?
///
/// Matches the title and every participant handle or display name, case
/// insensitively. Deliberately does NOT match the raw conversation id or remote id:
/// those carry the service prefix `iMessage;-;`, so matching them would make a query
/// as short as "s" or "i" match every conversation.
pub fn conversation_matches(conversation: &Conversation, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    if conversation
        .title
        .as_deref()
        .is_some_and(|title| title.to_lowercase().contains(&needle))
    {
        return true;
    }
    conversation.participants.iter().any(|participant| {
        participant.handle.value().to_lowercase().contains(&needle)
            || participant
                .display_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&needle))
    })
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

/// A compact label for a message's sender.
///
/// The mockup shows short names. Most engine participants have no display name, so
/// fall back to the local part of an email handle, capitalised, rather than printing
/// a full address on every message.
pub fn speaker_label(message: &Message, conversation: Option<&Conversation>) -> String {
    if message.from_me {
        return "You".to_owned();
    }
    if let Some(conversation) = conversation {
        if let Some(named) = conversation
            .participants
            .iter()
            .find(|participant| participant.handle == message.sender)
            .and_then(|participant| participant.display_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return named.to_owned();
        }
    }
    match message.sender.kind() {
        HandleKind::Email => match message.sender.value().split_once('@') {
            Some((local, _)) if !local.is_empty() => capitalise(local),
            _ => message.sender.value().to_owned(),
        },
        HandleKind::Tel => message.sender.value().to_owned(),
        HandleKind::Opaque => message.sender.value().to_owned(),
    }
}

fn capitalise(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

/// The status shown in the header: the daemon connection when it is not healthy,
/// otherwise the engine session. None when there is nothing worth showing.
pub fn status_text(app: &App) -> Option<String> {
    if !app.connection.is_empty() && app.connection != "Connected" {
        return Some(app.connection.clone());
    }
    let session = app.session.trim();
    if session.is_empty() {
        return None;
    }
    Some(session.to_owned())
}

/// Severity of the status shown in the header, so colour can carry meaning the way
/// the design's palette intends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    Healthy,
    Attention,
    Failure,
}

/// Session states that mean the engine needs the user's attention.
pub fn status_tone(app: &App) -> StatusTone {
    if !app.connection.is_empty() && app.connection != "Connected" {
        return StatusTone::Failure;
    }
    match app.session.trim() {
        "" | "Active" | "Registered" => StatusTone::Healthy,
        "Throttled" | "Blocked" | "Failed" | "Disconnected" => StatusTone::Failure,
        _ => StatusTone::Attention,
    }
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
    fn filtering_matches_names_handles_and_is_case_insensitive() {
        let mut app = App::default();
        let weekend = conversation("Weekend crew", Some(30));
        let jules = conversation("Jules", Some(20));
        app.set_conversations(vec![weekend.clone(), jules.clone()]);

        // By title, case-insensitively.
        app.begin_filter();
        for character in "weekend".chars() {
            app.filter_input(character);
        }
        assert_eq!(app.visible_ids(), vec![weekend.id.clone()]);

        // By participant handle.
        app.cancel_filter();
        app.begin_filter();
        for character in "JULES".chars() {
            app.filter_input(character);
        }
        assert_eq!(app.visible_ids(), vec![jules.id.clone()]);

        // Clearing restores everything.
        app.cancel_filter();
        assert_eq!(app.visible_ids().len(), 2);
    }

    #[test]
    fn a_filter_with_no_matches_reports_that_and_selects_nothing() {
        let mut app = App::default();
        app.set_conversations(vec![conversation("Weekend crew", Some(1))]);
        app.begin_filter();
        for character in "zzz".chars() {
            app.filter_input(character);
        }
        assert!(app.visible_conversations().is_empty());
        assert!(app.filter_has_no_matches());
        assert_eq!(app.selected, None, "no matches means nothing is selected");
    }

    #[test]
    fn confirming_the_filter_returns_the_chosen_conversation() {
        let mut app = App::default();
        let first = conversation("Weekend crew", Some(30));
        let second = conversation("Jules", Some(20));
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.begin_filter();
        for character in "jul".chars() {
            app.filter_input(character);
        }
        // The selection follows the narrowed list.
        assert_eq!(app.selected.as_ref(), Some(&second.id));
        assert_eq!(app.confirm_filter(), Some(second.id.clone()));
        assert_eq!(app.filter_query(), None, "confirming closes the filter");
        assert_eq!(app.visible_ids().len(), 2, "closing restores the full list");
    }

    #[test]
    fn opening_the_filter_keeps_an_in_progress_draft() {
        let mut app = App {
            composing: true,
            draft: "half written".into(),
            ..Default::default()
        };
        app.begin_filter();
        assert_eq!(app.draft, "half written", "the draft must survive");
        assert!(!app.composing, "the composer yields to the filter prompt");
        app.cancel_filter();
        assert_eq!(app.draft, "half written");
    }

    #[test]
    fn selection_steps_through_the_filtered_set_only() {
        let mut app = App::default();
        let first = conversation("Weekend crew", Some(30));
        let second = conversation("Jules", Some(20));
        let third = conversation("Design systems", Some(10));
        app.set_conversations(vec![first, second, third]);
        app.begin_filter();
        app.filter_input('s');
        let visible = app.visible_ids();
        assert!(
            visible.len() < 3,
            "the filter must narrow the list, query={:?} visible={}",
            app.filter_query(),
            visible.len()
        );
        app.select_step(1);
        let selected = app.selected.clone().expect("something is selected");
        assert!(
            visible.contains(&selected),
            "stepping must stay inside the filtered results"
        );
    }

    #[test]
    fn speaker_labels_prefer_names_then_short_forms() {
        let conversation = conversation("weekend", Some(1));
        let named = Message::new(
            MessageId::new(TransportId::new("openbubbles").unwrap(), "m1").unwrap(),
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Email, "maya@example.invalid").unwrap(),
            vec![bloom_domain::MessagePart::Text("hi".to_owned())],
            1,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        // No display name on the conversation: the local part is used, capitalised.
        assert_eq!(speaker_label(&named, Some(&conversation)), "Maya");

        let mut with_name = conversation.clone();
        with_name.participants[0].handle =
            Handle::new(HandleKind::Email, "maya@example.invalid").unwrap();
        with_name.participants[0].display_name = Some("Maya Chen".to_owned());
        assert_eq!(speaker_label(&named, Some(&with_name)), "Maya Chen");

        // Our own messages are always "You".
        let mine = Message::new(
            MessageId::new(TransportId::new("openbubbles").unwrap(), "m2").unwrap(),
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Email, "me@example.invalid").unwrap(),
            vec![bloom_domain::MessagePart::Text("mine".to_owned())],
            2,
            None,
            DeliveryState::Sent,
            true,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(speaker_label(&mine, Some(&conversation)), "You");

        // Phone numbers are shown as-is, and odd handles fall back to their value.
        let phone = Message::new(
            MessageId::new(TransportId::new("openbubbles").unwrap(), "m3").unwrap(),
            conversation.id.clone(),
            None,
            Handle::new(HandleKind::Tel, "+15550000001").unwrap(),
            vec![bloom_domain::MessagePart::Text("call".to_owned())],
            3,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(speaker_label(&phone, Some(&conversation)), "+15550000001");
        assert_eq!(speaker_label(&named, None), "Maya");
    }

    #[test]
    fn status_tone_distinguishes_trouble_from_health() {
        let healthy = App {
            connection: "Connected".into(),
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_tone(&healthy), StatusTone::Healthy);

        // The engine wanting a renewal is worth noticing, not a failure.
        let renewing = App {
            connection: "Connected".into(),
            session: "RenewalDue".into(),
            ..Default::default()
        };
        assert_eq!(status_tone(&renewing), StatusTone::Attention);

        // Throttled or blocked is a failure the user must act on.
        for state in ["Throttled", "Blocked", "Failed", "Disconnected"] {
            let bad = App {
                connection: "Connected".into(),
                session: state.into(),
                ..Default::default()
            };
            assert_eq!(status_tone(&bad), StatusTone::Failure, "state {state}");
        }

        // A dead daemon connection outranks whatever the session last was.
        let offline = App {
            connection: "Disconnected, retrying".into(),
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_tone(&offline), StatusTone::Failure);
    }

    #[test]
    fn header_status_prefers_connection_trouble_over_the_session() {
        let mut app = App {
            connection: "Connected".into(),
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_text(&app).as_deref(), Some("Active"));

        // A broken daemon connection is more important than the last known session.
        app.connection = "Disconnected, retrying".into();
        assert_eq!(status_text(&app).as_deref(), Some("Disconnected, retrying"));

        // Nothing worth showing yields nothing.
        app.connection = "Connected".into();
        app.session = "  ".into();
        assert_eq!(status_text(&app), None);
    }

    #[test]
    fn a_listing_change_asks_for_a_conversation_resync() {
        let mut app = App::default();
        assert!(!app.resync_conversations);
        app.apply_event(EventKind::ConversationsChanged);
        assert!(app.resync_conversations, "the caller must refresh the list");
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
