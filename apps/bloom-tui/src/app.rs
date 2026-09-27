//! UI state for the Bloom TUI. Rendering lives in `ui.rs`.

use crate::daemon::{ConnectionState, Snapshot};
use bloom_domain::{Conversation, ConversationId, ConversationKind, Handle, HandleKind, Message};
use bloom_ipc::EventKind;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

/// Rows a single PageUp or PageDown moves through the transcript.
const SCROLL_PAGE: usize = 10;

/// What a key press wants the shell to do.
///
/// The key semantics live here rather than inside the event loop so the real key
/// path is testable; the loop only performs the async work an action asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    SendMessage {
        conversation: ConversationId,
        body: String,
    },
    StartConversation {
        address: String,
        body: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPhase {
    Sending,
    Sent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFeedback {
    pub conversation: Option<ConversationId>,
    pub phase: SendPhase,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        };
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

#[derive(Default)]
pub struct App {
    pub theme: Theme,
    /// Terminal-detected theme used when the preference is automatic.
    pub system_theme: Theme,
    /// None means follow the terminal theme; Some pins a user preference.
    pub theme_override: Option<Theme>,
    pub compact_sidebar: bool,
    pub settings_open: bool,
    pub settings_selection: usize,
    pub conversations: Vec<Conversation>,
    pub selected: Option<ConversationId>,
    pub messages: HashMap<ConversationId, Vec<Message>>,
    pub connection: String,
    pub connection_state: ConnectionState,
    pub frame: u64,
    pub thread_loading: bool,
    pub session: String,
    pub draft: String,
    /// Extended-grapheme insertion position in the draft.
    pub draft_cursor: usize,
    /// Display rows scrolled back from the newest message. Zero pins the transcript
    /// to the newest content, which is what a chat client should do by default.
    pub scroll: usize,
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
    pub notice: Option<String>,
    pub send_feedback: Option<SendFeedback>,
    /// Local-only nickname overrides, keyed by stable participant handle.
    pub nicknames: HashMap<String, String>,
    /// Filterable command menu is only active while composing.
    pub command_menu: bool,
    pub command_selection: usize,
    pub leader_pending: bool,
}

impl App {
    pub fn is_animating(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Connecting | ConnectionState::Disconnected
        ) || !matches!(self.session.as_str(), "" | "Active")
            || self.thread_loading
            || self.messages.values().flatten().any(|message| {
                message.from_me && matches!(message.delivery, bloom_domain::DeliveryState::Queued)
            })
    }

    pub fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

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
            // A different thread is on screen now, so the reading position from the
            // previous one must not carry over.
            self.scroll = 0;
        }
    }

    /// Move the selection. A reading position belongs to the thread it was made in,
    /// so opening a different thread returns to that thread's newest message.
    pub fn select(&mut self, id: Option<ConversationId>) {
        if self.selected != id {
            self.scroll = 0;
            self.send_feedback = None;
        }
        self.selected = id;
    }

    /// Scroll the transcript back through older messages, in display rows.
    pub fn scroll_back(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
    }

    /// Scroll forward towards the newest message. Zero means pinned to the newest.
    pub fn scroll_forward(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Request the oldest content. The renderer clamps this to what exists.
    pub fn scroll_to_oldest(&mut self) {
        self.scroll = usize::MAX;
    }

    /// Return to the newest message.
    pub fn scroll_to_newest(&mut self) {
        self.scroll = 0;
    }

    /// True while the transcript is showing something other than the newest message.
    pub fn is_scrolled_back(&self) -> bool {
        self.scroll > 0
    }

    /// Start a new conversation: first the recipient address, then the message.
    pub fn begin_new_conversation(&mut self) {
        self.leader_pending = false;
        self.command_menu = false;
        self.new_conversation = true;
        self.pending_address = None;
        self.composing = true;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    pub fn cancel_composition(&mut self) {
        self.command_menu = false;
        self.new_conversation = false;
        self.pending_address = None;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Advance the composer one step. Address entry does not send anything; it
    /// moves on to the body so a new conversation cannot be started with a
    /// placeholder message.
    pub fn submit_intent(&mut self) -> CompositionIntent {
        let value = self.draft.trim().to_owned();
        self.draft.clear();
        self.draft_cursor = 0;
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
            self.select(None);
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|id| visible.iter().position(|candidate| candidate == id))
            .unwrap_or(0);
        let next = (at as isize + step).clamp(0, visible.len() as isize - 1) as usize;
        self.select(Some(visible[next].clone()));
    }

    /// The conversations the sidebar should show: everything, or the filtered set.
    pub fn visible_conversations(&self) -> Vec<&Conversation> {
        match self.filter_query() {
            None => self.conversations.iter().collect(),
            Some(query) if query.trim().is_empty() => self.conversations.iter().collect(),
            Some(query) => self
                .conversations
                .iter()
                .filter(|conversation| {
                    conversation_matches(conversation, query)
                        || conversation.participants.iter().any(|participant| {
                            self.nicknames
                                .get(&nickname_key(&participant.handle))
                                .is_some_and(|nickname| {
                                    nickname
                                        .to_lowercase()
                                        .contains(&query.trim().to_lowercase())
                                })
                        })
                })
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
        self.leader_pending = false;
        self.command_menu = false;
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

    fn cursor_byte(&self) -> usize {
        self.draft
            .grapheme_indices(true)
            .nth(self.draft_cursor)
            .map_or(self.draft.len(), |(index, _)| index)
    }

    pub fn draft_cursor_byte(&self) -> usize {
        self.cursor_byte()
    }

    fn insert_draft(&mut self, character: char) {
        let byte = self.cursor_byte();
        self.draft.insert(byte, character);
        let insertion_byte = byte + character.len_utf8();
        self.draft_cursor = self
            .draft
            .grapheme_indices(true)
            .take_while(|(index, _)| *index < insertion_byte)
            .count();
        if insertion_byte == self.draft.len() {
            self.draft_cursor = self.draft.graphemes(true).count();
        }
    }

    fn move_draft_cursor(&mut self, delta: isize) {
        self.draft_cursor = (self.draft_cursor as isize + delta)
            .clamp(0, self.draft.graphemes(true).count() as isize)
            as usize;
    }

    fn delete_before_cursor(&mut self) {
        let cursor = self.cursor_byte();
        if cursor == 0 {
            return;
        }
        let start = self.draft[..cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        self.draft.drain(start..cursor);
        self.draft_cursor = self.draft[..start].graphemes(true).count();
    }

    fn delete_at_cursor(&mut self) {
        let byte = self.cursor_byte();
        if let Some(grapheme) = self.draft[byte..].graphemes(true).next() {
            self.draft.drain(byte..byte + grapheme.len());
            self.draft_cursor = self.draft[..byte].graphemes(true).count();
        }
    }

    fn update_command_menu(&mut self) {
        self.command_menu = self.draft.trim_start().starts_with('/') && !self.draft.contains('\n');
        self.command_selection = 0;
    }

    pub fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let token = self.draft.split_whitespace().next().unwrap_or("/");
        [
            ("/help", "Show command help"),
            ("/theme", "Choose system, dark, or light theme"),
            ("/settings", "Open local display settings"),
            ("/nickname", "Name this direct chat locally"),
            ("/new", "Start a new direct chat"),
            ("/filter", "Filter your conversations"),
            ("/compact", "Toggle sidebar density"),
            ("/cancel", "Close the composer"),
        ]
        .into_iter()
        .filter(|(name, _)| name.starts_with(token) || token == *name)
        .collect()
    }

    fn complete_command(&mut self) {
        let suggestions = self.command_suggestions();
        let Some((name, _)) = suggestions.get(self.command_selection).copied() else {
            return;
        };
        let leading = self.draft.len() - self.draft.trim_start().len();
        let command = &self.draft[leading..];
        let token_end = command.find(char::is_whitespace).unwrap_or(command.len());
        let old_cursor = self.cursor_byte();
        let has_suffix = token_end < command.len();
        let shift = name.len() as isize - token_end as isize;
        self.draft.replace_range(leading..leading + token_end, name);
        let new_cursor = if old_cursor <= leading + token_end {
            leading + name.len()
        } else {
            (old_cursor as isize + shift).max(0) as usize
        }
        .min(self.draft.len());
        self.draft_cursor = self.draft[..new_cursor].graphemes(true).count();
        if !has_suffix {
            self.draft.push(' ');
            self.draft_cursor = self.draft.graphemes(true).count();
        }
        self.command_menu = false;
    }

    fn execute_nickname_command(&mut self) {
        let nickname = self
            .draft
            .strip_prefix("/nickname")
            .unwrap_or_default()
            .trim();
        let Some(conversation) = self.selected_conversation() else {
            self.error = Some("Select a direct chat before setting a nickname.".into());
            return;
        };
        if conversation.kind != ConversationKind::Direct {
            self.error = Some("Nicknames are available in direct chats only.".into());
            return;
        }
        let Some(participant) = conversation
            .participants
            .iter()
            .find(|participant| !participant.is_me)
        else {
            self.error = Some("This direct chat has no other participant to name.".into());
            return;
        };
        let key = nickname_key(&participant.handle);
        if nickname.eq_ignore_ascii_case("clear") {
            self.nicknames.remove(&key);
            self.error = None;
            self.notice = Some("Local nickname cleared.".into());
        } else if !crate::config::valid_nickname(nickname) {
            self.error = Some("Usage: /nickname <name> or /nickname clear".into());
            return;
        } else {
            self.nicknames.insert(key, nickname.to_owned());
            self.error = None;
            self.notice = Some(format!("Local nickname set to {nickname}."));
        }
        self.cancel_composition();
    }

    fn execute_command(&mut self) {
        let words: Vec<_> = self.draft.split_whitespace().collect();
        self.command_menu = false;
        match words.as_slice() {
            ["/help"] => {
                self.error =
                    Some("Commands: /help, /theme [auto|dark|light], /settings, /nickname <name|clear>, /new, /filter, /compact, /cancel".into());
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/theme"] => {
                self.toggle_theme();
                self.error = None;
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/theme", "dark"] => {
                self.set_theme_override(Some(Theme::Dark));
                self.error = None;
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/theme", "light"] => {
                self.set_theme_override(Some(Theme::Light));
                self.error = None;
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/theme", "auto"] => {
                self.set_theme_override(None);
                self.error = None;
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/settings"] => {
                self.error = None;
                self.open_settings();
            }
            ["/nickname", ..] => self.execute_nickname_command(),
            ["/new"] => {
                self.error = None;
                self.begin_new_conversation();
            }
            ["/filter"] => {
                self.error = None;
                self.cancel_composition();
                self.begin_filter();
            }
            ["/compact"] => {
                self.compact_sidebar = !self.compact_sidebar;
                self.error = None;
                self.cancel_composition();
            }
            ["/cancel"] => self.cancel_composition(),
            _ => self.error = Some("Unknown command or invalid syntax".into()),
        }
    }

    /// Apply a key press and report what the shell should do about it.
    ///
    /// Ctrl+C always quits. Otherwise the open prompt owns the keyboard: while the
    /// filter is open its characters are query text, and while composing they are
    /// message text, so `q` only quits from the plain conversation list.
    pub fn begin_send(&mut self, conversation: Option<ConversationId>) {
        self.error = None;
        self.notice = None;
        self.send_feedback = Some(SendFeedback {
            conversation,
            phase: SendPhase::Sending,
        });
    }

    pub fn finish_send(
        &mut self,
        conversation: Option<ConversationId>,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                self.error = None;
                self.notice = None;
                self.send_feedback = Some(SendFeedback {
                    conversation,
                    phase: SendPhase::Sent,
                });
            }
            Err(error) => {
                self.send_feedback = None;
                self.error = Some(error);
            }
        }
    }

    pub fn visible_send_feedback(&self) -> Option<&SendFeedback> {
        self.send_feedback.as_ref().filter(|feedback| {
            feedback
                .conversation
                .as_ref()
                .is_none_or(|conversation| Some(conversation) == self.selected.as_ref())
        })
    }

    pub fn set_theme_override(&mut self, theme: Option<Theme>) {
        self.theme_override = theme;
        self.theme = theme.unwrap_or(self.system_theme);
    }

    pub fn toggle_theme(&mut self) {
        let mut theme = self.theme;
        theme.toggle();
        self.set_theme_override(Some(theme));
    }

    fn toggle_setting(&mut self) {
        match self.settings_selection {
            0 => {
                let next = match self.theme_override {
                    None => Some(Theme::Dark),
                    Some(Theme::Dark) => Some(Theme::Light),
                    Some(Theme::Light) => None,
                };
                self.set_theme_override(next);
            }
            1 => self.compact_sidebar = !self.compact_sidebar,
            _ => {}
        }
    }

    fn open_settings(&mut self) {
        self.leader_pending = false;
        self.settings_open = true;
        self.settings_selection = 0;
        self.command_menu = false;
        self.composing = false;
        self.new_conversation = false;
        self.pending_address = None;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        self.notice = None;
        if self
            .send_feedback
            .as_ref()
            .is_some_and(|feedback| feedback.phase == SendPhase::Sent)
        {
            self.send_feedback = None;
        }
        if self.leader_pending {
            self.leader_pending = false;
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('h') => self.select_step(-1),
                KeyCode::Char('j') => self.scroll_forward(SCROLL_PAGE),
                KeyCode::Char('k') => self.scroll_back(SCROLL_PAGE),
                KeyCode::Char('l') => self.select_step(1),
                KeyCode::Char('n') => self.begin_new_conversation(),
                KeyCode::Char('f') => self.begin_filter(),
                KeyCode::Char('t') => self.toggle_theme(),
                _ => {}
            }
            return Action::None;
        }
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if !self.settings_open && self.filter_query().is_none() && !self.composing {
                self.leader_pending = true;
            }
            return Action::None;
        }
        if self.settings_open {
            match key.code {
                KeyCode::Esc => self.settings_open = false,
                KeyCode::Up => self.settings_selection = self.settings_selection.saturating_sub(1),
                KeyCode::Down => self.settings_selection = (self.settings_selection + 1).min(1),
                KeyCode::Enter | KeyCode::Char(' ') => self.toggle_setting(),
                KeyCode::Char('t') => self.toggle_theme(),
                _ => {}
            }
            return Action::None;
        }
        if self.filter_query().is_some() {
            match key.code {
                KeyCode::Esc => self.cancel_filter(),
                KeyCode::Enter => {
                    if let Some(id) = self.confirm_filter() {
                        self.select(Some(id));
                    }
                }
                KeyCode::Up => self.select_step(-1),
                KeyCode::Down => self.select_step(1),
                KeyCode::Backspace => self.filter_backspace(),
                KeyCode::Char(character) => self.filter_input(character),
                _ => {}
            }
            return Action::None;
        }
        if self.composing {
            if self.command_menu {
                match key.code {
                    KeyCode::Esc => self.command_menu = false,
                    KeyCode::Up => {
                        self.command_selection = self.command_selection.saturating_sub(1)
                    }
                    KeyCode::Down => {
                        let last = self.command_suggestions().len().saturating_sub(1);
                        self.command_selection = self.command_selection.saturating_add(1).min(last);
                    }
                    KeyCode::Tab => self.complete_command(),
                    KeyCode::Enter => {
                        let token = self.draft.split_whitespace().next().unwrap_or("");
                        let exact = self
                            .command_suggestions()
                            .iter()
                            .any(|(name, _)| *name == token);
                        if exact || self.command_suggestions().is_empty() {
                            self.execute_command();
                        } else {
                            self.complete_command();
                        }
                    }
                    KeyCode::Backspace => {
                        self.delete_before_cursor();
                        self.update_command_menu();
                    }
                    KeyCode::Delete => {
                        self.delete_at_cursor();
                        self.update_command_menu();
                    }
                    KeyCode::Left => self.move_draft_cursor(-1),
                    KeyCode::Right => self.move_draft_cursor(1),
                    KeyCode::Home => self.draft_cursor = 0,
                    KeyCode::End => self.draft_cursor = self.draft.graphemes(true).count(),
                    KeyCode::Char(character) => {
                        self.insert_draft(character);
                        self.update_command_menu();
                    }
                    _ => {}
                }
                return Action::None;
            }
            match key.code {
                KeyCode::Esc => {
                    self.cancel_composition();
                    return Action::None;
                }
                KeyCode::Enter => {
                    if self
                        .draft
                        .split_whitespace()
                        .next()
                        .is_some_and(|token| token.starts_with('/'))
                    {
                        self.execute_command();
                        return Action::None;
                    }
                    let action = match self.submit_intent() {
                        CompositionIntent::None => Action::None,
                        CompositionIntent::SendMessage(body) => {
                            match (self.selected.clone(), body.is_empty()) {
                                (Some(conversation), false) => {
                                    self.draft.clear();
                                    Action::SendMessage { conversation, body }
                                }
                                _ => Action::None,
                            }
                        }
                        CompositionIntent::StartConversation { address, body } => {
                            if address.is_empty() || body.is_empty() {
                                Action::None
                            } else {
                                self.draft.clear();
                                Action::StartConversation { address, body }
                            }
                        }
                    };
                    // `submit_intent` owns the composer state: it closes the composer
                    // when a message is sent, and deliberately keeps it open after the
                    // recipient step so the body can be typed without pressing Enter
                    // again, which is what the prompt then asks for.
                    return action;
                }
                KeyCode::Backspace => {
                    self.delete_before_cursor();
                    self.update_command_menu();
                }
                KeyCode::Delete => self.delete_at_cursor(),
                KeyCode::Left => self.move_draft_cursor(-1),
                KeyCode::Right => self.move_draft_cursor(1),
                KeyCode::Home => self.draft_cursor = 0,
                KeyCode::End => self.draft_cursor = self.draft.graphemes(true).count(),
                KeyCode::Char(character) => {
                    self.insert_draft(character);
                    self.update_command_menu();
                }
                _ => {}
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Down => self.select_step(1),
            KeyCode::Up => self.select_step(-1),
            // Reading back through a thread, a screenful at a time.
            KeyCode::PageUp => self.scroll_back(SCROLL_PAGE),
            KeyCode::PageDown => self.scroll_forward(SCROLL_PAGE),
            KeyCode::Home => self.scroll_to_oldest(),
            KeyCode::End => self.scroll_to_newest(),
            KeyCode::Char('t') => self.toggle_theme(),
            KeyCode::Char('n') => self.begin_new_conversation(),
            KeyCode::Char('/') => self.begin_filter(),
            KeyCode::Enter => self.composing = true,
            _ => {}
        }
        Action::None
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

    pub fn conversation_name(&self, conversation: &Conversation) -> String {
        if conversation.kind == ConversationKind::Direct {
            if let Some(nickname) = conversation
                .participants
                .iter()
                .find(|participant| !participant.is_me)
                .and_then(|participant| self.nicknames.get(&nickname_key(&participant.handle)))
            {
                return nickname.clone();
            }
        }
        chat_name(conversation).to_owned()
    }

    pub fn speaker_label_for(
        &self,
        message: &Message,
        conversation: Option<&Conversation>,
    ) -> String {
        if !message.from_me {
            if let Some(nickname) = self.nicknames.get(&nickname_key(&message.sender)) {
                return nickname.clone();
            }
        }
        speaker_label(message, conversation)
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
                // Reading back through a thread should not be interrupted by a new
                // message arriving in it; the user is pinned back to the newest.
                if self.selected.as_ref() == Some(&conversation_id) {
                    self.scroll = 0;
                }
                // A message for a conversation we do not know about means our list is
                // stale: ask for a refresh rather than caching it invisibly.
                if !self
                    .conversations
                    .iter()
                    .any(|conversation| conversation.id == conversation_id)
                {
                    self.resync_conversations = true;
                }
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

/// Stable local-only key for a participant nickname. The value is never sent to a transport.
pub fn nickname_key(handle: &Handle) -> String {
    let kind = match handle.kind() {
        HandleKind::Tel => "tel",
        HandleKind::Email => "email",
        HandleKind::Opaque => "opaque",
    };
    format!("{kind}:{}", handle.value())
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
    let mut tm = unsafe { std::mem::zeroed() };
    let local = unsafe { libc::localtime_r(&seconds, &mut tm) };
    if local.is_null() {
        return "--:--".into();
    }
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
    Some(match session {
        "RenewalDue" => "Renewal due".to_owned(),
        _ => session.to_owned(),
    })
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
        return if matches!(
            app.connection_state,
            crate::daemon::ConnectionState::Connecting
                | crate::daemon::ConnectionState::Disconnected
        ) {
            StatusTone::Attention
        } else {
            StatusTone::Failure
        };
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

    fn draft_key(app: &mut App, code: KeyCode) {
        assert_eq!(
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
            Action::None
        );
    }

    fn draft_state(app: &App, text: &str, cursor: usize, byte: usize) {
        assert_eq!(app.draft, text);
        assert_eq!(app.draft_cursor, cursor);
        assert_eq!(app.draft_cursor_byte(), byte);
        assert!(app.draft_cursor <= app.draft.graphemes(true).count());
    }

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
    fn draft_ascii_editing_tracks_grapheme_and_byte_positions() {
        let mut app = App {
            composing: true,
            ..Default::default()
        };
        for ch in "abc".chars() {
            draft_key(&mut app, KeyCode::Char(ch));
        }
        draft_state(&app, "abc", 3, 3);
        draft_key(&mut app, KeyCode::Left);
        draft_state(&app, "abc", 2, 2);
        draft_key(&mut app, KeyCode::Home);
        draft_state(&app, "abc", 0, 0);
        draft_key(&mut app, KeyCode::Left);
        draft_state(&app, "abc", 0, 0);
        draft_key(&mut app, KeyCode::Right);
        draft_state(&app, "abc", 1, 1);
        draft_key(&mut app, KeyCode::End);
        draft_state(&app, "abc", 3, 3);
        draft_key(&mut app, KeyCode::Right);
        draft_state(&app, "abc", 3, 3);
        draft_key(&mut app, KeyCode::Backspace);
        draft_state(&app, "ab", 2, 2);
        draft_key(&mut app, KeyCode::Home);
        draft_key(&mut app, KeyCode::Backspace);
        draft_state(&app, "ab", 0, 0);
        draft_key(&mut app, KeyCode::Home);
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "b", 0, 0);

        app.draft = "abc".into();
        app.draft_cursor = 0;
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "bc", 0, 0);
        draft_key(&mut app, KeyCode::Right);
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "b", 1, 1);
        draft_key(&mut app, KeyCode::End);
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "b", 1, 1);
    }

    #[test]
    fn draft_edits_use_extended_grapheme_clusters() {
        let family = "👨‍👩‍👧‍👦";
        let combined = "e\u{301}";
        let mut app = App {
            composing: true,
            ..Default::default()
        };
        for ch in combined.chars() {
            draft_key(&mut app, KeyCode::Char(ch));
        }
        draft_state(&app, combined, 1, combined.len());
        draft_key(&mut app, KeyCode::Home);
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "", 0, 0);
        for ch in family.chars() {
            draft_key(&mut app, KeyCode::Char(ch));
        }
        draft_state(&app, family, 1, family.len());
        draft_key(&mut app, KeyCode::Home);
        draft_key(&mut app, KeyCode::Right);
        draft_key(&mut app, KeyCode::Backspace);
        draft_state(&app, "", 0, 0);
    }

    #[test]
    fn draft_cursor_stays_valid_when_insertions_or_deletions_merge_graphemes() {
        let mut app = App {
            composing: true,
            draft: "ab".into(),
            draft_cursor: 1,
            ..Default::default()
        };
        draft_key(&mut app, KeyCode::Char('\u{301}'));
        draft_state(&app, "a\u{301}b", 1, "a\u{301}".len());

        app.draft = "a\u{301}b".into();
        app.draft_cursor = 1;
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "a\u{301}", 1, "a\u{301}".len());

        app.draft = "a\u{301}bc".into();
        app.draft_cursor = 1;
        draft_key(&mut app, KeyCode::Delete);
        draft_state(&app, "a\u{301}c", 1, "a\u{301}".len());
    }

    #[test]
    fn animation_triggers_and_idle_state() {
        let mut app = App {
            connection_state: ConnectionState::Connected,
            ..Default::default()
        };
        assert!(!app.is_animating());
        app.connection_state = ConnectionState::Connecting;
        assert!(app.is_animating());
        app.connection_state = ConnectionState::Disconnected;
        assert!(app.is_animating());
        app.connection_state = ConnectionState::Connected;
        app.session = "Starting".into();
        assert!(app.is_animating());
        app.session.clear();
        app.thread_loading = true;
        assert!(app.is_animating());
    }

    #[test]
    fn helper_text_and_timestamp_formatting() {
        let chat = conversation("a", None);
        assert_eq!(chat_name(&chat), "Conversation");
        assert_eq!(participant_count(1), "1 person");
        assert_eq!(participant_count(3), "3 people");
        assert_eq!(timestamp(0).len(), 5);
        assert!(timestamp(0).contains(':'));
        for value in [0, i64::MAX, -1] {
            assert_eq!(timestamp(value).len(), 5);
        }
        assert_eq!(timestamp(0), timestamp(0));
        assert_ne!(timestamp(0), timestamp(60_000));
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

        // An automatic reconnect deserves attention without alarming like a hard failure.
        let offline = App {
            connection: "Disconnected, retrying".into(),
            connection_state: crate::daemon::ConnectionState::Disconnected,
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_tone(&offline), StatusTone::Attention);
    }

    #[test]
    fn header_status_prefers_connection_trouble_over_the_session() {
        let mut app = App {
            connection: "Connected".into(),
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_text(&app).as_deref(), Some("Active"));
        app.session = "RenewalDue".into();
        assert_eq!(status_text(&app).as_deref(), Some("Renewal due"));
        app.session = "Active".into();

        // A broken daemon connection is more important than the last known session.
        app.connection = "Disconnected, retrying".into();
        assert_eq!(status_text(&app).as_deref(), Some("Disconnected, retrying"));

        // Nothing worth showing yields nothing.
        app.connection = "Connected".into();
        app.session = "  ".into();
        assert_eq!(status_text(&app), None);
    }

    #[test]
    fn a_message_for_an_unknown_conversation_asks_for_a_refresh() {
        // Sending the first message of a new conversation produces exactly this: the
        // daemon creates the conversation, but our sidebar has never seen it.
        let mut app = App::default();
        let known = conversation("Known", Some(10));
        app.set_conversations(vec![known.clone()]);
        app.apply_event(EventKind::Message {
            conversation_id: known.id.clone(),
            message: message(&known, "m1", 20, "known thread"),
        });
        assert!(
            !app.resync_conversations,
            "a known conversation must not force a refresh"
        );

        let unknown = conversation("Brand new", Some(30));
        app.apply_event(EventKind::Message {
            conversation_id: unknown.id.clone(),
            message: message(&unknown, "m2", 40, "first message"),
        });
        assert!(
            app.resync_conversations,
            "an unknown conversation must trigger a refresh so it becomes visible"
        );
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
