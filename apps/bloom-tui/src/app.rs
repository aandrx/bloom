//! UI state for the Bloom TUI. Rendering lives in `ui.rs`.

use crate::composer::wrap_draft;
use crate::daemon::{ConnectionState, Snapshot};
use crate::ui::{Hit, HitMap};
use bloom_domain::{
    Conversation, ConversationId, ConversationKind, Handle, HandleKind, Message, MessageId,
    Participant,
};
use bloom_ipc::{ConversationSyncStatus, EventKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

/// Rows a single PageUp or PageDown moves through the transcript.
const SCROLL_PAGE: usize = 10;

pub const HELP_ROW_COUNT: usize = 16;

/// What a key press wants the shell to do.
///
/// The key semantics live here rather than inside the event loop so the real key
/// path is testable; the loop only performs the async work an action asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    SendMessage {
        attempt: u64,
        conversation: ConversationId,
        body: String,
    },
    StartConversation {
        attempt: u64,
        address: String,
        body: String,
    },
    SyncConversations,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPhase {
    Sending,
    Sent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDraft {
    pub attempt: u64,
    pub target: Option<ConversationId>,
    pub origin: Option<ConversationId>,
    pub origin_focus: Focus,
    pub address: Option<String>,
    pub body: String,
    pub uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFailure {
    pub message: String,
    pub uncertain: bool,
}

impl From<bloom_ipc::client::ClientError> for SendFailure {
    fn from(error: bloom_ipc::client::ClientError) -> Self {
        Self {
            message: crate::diagnostics::safe_reason(&error).into(),
            uncertain: crate::diagnostics::is_uncertain(&error),
        }
    }
}

impl From<&str> for SendFailure {
    fn from(message: &str) -> Self {
        Self {
            message: message.into(),
            uncertain: false,
        }
    }
}

/// An accepted new-chat send awaiting its correlated message and chat metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingStartNavigation {
    message_id: MessageId,
    origin: Option<ConversationId>,
    origin_focus: Focus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFeedback {
    pub conversation: Option<ConversationId>,
    pub phase: SendPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiSuggestion {
    pub shortcode: String,
    pub emoji: String,
    pub name: String,
}

fn sanitize_paste(input: &str) -> String {
    let normalized = input
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    normalized
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

fn emoji_catalog() -> &'static [EmojiSuggestion] {
    static CATALOG: OnceLock<Vec<EmojiSuggestion>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        emojis::iter()
            .flat_map(|emoji| {
                emoji.shortcodes().map(move |shortcode| EmojiSuggestion {
                    shortcode: shortcode.to_lowercase(),
                    emoji: emoji.as_str().to_owned(),
                    name: emoji.name().to_owned(),
                })
            })
            .collect()
    })
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

/// Which editor a composer buffer belongs to, so switching chats or starting a new
/// conversation cannot silently retarget someone else's draft.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DraftOwner {
    Conversation(ConversationId),
    NewChat(Option<String>),
}

#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedDraft {
    text: String,
    cursor: usize,
    pasted_body: bool,
}

#[derive(Debug, Clone)]
pub struct SuspendedComposition {
    saved: SavedDraft,
    owner: Option<DraftOwner>,
}

pub struct App {
    pub theme: Theme,
    /// Terminal-detected theme used when the preference is automatic.
    pub system_theme: Theme,
    /// None means follow the terminal theme; Some pins a user preference.
    pub theme_override: Option<Theme>,
    pub compact_sidebar: bool,
    pub wrap_composer: bool,
    /// Current composer text and prompt widths, refreshed by the terminal loop.
    pub composer_columns: usize,
    pub composer_prefix_columns: usize,
    pub settings_open: bool,
    pub settings_category: usize,
    pub settings_selection: usize,
    pub theme_panel_open: bool,
    pub theme_panel_selection: usize,
    pub contacts_open: bool,
    pub contacts_selection: usize,
    pub contacts_tab: usize,
    pub contacts_query: String,
    pub contacts_editing: Option<String>,
    pub new_contact_selection: usize,
    pub new_contact_tab: usize,
    pub filter_category: usize,
    pub emoji_selection: usize,
    pub contacts_return_focus: Focus,
    pub conversations: Vec<Conversation>,
    pub selected: Option<ConversationId>,
    pub messages: HashMap<ConversationId, Vec<Message>>,
    /// Reject out-of-order snapshots and keep their history/list components ordered
    /// independently from live events.
    pub latest_snapshot_revision: u64,
    pub latest_conversations_revision: u64,
    pub latest_live_revision: HashMap<ConversationId, u64>,
    pub connection: String,
    pub connection_state: ConnectionState,
    pub frame: u64,
    pub thread_loading: bool,
    pub session: String,
    /// True while inbound activity indicates the daemon is catching up on messages.
    pub syncing: bool,
    pub draft: String,
    /// Pasted body text is literal even when it starts with a command prefix.
    pub pasted_body: bool,
    /// Extended-grapheme insertion position in the draft.
    pub draft_cursor: usize,
    #[doc(hidden)]
    pub draft_vertical_column: Option<usize>,
    /// Session-only drafts keyed by the recipient or new-chat flow they belong to.
    #[doc(hidden)]
    pub drafts: HashMap<DraftOwner, SavedDraft>,
    /// The owner of the buffer currently in `draft`; `None` while a scratch modal owns it.
    #[doc(hidden)]
    pub active_draft_owner: Option<DraftOwner>,
    #[doc(hidden)]
    pub suspended_composition: Option<SuspendedComposition>,
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
    pub sync_requested: bool,
    pub sync_in_flight: bool,
    pub send_feedback: Option<SendFeedback>,
    pub recoverable_send: Option<RecoveryDraft>,
    pub deferred_sends: VecDeque<RecoveryDraft>,
    pub next_send_attempt: u64,
    pub pending_send_attempt: Option<u64>,
    pub pending_start_navigation: Option<PendingStartNavigation>,
    /// Local-only nickname overrides, keyed by stable participant handle.
    pub nicknames: HashMap<String, String>,
    /// Filterable command menu is only active while composing.
    pub command_menu: bool,
    pub command_selection: usize,
    pub leader_pending: bool,
    /// Which region owns the arrow keys and the wheel. The composer is the
    /// default, so scrolling scrolls the transcript, the way Codex behaves.
    pub focus: Focus,
    /// The `?` shortcut sheet.
    pub help_open: bool,
    pub help_scroll: usize,
    /// Local titles, keyed by canonical conversation identity. Beats a nickname
    /// and the engine's title, and is what the sidebar's Rename action writes.
    pub titles: HashMap<String, String>,
    /// Conversations closed locally. They stay in the store and can be restored
    /// with `/unhide`; closing never deletes messages.
    pub hidden: HashSet<String>,
    /// The right-click menu for a conversation, when open.
    pub context_menu: Option<ContextMenu>,
    /// The conversation whose local title the composer is editing.
    pub renaming: Option<ConversationId>,
}

/// A small right-click menu over a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMenu {
    pub conversation: ConversationId,
    pub selection: usize,
}

/// The actions the conversation context menu offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextAction {
    Rename,
    Close,
}

impl ContextMenu {
    pub const ACTIONS: [(&'static str, ContextAction); 2] = [
        ("Rename", ContextAction::Rename),
        ("Close", ContextAction::Close),
    ];
}

/// The two focusable regions. Settings, the filter and the help sheet suspend
/// both while they are open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Composer,
    Sidebar,
}

impl Default for App {
    /// The composer starts active, the way Codex opens with its prompt focused:
    /// you can type immediately, and the arrow keys scroll the transcript.
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            system_theme: Theme::default(),
            theme_override: None,
            compact_sidebar: false,
            wrap_composer: true,
            composer_columns: 72,
            composer_prefix_columns: 3,
            settings_open: false,
            settings_category: 0,
            settings_selection: 0,
            theme_panel_open: false,
            theme_panel_selection: 0,
            contacts_open: false,
            contacts_selection: 0,
            contacts_tab: 0,
            contacts_query: String::new(),
            contacts_editing: None,
            new_contact_selection: 0,
            new_contact_tab: 0,
            filter_category: 0,
            emoji_selection: 0,
            contacts_return_focus: Focus::default(),
            conversations: Vec::new(),
            selected: None,
            messages: HashMap::new(),
            latest_snapshot_revision: 0,
            latest_conversations_revision: 0,
            latest_live_revision: HashMap::new(),
            connection: String::new(),
            connection_state: ConnectionState::default(),
            frame: 0,
            thread_loading: false,
            session: String::new(),
            syncing: false,
            draft: String::new(),
            pasted_body: false,
            draft_cursor: 0,
            draft_vertical_column: None,
            drafts: HashMap::new(),
            active_draft_owner: None,
            suspended_composition: None,
            scroll: 0,
            resync_conversations: false,
            filter: None,
            composing: true,
            pending_address: None,
            new_conversation: false,
            error: None,
            notice: None,
            sync_requested: false,
            sync_in_flight: false,
            send_feedback: None,
            recoverable_send: None,
            deferred_sends: VecDeque::new(),
            next_send_attempt: 1,
            pending_send_attempt: None,
            pending_start_navigation: None,
            nicknames: HashMap::new(),
            command_menu: false,
            command_selection: 0,
            leader_pending: false,
            focus: Focus::default(),
            help_open: false,
            help_scroll: 0,
            titles: HashMap::new(),
            hidden: HashSet::new(),
            context_menu: None,
            renaming: None,
        }
    }
}

impl App {
    pub fn is_animating(&self) -> bool {
        matches!(
            self.connection_state,
            ConnectionState::Connecting | ConnectionState::Disconnected
        ) || !matches!(self.session.as_str(), "" | "Active")
            || self.syncing
            || self.sync_in_flight
            || self.thread_loading
            || self.messages.values().flatten().any(|message| {
                message.from_me && matches!(message.delivery, bloom_domain::DeliveryState::Queued)
            })
    }

    pub fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Mark whether a catch-up burst is in flight, so the header can show it.
    pub fn set_syncing(&mut self, syncing: bool) {
        self.syncing = syncing;
    }

    /// Apply a store snapshot only if its read began after our newest state.
    /// History is then replaced, not merged, because the store is authoritative.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        if snapshot.revision <= self.latest_snapshot_revision {
            return;
        }
        self.latest_snapshot_revision = snapshot.revision;
        if let Some(id) = snapshot.visible.clone() {
            let latest_live = self
                .latest_live_revision
                .get(&id)
                .copied()
                .unwrap_or_default();
            if snapshot.revision > latest_live {
                self.messages.insert(id.clone(), snapshot.history);
                // Adopt the snapshot's conversation only when nothing is selected yet;
                // a deliberate user selection must not be pulled back. Route through
                // `select` so the draft buffer is attributed to the right chat.
                if self.selected.is_none() {
                    self.select(Some(id));
                }
            }
        }
        if snapshot.revision > self.latest_conversations_revision {
            self.latest_conversations_revision = snapshot.revision;
            self.set_conversations(snapshot.conversations);
        }
        self.resolve_completed_start();
    }

    pub fn set_conversations(&mut self, mut conversations: Vec<Conversation>) {
        if let Some(selected) = self.selected.clone() {
            if let Some(canonical) = conversations
                .iter()
                .find(|conversation| conversation.aliases.contains(&selected))
            {
                // An identity alias must carry the open draft with it, not strand it
                // under the retired row.
                let canonical_id = canonical.id.clone();
                self.migrate_draft_owner(&selected, &canonical_id);
                self.select(Some(canonical_id));
            }
        }
        conversations.sort_by_key(|c| std::cmp::Reverse(c.last_message_at));
        self.conversations = conversations;
        if self.filter.is_some() {
            self.reconcile_selection_with_visible();
        }
        self.contacts_selection = self
            .contacts_selection
            .min(self.visible_contact_handles().len().saturating_sub(1));
        if self.filter.is_none()
            && !self
                .selected
                .as_ref()
                .is_some_and(|id| self.conversations.iter().any(|c| &c.id == id))
        {
            self.select(self.visible_ids().first().cloned());
        }
        self.resolve_completed_start();
    }

    /// Move the selection. A reading position belongs to the thread it was made in,
    /// so opening a different thread returns to that thread's newest message.
    pub fn select(&mut self, id: Option<ConversationId>) {
        if self.selected != id {
            self.scroll = 0;
            self.send_feedback = None;
            if self.suspended_composition.is_none() {
                let next = if self.new_conversation {
                    Some(DraftOwner::NewChat(self.pending_address.clone()))
                } else {
                    id.clone().map(DraftOwner::Conversation)
                };
                self.retarget_draft(next);
            }
        }
        self.selected = id;
    }

    /// The owner the composer buffer belongs to in the current state, or `None`
    /// while a scratch editor (rename, nickname, filter query) owns it.
    fn composer_owner(&self) -> Option<DraftOwner> {
        if self.renaming.is_some()
            || self.contacts_open
            || self.settings_open
            || self.theme_panel_open
        {
            return None;
        }
        if self.new_conversation {
            Some(DraftOwner::NewChat(self.pending_address.clone()))
        } else {
            self.selected.clone().map(DraftOwner::Conversation)
        }
    }

    /// Stash the buffer under its current owner and load the buffer for `next`.
    /// Empty buffers retire their owner, so a cleared draft cannot resurrect.
    fn retarget_draft(&mut self, next: Option<DraftOwner>) {
        if next == self.active_draft_owner {
            return;
        }
        if let Some(owner) = self.active_draft_owner.take() {
            if self.draft.is_empty() {
                self.drafts.remove(&owner);
            } else {
                self.drafts.insert(
                    owner,
                    SavedDraft {
                        text: self.draft.clone(),
                        cursor: self.draft_cursor,
                        pasted_body: self.pasted_body,
                    },
                );
            }
        }
        self.active_draft_owner = next.clone();
        let saved = next
            .and_then(|owner| self.drafts.remove(&owner))
            .unwrap_or_default();
        self.draft = saved.text;
        self.draft_cursor = saved.cursor.min(self.draft.graphemes(true).count());
        self.pasted_body = saved.pasted_body;
        self.draft_vertical_column = None;
        self.command_menu = false;
        self.update_command_menu();
    }

    /// Retire the active buffer and its saved copy, e.g. after a message is sent.
    fn retire_active_draft(&mut self) {
        if let Some(owner) = self.active_draft_owner.clone() {
            self.drafts.remove(&owner);
        }
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_vertical_column = None;
        self.command_menu = false;
    }

    /// Move any saved or active draft across an identity alias so it is not lost.
    fn migrate_draft_owner(&mut self, from: &ConversationId, to: &ConversationId) {
        let from_owner = DraftOwner::Conversation(from.clone());
        let to_owner = DraftOwner::Conversation(to.clone());
        if let Some(saved) = self.drafts.remove(&from_owner) {
            self.drafts.entry(to_owner.clone()).or_insert(saved);
        }
        if self.active_draft_owner.as_ref() == Some(&from_owner) {
            self.active_draft_owner = Some(to_owner.clone());
        }
        if let Some(snapshot) = self.suspended_composition.as_mut() {
            if snapshot.owner.as_ref() == Some(&from_owner) {
                snapshot.owner = Some(to_owner);
            }
        }
    }

    /// Restore only the selected identity's draft. A modal never owns message text.
    fn resume_composer_draft(&mut self) {
        if let Some(snapshot) = self.suspended_composition.take() {
            if let Some(owner) = snapshot.owner {
                if !snapshot.saved.text.is_empty() {
                    self.drafts.entry(owner).or_insert(snapshot.saved);
                }
            }
        }
        self.retarget_draft(self.composer_owner());
    }

    fn suspend_composer_draft(&mut self) {
        // Switching between transient panels must not replace the saved body with
        // a nickname or another panel's scratch input.
        if self.suspended_composition.is_some() {
            return;
        }
        let owner = self
            .composer_owner()
            .or_else(|| self.active_draft_owner.take());
        self.active_draft_owner = None;
        self.suspended_composition = Some(SuspendedComposition {
            owner,
            saved: SavedDraft {
                text: std::mem::take(&mut self.draft),
                cursor: self.draft_cursor,
                pasted_body: self.pasted_body,
            },
        });
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.command_menu = false;
    }

    fn close_preferences(&mut self) {
        self.settings_open = false;
        self.theme_panel_open = false;
        self.composing = true;
        self.resume_composer_draft();
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
    pub fn contact_handles(&self) -> Vec<Handle> {
        let mut handles = HashMap::<String, Handle>::new();
        for participant in self
            .conversations
            .iter()
            .flat_map(|conversation| &conversation.participants)
            .filter(|participant| !participant.is_me)
        {
            handles
                .entry(nickname_key(&participant.handle))
                .or_insert_with(|| participant.handle.clone());
        }
        let mut handles: Vec<_> = handles.into_values().collect();
        handles.sort_by_key(|handle| {
            (
                self.contact_label(handle).to_lowercase(),
                nickname_key(handle),
            )
        });
        handles
    }

    pub fn contact_label(&self, handle: &Handle) -> String {
        self.nicknames
            .get(&nickname_key(handle))
            .cloned()
            .unwrap_or_else(|| handle.value().to_owned())
    }

    pub fn visible_contact_handles(&self) -> Vec<Handle> {
        let query = self.contacts_query.to_lowercase();
        let phone_query = phone_search_digits(&self.contacts_query);
        self.contact_handles()
            .into_iter()
            .filter(|handle| {
                let nickname = self.nicknames.get(&nickname_key(handle));
                let matches_phone = phone_query.as_ref().is_some_and(|query| {
                    handle.kind() == &HandleKind::Tel
                        && phone_search_digits(handle.value())
                            .is_some_and(|number| number.contains(query))
                });
                (self.contacts_tab == 0 || nickname.is_some())
                    && (query.is_empty()
                        || handle.value().to_lowercase().contains(&query)
                        || nickname.is_some_and(|name| name.to_lowercase().contains(&query))
                        || matches_phone)
            })
            .collect()
    }

    fn update_contacts_query(&mut self, query: String) {
        let selected = self.selected_contact_handle();
        self.contacts_query = query;
        let visible = self.visible_contact_handles();
        self.contacts_selection = selected
            .and_then(|selected| visible.iter().position(|handle| handle == &selected))
            .unwrap_or(0);
    }

    fn erase_last_contact_query_grapheme(&mut self) {
        let mut query = self.contacts_query.clone();
        if let Some((start, _)) = query.grapheme_indices(true).next_back() {
            query.truncate(start);
            self.update_contacts_query(query);
        }
    }

    pub fn selected_contact_handle(&self) -> Option<Handle> {
        self.visible_contact_handles()
            .get(self.contacts_selection)
            .cloned()
    }

    fn contact_last_activity(&self, handle: &Handle) -> i64 {
        let key = nickname_key(handle);
        self.conversations
            .iter()
            .filter(|conversation| {
                conversation
                    .participants
                    .iter()
                    .any(|participant| nickname_key(&participant.handle) == key)
            })
            .map(|conversation| {
                conversation
                    .last_message_at
                    .unwrap_or(conversation.created_at)
            })
            .max()
            .unwrap_or(i64::MIN)
    }

    pub fn new_contact_suggestions(&self) -> Vec<Handle> {
        let mut handles = self.contact_handles();
        if self.new_contact_tab == 0 {
            handles.sort_by(|left, right| {
                self.contact_last_activity(right)
                    .cmp(&self.contact_last_activity(left))
                    .then_with(|| self.contact_label(left).cmp(&self.contact_label(right)))
            });
            handles.truncate(6);
        }
        let query = self.draft.trim().to_lowercase();
        if query.is_empty() {
            return handles;
        }
        handles
            .into_iter()
            .filter(|handle| {
                let value = handle.value().to_lowercase();
                let address = match handle.kind() {
                    HandleKind::Email => format!("mailto:{value}"),
                    HandleKind::Tel => format!("tel:{value}"),
                    HandleKind::Opaque => value.clone(),
                };
                self.contact_label(handle).to_lowercase().contains(&query)
                    || value.contains(&query)
                    || address.contains(&query)
            })
            .collect()
    }

    fn emoji_token_range(&self) -> Option<(usize, usize, String)> {
        if !self.composing
            || self.command_menu
            || self.contacts_open
            || self.settings_open
            || self.theme_panel_open
            || self.filter_query().is_some()
            || (self.new_conversation && self.pending_address.is_none())
        {
            return None;
        }
        let cursor = self.cursor_byte();
        let before = &self.draft[..cursor];
        let (start, query_end, end) = if before.ends_with(':') {
            let close = cursor.saturating_sub(1);
            if let Some(start) = before[..close].rfind(':') {
                (start, close, cursor)
            } else {
                (close, cursor, cursor)
            }
        } else {
            let start = before.rfind(':')?;
            let mut query_end = cursor;
            for (offset, character) in self.draft[cursor..].char_indices() {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '+' | '-') {
                    query_end = cursor + offset + character.len_utf8();
                } else {
                    break;
                }
            }
            let mut end = query_end;
            if self.draft[end..].starts_with(':') {
                end += 1;
            }
            (start, query_end, end)
        };
        let previous = self.draft[..start].chars().next_back();
        if previous.is_some_and(|character| character.is_alphanumeric() || character == '_') {
            return None;
        }
        let query = &self.draft[start + 1..query_end];
        if query.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '+' | '-'))
        }) {
            return None;
        }
        Some((start, end.min(self.draft.len()), query.to_lowercase()))
    }

    pub fn emoji_suggestions(&self) -> Vec<EmojiSuggestion> {
        let Some((_, _, query)) = self.emoji_token_range() else {
            return Vec::new();
        };
        let mut matches = emoji_catalog()
            .iter()
            .filter_map(|suggestion| {
                let rank = if query.is_empty() || suggestion.shortcode == query {
                    0
                } else if suggestion.shortcode.starts_with(&query) {
                    1
                } else {
                    return None;
                };
                Some((rank, suggestion.shortcode.len(), suggestion))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.shortcode.cmp(&right.2.shortcode))
        });
        let mut seen = HashSet::new();
        matches
            .into_iter()
            .filter(|(_, _, suggestion)| seen.insert(suggestion.emoji.clone()))
            .map(|(_, _, suggestion)| suggestion.clone())
            .take(8)
            .collect()
    }

    fn complete_emoji_suggestion(&mut self) -> bool {
        let Some((start, end, _)) = self.emoji_token_range() else {
            return false;
        };
        let Some(suggestion) = self.emoji_suggestions().get(self.emoji_selection).cloned() else {
            return false;
        };
        self.draft.replace_range(start..end, &suggestion.emoji);
        self.draft_cursor = self.draft[..start + suggestion.emoji.len()]
            .graphemes(true)
            .count();
        self.emoji_selection = 0;
        self.update_command_menu();
        true
    }

    pub fn begin_contacts(&mut self) {
        self.contacts_return_focus = self.focus;
        self.suspend_composer_draft();
        self.contacts_open = true;
        self.settings_open = false;
        self.theme_panel_open = false;
        self.help_open = false;
        self.contacts_selection = 0;
        self.contacts_tab = 0;
        self.contacts_query.clear();
        self.contacts_editing = None;
        self.command_menu = false;
        self.focus = Focus::Composer;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
        self.error = None;
    }

    pub fn close_contacts(&mut self) {
        self.contacts_open = false;
        self.contacts_query.clear();
        self.contacts_editing = None;
        self.composing = true;
        self.draft.clear();
        self.draft_cursor = 0;
        self.resume_composer_draft();
        self.focus = self.contacts_return_focus;
    }

    fn contacts_step(&mut self, delta: isize) {
        let count = self.visible_contact_handles().len();
        if count > 0 {
            self.contacts_selection =
                (self.contacts_selection as isize + delta).clamp(0, count as isize - 1) as usize;
        } else {
            self.contacts_selection = 0;
        }
    }

    fn begin_contact_edit(&mut self) {
        let Some(handle) = self.selected_contact_handle() else {
            self.error = Some("No contacts are available yet.".into());
            return;
        };
        let key = nickname_key(&handle);
        self.draft = self.nicknames.get(&key).cloned().unwrap_or_default();
        self.draft_cursor = self.draft.graphemes(true).count();
        self.contacts_editing = Some(key);
        self.composing = true;
        self.error = None;
    }

    fn cancel_contact_edit(&mut self) {
        self.contacts_editing = None;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
        self.error = None;
    }

    fn save_contact_edit(&mut self) {
        let Some(key) = self.contacts_editing.clone() else {
            return;
        };
        let nickname = self.draft.trim();
        if nickname.is_empty() || nickname.eq_ignore_ascii_case("clear") {
            self.nicknames.remove(&key);
            self.notice = Some("Local nickname cleared.".into());
        } else if !crate::config::valid_nickname(nickname) {
            self.error = Some("Nickname must be 1 to 80 printable characters.".into());
            return;
        } else {
            self.nicknames.insert(key.clone(), nickname.to_owned());
            self.notice = Some("Local nickname saved.".into());
        }
        if let Some(index) = self
            .visible_contact_handles()
            .iter()
            .position(|handle| nickname_key(handle) == key)
        {
            self.contacts_selection = index;
        }
        self.contacts_editing = None;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
        self.error = None;
    }

    pub fn begin_new_conversation(&mut self) {
        self.close_preferences();
        if self.contacts_open {
            self.close_contacts();
        }
        self.retarget_draft(None);
        self.leader_pending = false;
        self.settings_open = false;
        self.contacts_open = false;
        self.theme_panel_open = false;
        self.new_contact_selection = 0;
        self.new_contact_tab = 0;
        self.command_menu = false;
        self.focus = Focus::Composer;
        self.new_conversation = true;
        self.pending_address = None;
        self.composing = true;
        // The entered command is not a message draft; discard it before the new-chat buffer loads.
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.resume_composer_draft();
    }

    /// Abandon a draft or the new-conversation flow. The composer stays active,
    /// because it is always active; `Esc` should not take typing away.
    pub fn cancel_composition(&mut self) {
        self.command_menu = false;
        self.theme_panel_open = false;
        self.new_conversation = false;
        self.pending_address = None;
        self.composing = true;
        // A cancelled new-chat scratch is discarded, then the selected chat's draft returns.
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.resume_composer_draft();
    }

    /// Advance the composer one step. Address entry does not send anything; it
    /// moves on to the body so a new conversation cannot be started with a
    /// placeholder message.
    pub fn submit_intent(&mut self) -> CompositionIntent {
        if self.pending_send_attempt.is_some() {
            return CompositionIntent::None;
        }
        let value = if self.new_conversation && self.pending_address.is_none() {
            self.draft.trim().to_owned()
        } else {
            self.draft.clone()
        };
        self.draft.clear();
        self.draft_cursor = 0;
        if self.new_conversation && self.pending_address.is_none() {
            if value.is_empty() {
                self.cancel_composition();
                return CompositionIntent::None;
            }
            self.pending_address = Some(value);
            self.retarget_draft(self.composer_owner());
            self.composing = true;
            return CompositionIntent::None;
        }
        self.composing = true;
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
        let filtering = self.filter.is_some();
        let category = self.filter_category;
        let listed = self.conversations.iter().filter(|conversation| {
            !self.conversation_is_hidden(conversation)
                && (!filtering
                    || match category {
                        1 => conversation.kind == ConversationKind::Direct,
                        2 => conversation.kind == ConversationKind::Group,
                        _ => true,
                    })
        });
        match self.filter_query() {
            None => listed.collect(),
            Some(query) if query.trim().is_empty() => listed.collect(),
            Some(query) => listed
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

    fn conversation_is_hidden(&self, conversation: &Conversation) -> bool {
        std::iter::once(&conversation.id)
            .chain(conversation.aliases.iter())
            .any(|id| self.hidden.contains(&conversation_key(id)))
    }

    /// The filter query, or None when the filter is closed.
    pub fn filter_query(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    fn step_filter_category(&mut self, delta: isize) {
        self.filter_category = (self.filter_category as isize + delta).rem_euclid(3) as usize;
        self.reconcile_selection_with_visible();
    }

    fn step_contacts_tab(&mut self, delta: isize) {
        self.contacts_tab = (self.contacts_tab as isize + delta).rem_euclid(2) as usize;
        self.contacts_selection = 0;
    }

    fn step_new_contact_tab(&mut self, delta: isize) {
        self.new_contact_tab = (self.new_contact_tab as isize + delta).rem_euclid(2) as usize;
        self.new_contact_selection = 0;
    }

    fn new_contact_step(&mut self, delta: isize) {
        let count = self.new_contact_suggestions().len();
        if count > 0 {
            self.new_contact_selection =
                (self.new_contact_selection as isize + delta).clamp(0, count as isize - 1) as usize;
        } else {
            self.new_contact_selection = 0;
        }
    }

    fn select_new_contact(&mut self) -> bool {
        let suggestions = self.new_contact_suggestions();
        let Some(handle) = suggestions.get(self.new_contact_selection).cloned() else {
            return false;
        };
        self.pending_address = Some(match handle.kind() {
            HandleKind::Email => format!("mailto:{}", handle.value()),
            HandleKind::Tel => format!("tel:{}", handle.value()),
            HandleKind::Opaque => handle.value().to_owned(),
        });
        self.new_conversation = true;
        self.draft.clear();
        self.draft_cursor = 0;
        self.retarget_draft(self.composer_owner());
        self.composing = true;
        true
    }

    /// True when a query is narrowing the list and nothing matches it.
    pub fn filter_has_no_matches(&self) -> bool {
        matches!(self.filter_query(), Some(query) if !query.trim().is_empty() || self.filter_category != 0)
            && self.visible_conversations().is_empty()
    }

    /// Open the filter without touching an in-progress draft, so `/` cannot lose
    /// what the user was typing.
    pub fn begin_filter(&mut self) {
        self.close_preferences();
        if self.contacts_open {
            self.close_contacts();
        }
        if self.new_conversation {
            self.retarget_draft(None);
        }
        self.leader_pending = false;
        self.settings_open = false;
        self.contacts_open = false;
        self.theme_panel_open = false;
        self.command_menu = false;
        self.filter_category = 0;
        self.filter = Some(String::new());
        self.focus = Focus::Composer;
        self.composing = false;
        self.new_conversation = false;
        self.pending_address = None;
        self.resume_composer_draft();
    }

    pub fn cancel_filter(&mut self) {
        self.filter = None;
        self.filter_category = 0;
        self.composing = true;
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
        self.filter_category = 0;
        self.composing = true;
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

    /// The byte index where the previous word starts: trailing whitespace is
    /// skipped first, then the run of non-whitespace before the cursor.
    fn word_start_before(&self, cursor: usize) -> usize {
        let before = &self.draft[..cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        start
    }

    fn word_end_after(&self, cursor: usize) -> usize {
        let after = &self.draft[cursor..];
        let mut end = cursor;
        let mut seen_word = false;
        for (index, character) in after.char_indices() {
            if character.is_whitespace() {
                if seen_word {
                    break;
                }
            } else {
                seen_word = true;
                end = cursor + index + character.len_utf8();
            }
        }
        end
    }

    fn set_draft_cursor_byte(&mut self, byte: usize) {
        let byte = byte.min(self.draft.len());
        self.draft_cursor = self.draft[..byte].graphemes(true).count();
    }

    /// Delete from the cursor back to the start of the previous word (`Ctrl+W`,
    /// `Alt+Backspace`), the gesture a terminal user expects.
    fn delete_word_before_cursor(&mut self) {
        let cursor = self.cursor_byte();
        if cursor == 0 {
            return;
        }
        let start = self.word_start_before(cursor);
        self.draft.drain(start..cursor);
        self.set_draft_cursor_byte(start);
    }

    fn move_word(&mut self, forward: bool) {
        let cursor = self.cursor_byte();
        if forward {
            let end = self.word_end_after(cursor);
            self.set_draft_cursor_byte(end);
        } else {
            let start = self.word_start_before(cursor);
            self.set_draft_cursor_byte(start);
        }
    }

    fn clear_before_cursor(&mut self) {
        let cursor = self.cursor_byte();
        self.draft.drain(..cursor);
        self.draft_cursor = 0;
    }

    fn kill_to_end(&mut self) {
        let cursor = self.cursor_byte();
        self.draft.truncate(cursor);
    }

    fn move_draft_vertical(&mut self, direction: isize) -> bool {
        if (!self.wrap_composer && !self.draft.contains('\n'))
            || self.draft.is_empty()
            || self.composer_columns == 0
        {
            return false;
        }
        let prefix_width = self
            .composer_prefix_columns
            .min(self.composer_columns.saturating_sub(1));
        let wrapped = wrap_draft(
            &" ".repeat(prefix_width),
            &self.draft,
            self.composer_columns,
        );
        let cursor_index = self
            .draft_cursor
            .min(wrapped.positions.len().saturating_sub(1));
        let (line, column) = wrapped.positions[cursor_index];
        let target_line = if direction < 0 {
            line.checked_sub(1)
        } else {
            line.checked_add(1)
                .filter(|target| *target < wrapped.lines.len())
        };
        let Some(target_line) = target_line else {
            return false;
        };
        let preferred_column = *self.draft_vertical_column.get_or_insert(column);
        let target_cursor = wrapped
            .positions
            .iter()
            .enumerate()
            .filter(|(_, (row, _))| *row == target_line)
            .min_by_key(|(_, (_, target_column))| target_column.abs_diff(preferred_column))
            .map(|(index, _)| index);
        if let Some(target_cursor) = target_cursor {
            self.draft_cursor = target_cursor;
            true
        } else {
            false
        }
    }

    /// Apply a terminal-style editing gesture to the draft. Returns true when the
    /// key was an edit, so callers can refresh derived state.
    fn edit_draft(&mut self, key: KeyEvent) -> bool {
        self.draft_vertical_column = None;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Backspace if control || alt => self.delete_word_before_cursor(),
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Left if control || alt => self.move_word(false),
            KeyCode::Right if control || alt => self.move_word(true),
            KeyCode::Left => self.move_draft_cursor(-1),
            KeyCode::Right => self.move_draft_cursor(1),
            KeyCode::Home => self.draft_cursor = 0,
            KeyCode::End => self.draft_cursor = self.draft.graphemes(true).count(),
            KeyCode::Char('a') if control => self.draft_cursor = 0,
            KeyCode::Char('e') if control => self.draft_cursor = self.draft.graphemes(true).count(),
            KeyCode::Char('w') if control || alt => self.delete_word_before_cursor(),
            KeyCode::Char('u') if control => self.clear_before_cursor(),
            KeyCode::Char('k') if control => self.kill_to_end(),
            KeyCode::Char(character) if !control && !alt => self.insert_draft(character),
            _ => return false,
        }
        let draft_changed = matches!(
            key.code,
            KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('w' | 'u' | 'k')
        ) || (matches!(key.code, KeyCode::Char(_)) && !control && !alt);
        if draft_changed {
            self.update_command_menu();
            self.emoji_selection = 0;
        }
        true
    }

    fn update_command_menu(&mut self) {
        // The popup picks a command; once arguments follow it, the draft is a
        // command line and Enter must run it rather than autofill again.
        let trimmed = self.draft.trim_start();
        if self.draft.is_empty() {
            self.pasted_body = false;
        }
        self.command_menu = !self.pasted_body
            && !self.new_conversation
            && trimmed.starts_with('/')
            && !trimmed.contains(char::is_whitespace);
        if self.command_menu {
            self.command_selection = 0;
        }
    }

    pub fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        let token = self.draft.split_whitespace().next().unwrap_or("/");
        [
            ("/help", "Show command help"),
            ("/sync", "Refresh conversation list metadata"),
            ("/theme", "Choose system, dark, or light theme"),
            ("/settings", "Open local display settings"),
            ("/nickname", "Name this direct chat locally"),
            ("/contacts", "Browse known participants and nicknames"),
            ("/new", "Start a new direct chat"),
            ("/filter", "Filter your conversations"),
            ("/compact", "Toggle sidebar density"),
            ("/unhide", "Restore chats hidden with Close"),
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
        let has_arguments = !command[token_end..].trim().is_empty();
        let shift = name.len() as isize - token_end as isize;
        self.draft.replace_range(leading..leading + token_end, name);
        let new_cursor = if old_cursor <= leading + token_end {
            leading + name.len()
        } else {
            (old_cursor as isize + shift).max(0) as usize
        }
        .min(self.draft.len());
        self.draft_cursor = self.draft[..new_cursor].graphemes(true).count();
        if name == "/theme" && !has_arguments {
            self.retire_active_draft();
            self.open_theme_panel();
        } else if !has_suffix {
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
            // `/help` opens the same popup the composer shows, rather than
            // printing a wall of text as an error.
            ["/help"] => {
                self.error = None;
                self.draft = "/".to_owned();
                self.draft_cursor = 1;
                self.command_menu = true;
                self.command_selection = 0;
            }
            ["/theme"] => {
                self.error = None;
                self.retire_active_draft();
                self.open_theme_panel();
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
            ["/sync"] => {
                self.error = None;
                self.notice = Some("Syncing conversation-list metadata...".into());
                self.draft.clear();
                self.draft_cursor = 0;
                self.cancel_composition();
                self.sync_requested = true;
            }
            ["/settings"] => {
                self.error = None;
                self.retire_active_draft();
                self.open_settings();
            }
            ["/contacts"] => {
                self.retire_active_draft();
                self.begin_contacts();
            }
            ["/nickname", ..] => self.execute_nickname_command(),
            ["/new"] => {
                self.error = None;
                self.retire_active_draft();
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
            ["/unhide"] => {
                self.unhide_all();
                self.error = None;
                self.draft.clear();
                self.draft_cursor = 0;
            }
            ["/cancel"] => self.cancel_composition(),
            _ => self.error = Some("Unknown command or invalid syntax".into()),
        }
    }

    fn take_pending_action(&mut self) -> Action {
        if std::mem::take(&mut self.sync_requested) && !self.sync_in_flight {
            self.sync_in_flight = true;
            Action::SyncConversations
        } else {
            Action::None
        }
    }

    /// Apply a key press and report what the shell should do about it.
    ///
    /// Ctrl+C clears nonempty composer or filter text first, then quits when the
    /// input line is already empty. Prompt contents remain ordinary text, so `q`
    /// only quits from the plain conversation list.
    pub fn begin_send(&mut self, attempt: u64, conversation: Option<ConversationId>) {
        if self.pending_send_attempt.is_some() {
            return;
        }
        self.pending_start_navigation = None;
        self.pending_send_attempt = Some(attempt);
        self.error = None;
        self.notice = None;
        self.send_feedback = Some(SendFeedback {
            conversation,
            phase: SendPhase::Sending,
        });
    }

    /// Preserve the daemon's accepted message identity for safe new-chat navigation.
    pub fn finish_send_receipt(
        &mut self,
        attempt: u64,
        conversation: Option<ConversationId>,
        result: Result<MessageId, SendFailure>,
    ) {
        if self.pending_send_attempt != Some(attempt) {
            return;
        }
        let navigation = if conversation.is_none() {
            result.as_ref().ok().and_then(|message_id| {
                self.recoverable_send
                    .as_ref()
                    .filter(|draft| draft.attempt == attempt && draft.target.is_none())
                    .map(|draft| PendingStartNavigation {
                        message_id: message_id.clone(),
                        origin: draft.origin.clone(),
                        origin_focus: draft.origin_focus,
                    })
            })
        } else {
            None
        };
        self.finish_send(attempt, conversation, result.map(|_| ()));
        self.pending_start_navigation = navigation;
        self.resolve_completed_start();
    }

    fn resolve_completed_start(&mut self) {
        let Some(navigation) = self.pending_start_navigation.clone() else {
            return;
        };
        if !self.draft.is_empty()
            || !self.composing
            || self.focus != navigation.origin_focus
            || !self.recovery_prompt_available()
            || self.pending_send_attempt.is_some()
        {
            self.pending_start_navigation = None;
            return;
        }
        let targets: HashSet<_> = self
            .messages
            .values()
            .flatten()
            .filter(|message| message.from_me && message.id() == &navigation.message_id)
            .filter_map(|message| {
                self.conversations
                    .iter()
                    .find(|chat| {
                        &chat.id == message.conversation_id()
                            || chat.aliases.contains(message.conversation_id())
                    })
                    .map(|chat| chat.id.clone())
            })
            .collect();
        if targets.len() != 1 {
            return;
        }
        let target = targets.into_iter().next().expect("one correlated target");
        let original_selected = self.selected == navigation.origin
            || self.conversations.iter().any(|chat| {
                self.selected.as_ref() == Some(&chat.id)
                    && navigation
                        .origin
                        .as_ref()
                        .is_some_and(|origin| chat.aliases.contains(origin))
            });
        self.pending_start_navigation = None;
        if !original_selected && self.selected.as_ref() != Some(&target) {
            return;
        }
        self.select(Some(target.clone()));
        self.send_feedback = Some(SendFeedback {
            conversation: Some(target),
            phase: SendPhase::Sent,
        });
    }

    pub fn finish_send(
        &mut self,
        attempt: u64,
        conversation: Option<ConversationId>,
        result: Result<(), SendFailure>,
    ) {
        if self.pending_send_attempt != Some(attempt) {
            return;
        }
        self.pending_send_attempt = None;
        let recovery = self.recoverable_send.take();
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
                let mut message = error.message;
                if let Some(mut recovery) = recovery.filter(|r| r.attempt == attempt) {
                    recovery.uncertain = error.uncertain;
                    if recovery.target == conversation
                        && self.selected == recovery.origin
                        && self.focus == recovery.origin_focus
                        && self.composing
                        && self.recovery_prompt_available()
                        && self.draft.is_empty()
                    {
                        self.restore_send_draft(recovery);
                    } else {
                        self.deferred_sends.push_back(recovery);
                        message.push_str(" Draft saved; Ctrl+R recovers it in the original chat.");
                    }
                }
                self.error = Some(message);
            }
        }
    }

    fn recovery_prompt_available(&self) -> bool {
        !self.settings_open
            && !self.contacts_open
            && !self.theme_panel_open
            && !self.help_open
            && self.filter.is_none()
            && self.context_menu.is_none()
            && self.renaming.is_none()
            && self.contacts_editing.is_none()
            && !self.new_conversation
            && self.pending_address.is_none()
            && !self.command_menu
    }

    fn restore_send_draft(&mut self, recovery: RecoveryDraft) {
        self.draft = recovery.body;
        // A recovered body is always message data, even if it begins with '/'.
        self.pasted_body = true;
        self.draft_cursor = self.draft.graphemes(true).count();
        self.draft_vertical_column = None;
        self.focus = Focus::Composer;
        self.composing = true;
        self.pending_address = recovery.address;
        self.new_conversation = self.pending_address.is_some();
        self.error = None;
        self.notice = Some(
            if recovery.uncertain {
                "Outcome uncertain; verify the conversation before retrying."
            } else {
                "Recovered failed message."
            }
            .into(),
        );
    }

    pub fn finish_sync(&mut self, result: Result<(ConversationSyncStatus, u32), String>) {
        self.sync_requested = false;
        self.sync_in_flight = false;
        self.error = None;
        self.notice = None;
        match result {
            Ok((ConversationSyncStatus::Complete, count)) => {
                self.notice = Some(format!("Sync complete: {count} conversations listed."));
            }
            Ok((ConversationSyncStatus::Limited, count)) => {
                self.notice = Some(format!(
                    "Sync limited after {count} conversations; older chats may be missing."
                ));
            }
            Ok((ConversationSyncStatus::Failed, count)) => {
                self.error = Some(format!(
                    "Conversation-list sync failed after {count} conversations."
                ));
            }
            Err(message) => self.error = Some(message),
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
        match (self.settings_category, self.settings_selection) {
            (0, 0) => {
                let next = match self.theme_override {
                    None => Some(Theme::Dark),
                    Some(Theme::Dark) => Some(Theme::Light),
                    Some(Theme::Light) => None,
                };
                self.set_theme_override(next);
            }
            (1, 0) => self.compact_sidebar = !self.compact_sidebar,
            (1, 1) => self.wrap_composer = !self.wrap_composer,
            _ => {}
        }
    }

    fn step_settings_category(&mut self, step: isize) {
        self.settings_category = (self.settings_category as isize + step).rem_euclid(2) as usize;
        self.settings_selection = 0;
    }

    pub fn open_theme_panel(&mut self) {
        self.suspend_composer_draft();
        self.theme_panel_open = true;
        self.settings_open = false;
        self.contacts_open = false;
        self.theme_panel_selection = match self.theme_override {
            None => 0,
            Some(Theme::Dark) => 1,
            Some(Theme::Light) => 2,
        };
        self.command_menu = false;
        self.draft.clear();
        self.draft_cursor = 0;
        self.focus = Focus::Composer;
        self.composing = false;
    }

    pub fn open_settings(&mut self) {
        self.suspend_composer_draft();
        self.leader_pending = false;
        self.help_open = false;
        self.contacts_open = false;
        self.contacts_editing = None;
        self.theme_panel_open = false;
        self.settings_open = true;
        self.settings_category = 0;
        self.settings_selection = 0;
        self.command_menu = false;
        self.focus = Focus::Composer;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
    }

    /// Paste is data, never keystrokes or a command. Non-editable overlays own input.
    pub fn paste(&mut self, input: &str) {
        if self.context_menu.is_some()
            || self.help_open
            || self.settings_open
            || self.theme_panel_open
        {
            return;
        }
        let clean = sanitize_paste(input);
        if self.contacts_open && self.contacts_editing.is_none() {
            self.update_contacts_query(format!(
                "{}{}",
                self.contacts_query,
                clean.replace('\n', " ")
            ));
            return;
        }
        if let Some(query) = self.filter.as_mut() {
            query.push_str(&clean.replace('\n', " "));
            self.reconcile_selection_with_visible();
            return;
        }
        let single = self.renaming.is_some()
            || self.contacts_editing.is_some()
            || (self.new_conversation && self.pending_address.is_none());
        if single {
            self.insert_paste(&clean.replace('\n', " "), false);
        } else {
            self.focus = Focus::Composer;
            self.insert_paste(&clean, true);
        }
    }

    fn insert_paste(&mut self, text: &str, body: bool) {
        if text.is_empty() {
            return;
        }
        let byte = self
            .draft
            .grapheme_indices(true)
            .nth(self.draft_cursor)
            .map_or(self.draft.len(), |(at, _)| at);
        self.draft.insert_str(byte, text);
        let end = byte + text.len();
        self.draft_cursor = self
            .draft
            .grapheme_indices(true)
            .take_while(|(index, _)| *index < end)
            .count();
        self.draft_vertical_column = None;
        self.pasted_body |= body;
        self.composing = true;
        self.update_command_menu();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.draft.is_empty() {
            self.pasted_body = false;
        }
        let newline = (key.code == KeyCode::Char('j')
            && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Enter
                && key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT));
        if newline {
            let body_editor = self.composing
                && self.focus == Focus::Composer
                && !self.settings_open
                && !self.theme_panel_open
                && !self.contacts_open
                && !self.help_open
                && self.context_menu.is_none()
                && self.renaming.is_none()
                && self.filter.is_none()
                && !self.leader_pending
                && (!self.new_conversation || self.pending_address.is_some());
            if body_editor {
                self.insert_paste("\n", true);
            }
            return Action::None;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.pending_send_attempt.is_some() || !self.recovery_prompt_available() {
                return Action::None;
            }
            if let Some(recovery) = self.deferred_sends.front() {
                let refusal = if !self.draft.is_empty() {
                    Some("Clear the current draft before recovery.")
                } else if recovery
                    .target
                    .as_ref()
                    .is_some_and(|target| self.selected.as_ref() != Some(target))
                {
                    Some("Return to the original conversation before recovering this message.")
                } else {
                    None
                };
                if let Some(message) = refusal {
                    self.error = None;
                    self.notice = Some(message.into());
                } else {
                    let recovery = self.deferred_sends.pop_front().expect("checked recovery");
                    self.restore_send_draft(recovery);
                }
            }
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.filter_query().is_some() && !self.filter_query().unwrap_or_default().is_empty()
            {
                self.filter = Some(String::new());
                self.draft_cursor = 0;
                return Action::None;
            }
            if self.renaming.is_none()
                && self.contacts_editing.is_none()
                && (self.composing || self.command_menu)
                && !(self.settings_open
                    || self.theme_panel_open
                    || self.contacts_open
                    || self.help_open)
                && !self.draft.is_empty()
            {
                self.draft.clear();
                self.draft_cursor = 0;
                self.command_menu = false;
                return Action::None;
            }
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

        // The context menu owns the keyboard while it is open.
        if self.context_menu.is_some() {
            match key.code {
                KeyCode::Esc => self.close_context_menu(),
                KeyCode::Up => self.context_step(-1),
                KeyCode::Down => self.context_step(1),
                KeyCode::Enter => self.context_confirm(),
                _ => {}
            }
            return Action::None;
        }

        // The rename prompt owns the keyboard while it is open.
        if self.renaming.is_some() {
            match key.code {
                KeyCode::Esc => self.cancel_rename(),
                KeyCode::Enter => self.apply_rename(),
                _ => {
                    self.edit_draft(key);
                }
            }
            return Action::None;
        }

        if !self.leader_pending && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.focus = match self.focus {
                Focus::Composer => Focus::Sidebar,
                Focus::Sidebar => Focus::Composer,
            };
            return Action::None;
        }
        if !self.leader_pending
            && key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !self.contacts_open
            && !self.settings_open
            && !self.theme_panel_open
            && !self.help_open
        {
            self.select_step(-1);
            return Action::None;
        }
        if self.contacts_open {
            if self.contacts_editing.is_some() {
                match key.code {
                    KeyCode::Esc => self.cancel_contact_edit(),
                    KeyCode::Enter => self.save_contact_edit(),
                    _ => {
                        self.edit_draft(key);
                    }
                }
            } else {
                match key.code {
                    KeyCode::Esc => self.close_contacts(),
                    KeyCode::Enter => self.begin_contact_edit(),
                    KeyCode::Up => self.contacts_step(-1),
                    KeyCode::Down => self.contacts_step(1),
                    KeyCode::PageUp => self.contacts_step(-10),
                    KeyCode::PageDown => self.contacts_step(10),
                    KeyCode::Home => self.contacts_selection = 0,
                    KeyCode::End => {
                        self.contacts_selection =
                            self.visible_contact_handles().len().saturating_sub(1)
                    }
                    KeyCode::Left => self.step_contacts_tab(-1),
                    KeyCode::Right => self.step_contacts_tab(1),
                    KeyCode::Backspace => self.erase_last_contact_query_grapheme(),
                    KeyCode::Delete => self.update_contacts_query(String::new()),
                    KeyCode::Char(ch)
                        if !ch.is_control()
                            && !key.modifiers.intersects(
                                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                            ) =>
                    {
                        let mut query = self.contacts_query.clone();
                        query.push(ch);
                        self.update_contacts_query(query);
                    }
                    _ => {}
                }
            }
            return Action::None;
        }

        if self.focus == Focus::Sidebar
            && (self.contacts_open
                || self.settings_open
                || self.theme_panel_open
                || self.filter_query().is_some()
                || self.command_menu
                || (self.new_conversation && self.pending_address.is_none()))
        {
            match key.code {
                KeyCode::Backspace | KeyCode::Delete if self.command_menu => {
                    self.focus = Focus::Composer;
                    self.edit_draft(key);
                }
                KeyCode::Up => self.select_step(-1),
                KeyCode::Down => self.select_step(1),
                KeyCode::Enter => self.focus = Focus::Composer,
                KeyCode::Esc => {
                    if self.contacts_open {
                        self.close_contacts();
                    } else if self.settings_open || self.theme_panel_open {
                        self.close_preferences();
                    } else if self.filter_query().is_some() {
                        self.cancel_filter();
                    } else {
                        self.command_menu = false;
                    }
                }
                _ => {}
            }
            return Action::None;
        }

        // The shortcut sheet owns the keyboard while it is open.
        if self.help_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                    self.help_open = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = (self.help_scroll + 1).min(HELP_ROW_COUNT - 1);
                }
                KeyCode::PageUp | KeyCode::Home => self.help_scroll = 0,
                KeyCode::PageDown | KeyCode::End => self.help_scroll = HELP_ROW_COUNT - 1,
                _ => {}
            }
            return Action::None;
        }

        if self.leader_pending {
            self.leader_pending = false;
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('h') => self.select_step(-1),
                KeyCode::Char('j') => self.scroll_forward(SCROLL_PAGE),
                KeyCode::Char('k') => self.scroll_back(SCROLL_PAGE),
                KeyCode::Char('l') => self.select_step(1),
                KeyCode::Char('n') => self.select_step(1),
                KeyCode::Char('p') => self.select_step(-1),
                KeyCode::Char('f') => self.begin_filter(),
                KeyCode::Char('t') => self.toggle_theme(),
                KeyCode::Char('s') => self.open_settings(),
                _ => {}
            }
            return Action::None;
        }
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if !self.settings_open && !self.contacts_open && self.filter_query().is_none() {
                self.leader_pending = true;
            }
            return Action::None;
        }

        if self.theme_panel_open {
            match key.code {
                KeyCode::Esc => self.close_preferences(),
                KeyCode::Up => self.theme_panel_selection = (self.theme_panel_selection + 2) % 3,
                KeyCode::Down => self.theme_panel_selection = (self.theme_panel_selection + 1) % 3,
                KeyCode::Left => self.theme_panel_selection = (self.theme_panel_selection + 2) % 3,
                KeyCode::Right => self.theme_panel_selection = (self.theme_panel_selection + 1) % 3,
                KeyCode::Enter => {
                    self.set_theme_override(match self.theme_panel_selection {
                        0 => None,
                        1 => Some(Theme::Dark),
                        _ => Some(Theme::Light),
                    });
                    self.close_preferences();
                }
                _ => {}
            }
            return Action::None;
        }
        if self.settings_open {
            let row_count = if self.settings_category == 0 { 1 } else { 2 };
            match key.code {
                KeyCode::Esc => self.close_preferences(),
                KeyCode::Up => self.settings_selection = self.settings_selection.saturating_sub(1),
                KeyCode::Down => {
                    self.settings_selection = (self.settings_selection + 1).min(row_count - 1)
                }
                KeyCode::Left => self.step_settings_category(-1),
                KeyCode::Right => self.step_settings_category(1),
                KeyCode::Enter | KeyCode::Char(' ') => self.toggle_setting(),
                KeyCode::Char('t') => self.toggle_theme(),
                _ => {}
            }
            return Action::None;
        }

        if self.filter_query().is_some() {
            match key.code {
                KeyCode::Esc => self.cancel_filter(),
                KeyCode::Left => self.step_filter_category(-1),
                KeyCode::Right => self.step_filter_category(1),
                KeyCode::Enter => {
                    if let Some(id) = self.confirm_filter() {
                        self.select(Some(id));
                    }
                }
                KeyCode::Up => self.select_step(-1),
                KeyCode::Down => self.select_step(1),
                _ => {
                    if self.edit_filter(key) {
                        self.reconcile_selection_with_visible();
                    }
                }
            }
            return Action::None;
        }

        if self.command_menu && self.focus == Focus::Composer {
            match key.code {
                KeyCode::Esc => self.command_menu = false,
                KeyCode::Up => {
                    let count = self.command_suggestions().len();
                    if count > 0 {
                        self.command_selection = if self.command_selection == 0 {
                            count - 1
                        } else {
                            self.command_selection - 1
                        };
                    }
                }
                KeyCode::Down => {
                    let count = self.command_suggestions().len();
                    if count > 0 {
                        self.command_selection = (self.command_selection + 1) % count;
                    }
                }
                // Enter acts on the highlighted command: mode-opening commands run
                // immediately, argument-taking ones autofill for editing.
                KeyCode::Enter => {
                    self.activate_selected_command();
                    return self.take_pending_action();
                }
                _ => {
                    self.edit_draft(key);
                }
            }
            return Action::None;
        }

        // Keys that work no matter where the draft is. `?` and `,` only claim an
        // empty composer so they can still be typed as prose.
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Composer => Focus::Sidebar,
                    Focus::Sidebar => Focus::Composer,
                };
                return Action::None;
            }
            KeyCode::Char('?') if self.draft.is_empty() => {
                self.help_scroll = 0;
                self.help_open = true;
                return Action::None;
            }
            KeyCode::Char(',') if self.draft.is_empty() => {
                self.open_settings();
                return Action::None;
            }
            _ => {}
        }

        let emoji_count = self.emoji_suggestions().len();
        if emoji_count > 0 && self.focus == Focus::Composer {
            match key.code {
                KeyCode::Up => {
                    self.emoji_selection = (self.emoji_selection + emoji_count - 1) % emoji_count;
                    return Action::None;
                }
                KeyCode::Down => {
                    self.emoji_selection = (self.emoji_selection + 1) % emoji_count;
                    return Action::None;
                }
                KeyCode::Enter if self.complete_emoji_suggestion() => return Action::None,
                _ => {}
            }
        }

        if self.new_conversation && self.pending_address.is_none() && self.focus == Focus::Composer
        {
            match key.code {
                KeyCode::Up => self.new_contact_step(-1),
                KeyCode::Down => self.new_contact_step(1),
                KeyCode::Left => self.step_new_contact_tab(-1),
                KeyCode::Right => self.step_new_contact_tab(1),
                KeyCode::Enter if !self.new_contact_suggestions().is_empty() => {
                    self.select_new_contact();
                }
                _ => {}
            }
            if matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
            ) || (key.code == KeyCode::Enter && self.pending_address.is_some())
            {
                return Action::None;
            }
        }

        if self.composing {
            match key.code {
                KeyCode::Esc => {
                    self.cancel_composition();
                    return Action::None;
                }
                // In sidebar focus, Enter opens the highlighted chat and hands the
                // keyboard back to the composer.
                KeyCode::Enter if self.focus == Focus::Sidebar => {
                    self.focus = Focus::Composer;
                    return Action::None;
                }
                KeyCode::Enter => {
                    if !self.pasted_body
                        && !self.new_conversation
                        && self
                            .draft
                            .split_whitespace()
                            .next()
                            .is_some_and(|token| token.starts_with('/'))
                    {
                        self.execute_command();
                        return self.take_pending_action();
                    }
                    if self.draft.trim().is_empty() {
                        return Action::None;
                    }
                    let action = match self.submit_intent() {
                        CompositionIntent::None => Action::None,
                        CompositionIntent::SendMessage(body) => {
                            match (self.selected.clone(), body.is_empty()) {
                                (Some(conversation), false) => {
                                    self.retire_active_draft();
                                    let attempt = self.next_send_attempt;
                                    self.next_send_attempt += 1;
                                    self.recoverable_send = Some(RecoveryDraft {
                                        attempt,
                                        target: Some(conversation.clone()),
                                        origin: self.selected.clone(),
                                        origin_focus: self.focus,
                                        address: None,
                                        body: body.clone(),
                                        uncertain: false,
                                    });
                                    Action::SendMessage {
                                        attempt,
                                        conversation,
                                        body,
                                    }
                                }
                                _ => Action::None,
                            }
                        }
                        CompositionIntent::StartConversation { address, body } => {
                            if address.is_empty() || body.is_empty() {
                                Action::None
                            } else {
                                self.retire_active_draft();
                                let attempt = self.next_send_attempt;
                                self.next_send_attempt += 1;
                                self.recoverable_send = Some(RecoveryDraft {
                                    attempt,
                                    target: None,
                                    origin: self.selected.clone(),
                                    origin_focus: self.focus,
                                    address: Some(address.clone()),
                                    body: body.clone(),
                                    uncertain: false,
                                });
                                Action::StartConversation {
                                    attempt,
                                    address,
                                    body,
                                }
                            }
                        }
                    };
                    return action;
                }
                KeyCode::Up if self.focus == Focus::Sidebar => self.select_step(-1),
                KeyCode::Down if self.focus == Focus::Sidebar => self.select_step(1),
                // Wrapped drafts use vertical cursor motion first; at their edges,
                // Up/Down continue to scroll the transcript as usual.
                KeyCode::Up => {
                    if !self.move_draft_vertical(-1) {
                        self.scroll_back(1);
                    }
                }
                KeyCode::Down => {
                    if !self.move_draft_vertical(1) {
                        self.scroll_forward(1);
                    }
                }
                KeyCode::PageUp => self.scroll_back(SCROLL_PAGE),
                KeyCode::PageDown => self.scroll_forward(SCROLL_PAGE),
                // Home moves the draft cursor; End returns to the newest transcript
                // while preserving the draft cursor. Ctrl+E remains cursor-end.
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.scroll_to_oldest()
                }
                KeyCode::End => self.scroll_to_newest(),
                _ => {
                    // Typing always aims at the composer.
                    if matches!(key.code, KeyCode::Char(_)) {
                        self.focus = Focus::Composer;
                    }
                    self.edit_draft(key);
                }
            }
            return Action::None;
        }

        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Down => self.select_step(1),
            KeyCode::Up => self.select_step(-1),
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

    /// Resolve a mouse event against the last frame's hitboxes. The hitboxes come
    /// from rendering, so a click can only land on something that was drawn.
    pub fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        hits: &HitMap,
    ) -> Action {
        // A wheel notch moves a few rows, the way a terminal usually scrolls.
        const WHEEL_ROWS: usize = 3;
        if self.help_open {
            match kind {
                MouseEventKind::ScrollUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(WHEEL_ROWS);
                }
                MouseEventKind::ScrollDown => {
                    self.help_scroll = (self.help_scroll + WHEEL_ROWS).min(HELP_ROW_COUNT - 1);
                }
                MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {
                    // Dismiss the modal, but never dispatch this click to the pane
                    // underneath it.
                    self.help_open = false;
                }
                _ => {}
            }
            return Action::None;
        }
        match kind {
            MouseEventKind::ScrollUp => {
                if self.contacts_open {
                    self.contacts_step(-(WHEEL_ROWS as isize));
                } else if self.new_conversation && self.pending_address.is_none() {
                    self.new_contact_step(-(WHEEL_ROWS as isize));
                } else if self.filter_query().is_some() {
                    self.select_step(-(WHEEL_ROWS as isize));
                } else if matches!(hits.at(column, row), Some(Hit::SidebarChat(_))) {
                    self.select_step(-1);
                } else {
                    self.scroll_back(WHEEL_ROWS);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.contacts_open {
                    self.contacts_step(WHEEL_ROWS as isize);
                } else if self.new_conversation && self.pending_address.is_none() {
                    self.new_contact_step(WHEEL_ROWS as isize);
                } else if self.filter_query().is_some() {
                    self.select_step(WHEEL_ROWS as isize);
                } else if matches!(hits.at(column, row), Some(Hit::SidebarChat(_))) {
                    self.select_step(1);
                } else {
                    self.scroll_forward(WHEEL_ROWS);
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(Hit::SidebarChat(id)) = hits.at(column, row) {
                    self.select(Some(id.clone()));
                    self.open_context_menu(id);
                } else {
                    self.close_context_menu();
                }
            }
            MouseEventKind::Down(MouseButton::Left) => match hits.at(column, row) {
                Some(Hit::ContactRow(index)) if self.contacts_open => {
                    self.contacts_selection = index;
                }
                Some(Hit::ContactTab(index)) if self.contacts_open => {
                    self.contacts_tab = index.min(1);
                    self.contacts_selection = 0;
                }
                Some(Hit::NewContactRow(index))
                    if self.new_conversation && self.pending_address.is_none() =>
                {
                    self.new_contact_selection = index;
                }
                Some(Hit::NewContactTab(index))
                    if self.new_conversation && self.pending_address.is_none() =>
                {
                    self.new_contact_tab = index.min(1);
                    self.new_contact_selection = 0;
                }
                Some(Hit::FilterTab(index)) if self.filter_query().is_some() => {
                    self.filter_category = index.min(2);
                    self.reconcile_selection_with_visible();
                }
                Some(Hit::FilterRow(index)) if self.filter_query().is_some() => {
                    if let Some(conversation) = self.visible_conversations().get(index) {
                        self.select(Some(conversation.id.clone()));
                    }
                }
                Some(Hit::ContextMenuRow(index)) => {
                    if let Some(menu) = self.context_menu.as_mut() {
                        menu.selection = index;
                    }
                    self.context_confirm();
                }
                Some(Hit::SidebarChat(id)) => {
                    self.select(Some(id));
                    self.focus = Focus::Sidebar;
                    self.close_context_menu();
                }
                Some(Hit::Composer) => {
                    self.focus = Focus::Composer;
                    self.composing = true;
                    if self.filter_query().is_none() {
                        if let Some(index) = hits.composer_cursor_at(column, row) {
                            self.draft_cursor = index.min(self.draft.graphemes(true).count());
                            self.draft_vertical_column = None;
                        }
                    }
                }
                Some(Hit::MenuRow(index)) => self.activate_command(index),
                Some(Hit::SettingsTab(index)) => {
                    self.settings_category = index.min(1);
                    self.settings_selection = 0;
                }
                Some(Hit::SettingsRow(index)) => {
                    self.settings_selection = index;
                    self.toggle_setting();
                }
                Some(Hit::EmojiRow(index)) => {
                    self.emoji_selection = index;
                    self.complete_emoji_suggestion();
                }
                Some(Hit::ThemeRow(index)) if self.theme_panel_open => {
                    self.theme_panel_selection = index;
                    self.set_theme_override(match index {
                        0 => None,
                        1 => Some(Theme::Dark),
                        _ => Some(Theme::Light),
                    });
                    self.close_preferences();
                }
                Some(Hit::HeaderSettings) => self.open_settings(),
                Some(Hit::HeaderHelp) => {
                    if self.contacts_open {
                        self.close_contacts();
                    }
                    self.help_scroll = 0;
                    self.help_open = !self.help_open;
                }
                Some(Hit::CloseOverlay) => {
                    self.help_open = false;
                    self.command_menu = false;
                    if self.contacts_open {
                        self.close_contacts();
                    } else if self.settings_open || self.theme_panel_open {
                        self.close_preferences();
                    } else if self.new_conversation {
                        self.cancel_composition();
                    } else if self.filter_query().is_some() {
                        self.cancel_filter();
                    }
                }
                _ => {}
            },
            _ => {}
        }
        self.take_pending_action()
    }

    /// Run the command at a suggestion index, used by a click in the popup.
    fn activate_command(&mut self, index: usize) {
        let Some((name, _)) = self.command_suggestions().get(index).copied() else {
            return;
        };
        self.draft = name.to_owned();
        self.draft_cursor = self.draft.graphemes(true).count();
        self.execute_command();
    }

    /// Apply a terminal editing gesture to the filter query, so `Ctrl+W` and the
    /// rest behave the same there as in the composer.
    fn edit_filter(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(query) = self.filter.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Backspace if control || alt => {
                let trimmed = query.trim_end();
                let start = trimmed
                    .char_indices()
                    .rev()
                    .find(|(_, character)| character.is_whitespace())
                    .map_or(0, |(index, character)| index + character.len_utf8());
                query.truncate(start);
            }
            KeyCode::Backspace => {
                query.pop();
            }
            KeyCode::Char('w') if control || alt => {
                let trimmed = query.trim_end();
                let start = trimmed
                    .char_indices()
                    .rev()
                    .find(|(_, character)| character.is_whitespace())
                    .map_or(0, |(index, character)| index + character.len_utf8());
                query.truncate(start);
            }
            KeyCode::Char('u') if control => query.clear(),
            KeyCode::Char(character) if !control && !alt => query.push(character),
            _ => return false,
        }
        true
    }

    /// A stable local key for a conversation, used for titles and hidden ids.
    pub fn conversation_key(id: &ConversationId) -> String {
        conversation_key(id)
    }

    pub fn open_context_menu(&mut self, conversation: ConversationId) {
        self.context_menu = Some(ContextMenu {
            conversation,
            selection: 0,
        });
        self.focus = Focus::Sidebar;
    }

    pub fn close_context_menu(&mut self) {
        self.context_menu = None;
    }

    pub fn context_step(&mut self, delta: isize) {
        if let Some(menu) = self.context_menu.as_mut() {
            let last = ContextMenu::ACTIONS.len().saturating_sub(1) as isize;
            menu.selection = (menu.selection as isize + delta).clamp(0, last) as usize;
        }
    }

    pub fn context_confirm(&mut self) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        let index = menu
            .selection
            .min(ContextMenu::ACTIONS.len().saturating_sub(1));
        match ContextMenu::ACTIONS[index].1 {
            ContextAction::Rename => self.begin_rename(menu.conversation),
            ContextAction::Close => self.close_conversation(menu.conversation),
        }
    }

    /// Start editing a local title for a conversation, seeded with its current name.
    pub fn begin_rename(&mut self, id: ConversationId) {
        let current = self
            .conversations
            .iter()
            .find(|conversation| conversation.id == id)
            .map(|conversation| self.conversation_name(conversation))
            .unwrap_or_default();
        self.suspend_composer_draft();
        self.renaming = Some(id);
        self.draft = current;
        self.draft_cursor = self.draft.graphemes(true).count();
        self.command_menu = false;
        self.composing = true;
        self.focus = Focus::Composer;
    }

    pub fn cancel_rename(&mut self) {
        self.renaming = None;
        self.draft.clear();
        self.draft_cursor = 0;
        self.resume_composer_draft();
    }

    fn apply_rename(&mut self) {
        let Some(id) = self.renaming.take() else {
            return;
        };
        let name = self.draft.trim().to_owned();
        let aliases = self
            .conversations
            .iter()
            .find(|conversation| conversation.id == id)
            .map(|conversation| conversation.aliases.clone())
            .unwrap_or_default();
        self.draft.clear();
        self.draft_cursor = 0;
        self.resume_composer_draft();
        if name.is_empty() {
            self.titles.remove(&conversation_key(&id));
            for alias in aliases {
                self.titles.remove(&conversation_key(&alias));
            }
            self.notice = Some("Local title cleared.".into());
        } else if crate::config::valid_nickname(&name) {
            self.titles.insert(conversation_key(&id), name.clone());
            self.notice = Some(format!("Renamed to {name}."));
        } else {
            self.error = Some("Names must be 1-80 characters with no control characters.".into());
        }
    }

    /// Hide a conversation locally. The messages are untouched; `/unhide` restores
    /// everything at once, so this can never destroy data.
    pub fn close_conversation(&mut self, id: ConversationId) {
        self.hidden.insert(conversation_key(&id));
        if let Some(conversation) = self.conversations.iter().find(|row| row.id == id) {
            self.hidden
                .extend(conversation.aliases.iter().map(conversation_key));
        }
        let visible = self.visible_ids();
        if !visible
            .iter()
            .any(|candidate| Some(candidate) == self.selected.as_ref())
        {
            let first = visible.first().cloned();
            self.select(first);
            self.scroll = 0;
        }
        self.notice = Some("Chat hidden. /unhide restores hidden chats.".into());
    }

    pub fn unhide_all(&mut self) {
        let known_keys: HashSet<_> = self
            .conversations
            .iter()
            .flat_map(|conversation| {
                std::iter::once(conversation_key(&conversation.id))
                    .chain(conversation.aliases.iter().map(conversation_key))
            })
            .collect();
        let restored = self
            .conversations
            .iter()
            .filter(|conversation| self.conversation_is_hidden(conversation))
            .count()
            + self
                .hidden
                .iter()
                .filter(|key| !known_keys.contains(*key))
                .count();
        self.hidden.clear();
        self.notice = Some(match restored {
            0 => "No hidden chats.".into(),
            1 => "Restored 1 hidden chat.".into(),
            count => format!("Restored {count} hidden chats."),
        });
    }

    /// Commands that take an argument autofill into the composer; commands that
    /// open a mode run the moment they are chosen.
    fn command_needs_argument(name: &str) -> bool {
        matches!(name, "/nickname" | "/theme")
    }

    /// Run (or autofill) the highlighted command from the popup.
    fn activate_selected_command(&mut self) {
        let suggestions = self.command_suggestions();
        match suggestions.get(self.command_selection).copied() {
            Some((name, _)) if Self::command_needs_argument(name) => self.complete_command(),
            Some((name, _)) => {
                self.draft = name.to_owned();
                self.draft_cursor = name.graphemes(true).count();
                self.execute_command();
            }
            None => self.execute_command(),
        }
    }

    /// Keep the selection inside the filtered set as the query changes.
    fn reconcile_selection_with_visible(&mut self) {
        let visible = self.visible_ids();
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| visible.iter().any(|candidate| candidate == id))
        {
            self.select(visible.first().cloned());
        }
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations
            .iter()
            .find(|c| Some(&c.id) == self.selected.as_ref())
    }

    pub fn conversation_name(&self, conversation: &Conversation) -> String {
        // A local Rename wins over everything, including a participant nickname.
        if let Some(title) = std::iter::once(&conversation.id)
            .chain(conversation.aliases.iter())
            .find_map(|id| self.titles.get(&conversation_key(id)))
        {
            return title.clone();
        }
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
        chat_name(conversation)
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
        let revision = crate::daemon::next_revision();
        match &event {
            EventKind::Message {
                conversation_id, ..
            }
            | EventKind::Update {
                conversation_id, ..
            }
            | EventKind::Delivery {
                conversation_id, ..
            } => {
                self.latest_live_revision
                    .insert(conversation_id.clone(), revision);
                if matches!(&event, EventKind::Message { .. } | EventKind::Update { .. }) {
                    self.latest_conversations_revision = revision;
                }
            }
            EventKind::ConversationsChanged => {
                self.latest_conversations_revision = revision;
            }
            EventKind::Session { .. } => {}
        }
        match event {
            EventKind::Message {
                conversation_id,
                message,
                ..
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
        self.resolve_completed_start();
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
fn phone_search_digits(value: &str) -> Option<String> {
    let valid_number = value.chars().any(|character| character.is_ascii_digit())
        && value.chars().all(|character| {
            character.is_ascii_digit()
                || character == '+'
                || character.is_whitespace()
                || "().-".contains(character)
        });
    valid_number.then(|| {
        value
            .chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
    })
}

pub fn nickname_key(handle: &Handle) -> String {
    let kind = match handle.kind() {
        HandleKind::Tel => "tel",
        HandleKind::Email => "email",
        HandleKind::Opaque => "opaque",
    };
    let value = if handle.kind() == &HandleKind::Tel {
        let raw = handle.value();
        let valid = raw.chars().any(|character| character.is_ascii_digit())
            && raw.chars().all(|character| {
                character.is_ascii_digit()
                    || character == '+'
                    || character.is_whitespace()
                    || "().-".contains(character)
            });
        if valid {
            raw.chars()
                .filter(|character| !character.is_whitespace() && !"().-".contains(*character))
                .collect()
        } else {
            raw.to_owned()
        }
    } else {
        handle.value().to_owned()
    };
    format!("{kind}:{value}")
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

/// A stable key for local per-conversation state (titles, hidden chats). It does
/// not need to match the daemon's own key; it only has to be stable and unique.
pub fn conversation_key(id: &ConversationId) -> String {
    format!("{}|{}", id.transport().as_str(), id.value())
}

/// The people on the far side of a conversation, in the order the engine listed
/// them. `is_me` is excluded, so the result is who the chat is *with*.
pub fn others(conversation: &Conversation) -> impl Iterator<Item = &Participant> {
    conversation
        .participants
        .iter()
        .filter(|participant| !participant.is_me)
}

/// A person's label: their display name when the engine has one, otherwise the
/// handle itself. The engine usually has no display name, and a bare handle is
/// far more useful than the word "Conversation".
fn participant_label(participant: &Participant) -> String {
    participant
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| participant.handle.value().to_owned())
}

/// How many names an untitled group title lists before it summarises the rest.
const GROUP_TITLE_NAMES: usize = 3;

/// A conversation's name, independent of local nickname overrides.
///
/// A conversation title wins. A direct chat is named after the other person. An
/// untitled group lists the other people, then counts the remainder, so the
/// sidebar never shows several identical `Conversation` rows.
pub fn chat_name(conversation: &Conversation) -> String {
    if let Some(title) = conversation
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        return title.to_owned();
    }
    let names: Vec<String> = others(conversation).map(participant_label).collect();
    match names.len() {
        0 => "Conversation".to_owned(),
        1 => names.into_iter().next().unwrap_or_default(),
        count => {
            let shown = GROUP_TITLE_NAMES.min(count);
            let mut title = names[..shown].join(", ");
            if count > shown {
                title.push_str(&format!(" +{}", count - shown));
            }
            title
        }
    }
}

/// The quiet second line in the header: the recipient for a direct chat, the
/// participant count for a group.
pub fn conversation_subtitle(conversation: &Conversation) -> String {
    if conversation.kind == ConversationKind::Direct {
        return others(conversation)
            .next()
            .map(participant_label)
            .unwrap_or_else(|| participant_count(conversation.participants.len()));
    }
    participant_count(conversation.participants.len())
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

/// The local calendar day of a timestamp, as `(year, day of year, label)`. The
/// label reads like Slack's dividers: `Tuesday, November 14`.
pub fn day_label(created_at: i64) -> (i32, i32, String) {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let seconds = (created_at.max(0) / 1000) as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return (0, 0, String::new());
    }
    let day = DAYS.get(tm.tm_wday as usize).copied().unwrap_or("");
    let month = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("");
    (
        tm.tm_year,
        tm.tm_yday,
        format!("{day}, {month} {}", tm.tm_mday),
    )
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
    // A catch-up burst or a manual `/sync` is the most useful thing to report
    // while it is happening.
    let session = app.session.trim();
    if app.syncing || app.sync_requested || app.sync_in_flight {
        if matches!(session, "Failed" | "Blocked" | "Disconnected") {
            return Some(session.to_owned());
        }
        return Some("Syncing".to_owned());
    }
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
    if app.syncing || app.sync_requested || app.sync_in_flight {
        if matches!(app.session.trim(), "Failed" | "Blocked" | "Disconnected") {
            return StatusTone::Failure;
        }
        return StatusTone::Attention;
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
            aliases: Vec::new(),
            preview: None,
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
        draft_state(&app, "abc", 1, 1);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL,)),
            Action::None
        );
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
        // A direct chat with no title is named after the other person, not
        // `Conversation`. The opaque handle value is all the engine gave us here.
        let chat = conversation("a", None);
        assert_eq!(chat_name(&chat), "a");
        assert_eq!(conversation_subtitle(&chat), "a");
        assert_eq!(participant_count(1), "1 person");
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
    fn untitled_conversations_are_named_after_their_people() {
        let mut direct = conversation("a", None);
        direct.title = None;
        direct.participants[0].display_name = Some("Maya Chen".into());
        assert_eq!(chat_name(&direct), "Maya Chen");
        assert_eq!(conversation_subtitle(&direct), "Maya Chen");

        // A title always wins, even for a direct chat with a display name.
        direct.title = Some("Weekend crew".into());
        assert_eq!(chat_name(&direct), "Weekend crew");

        // An untitled group lists its people and counts the remainder.
        let mut group = conversation("g", None);
        group.kind = ConversationKind::Group;
        group.title = None;
        group.participants = vec![
            Participant {
                handle: Handle::new(HandleKind::Opaque, "me").unwrap(),
                display_name: None,
                is_me: true,
            },
            Participant {
                handle: Handle::new(HandleKind::Opaque, "one").unwrap(),
                display_name: Some("One".into()),
                is_me: false,
            },
            Participant {
                handle: Handle::new(HandleKind::Opaque, "two").unwrap(),
                display_name: None,
                is_me: false,
            },
            Participant {
                handle: Handle::new(HandleKind::Opaque, "three").unwrap(),
                display_name: None,
                is_me: false,
            },
            Participant {
                handle: Handle::new(HandleKind::Opaque, "four").unwrap(),
                display_name: None,
                is_me: false,
            },
        ];
        assert_eq!(chat_name(&group), "One, two, three +1");
        assert_eq!(conversation_subtitle(&group), "5 people");
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
        // The composer stays active after cancelling: it is always active now, so
        // Esc clears the draft rather than taking typing away.
        assert!(app.composing && !app.new_conversation);

        // An empty address aborts instead of starting a conversation.
        app.begin_new_conversation();
        app.draft = "   ".into();
        assert!(matches!(app.submit_intent(), CompositionIntent::None));
        assert!(app.composing);
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
            revision: crate::daemon::next_revision(),
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
    fn older_snapshots_cannot_erase_newer_history_or_live_messages() {
        let mut app = App::default();
        let chat = conversation("race", Some(1));
        let old = crate::daemon::next_revision();
        let new = crate::daemon::next_revision();
        app.apply_snapshot(Snapshot {
            revision: new,
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "new", 2, "newest")],
        });
        app.apply_snapshot(Snapshot {
            revision: old,
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "old", 1, "stale")],
        });
        assert_eq!(app.preview(&chat.id), "newest");

        let pending_read = crate::daemon::next_revision();
        app.apply_event(EventKind::Message {
            conversation_id: chat.id.clone(),
            message: message(&chat, "live", 3, "arrived live"),
            push_sequence: None,
        });
        app.apply_snapshot(Snapshot {
            revision: pending_read,
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "new", 2, "newest")],
        });
        assert_eq!(app.preview(&chat.id), "arrived live");
        assert_eq!(app.messages[&chat.id].len(), 2);

        app.apply_snapshot(Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "stored", 4, "authoritative")],
        });
        assert_eq!(app.preview(&chat.id), "authoritative");

        let other = conversation("other", Some(1));
        app.set_conversations(vec![chat.clone(), other.clone()]);
        let unrelated_read = crate::daemon::next_revision();
        app.apply_event(EventKind::Message {
            conversation_id: other.id.clone(),
            message: message(&other, "other-live", 9, "other thread event"),
            push_sequence: None,
        });
        app.apply_snapshot(Snapshot {
            revision: unrelated_read,
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(
                &chat,
                "reconciled",
                5,
                "unrelated event cannot block me",
            )],
        });
        assert_eq!(app.preview(&chat.id), "unrelated event cannot block me");
        assert!(app.conversations.iter().any(|item| item.id == other.id));
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
            push_sequence: None,
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
            push_sequence: None,
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
    fn catching_up_on_messages_shows_a_syncing_status() {
        let mut app = App {
            connection: "Connected".into(),
            connection_state: crate::daemon::ConnectionState::Connected,
            session: "Active".into(),
            ..Default::default()
        };
        assert_eq!(status_text(&app).as_deref(), Some("Active"));
        assert_eq!(status_tone(&app), StatusTone::Healthy);
        assert!(!app.is_animating());

        app.set_syncing(true);
        assert_eq!(status_text(&app).as_deref(), Some("Syncing"));
        assert_eq!(status_tone(&app), StatusTone::Attention);
        assert!(
            app.is_animating(),
            "the spinner must keep ticking while syncing"
        );

        // A manual `/sync` reports the same status even without inbound traffic.
        app.set_syncing(false);
        app.sync_requested = true;
        assert_eq!(status_text(&app).as_deref(), Some("Syncing"));
        assert_eq!(status_tone(&app), StatusTone::Attention);

        // Connection trouble still outranks the sync indicator.
        app.connection = "Disconnected, retrying".into();
        app.connection_state = crate::daemon::ConnectionState::Disconnected;
        assert_eq!(status_text(&app).as_deref(), Some("Disconnected, retrying"));
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
            push_sequence: None,
        });
        assert!(
            !app.resync_conversations,
            "a known conversation must not force a refresh"
        );

        let unknown = conversation("Brand new", Some(30));
        app.apply_event(EventKind::Message {
            conversation_id: unknown.id.clone(),
            message: message(&unknown, "m2", 40, "first message"),
            push_sequence: None,
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

    #[test]
    fn word_and_line_editing_gestures_match_a_shell() {
        let mut app = App::default();
        for character in "hello brave world".chars() {
            draft_key(&mut app, KeyCode::Char(character));
        }
        assert_eq!(app.draft, "hello brave world");

        // Ctrl+W removes the trailing word up to the space before it.
        assert!(app.edit_draft(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
        assert_eq!(app.draft, "hello brave ");

        // Alt+Backspace does the same, skipping the trailing space first.
        app.edit_draft(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(app.draft, "hello ");

        // Word movement then Ctrl+K kills the rest of the line.
        app.draft = "alpha beta gamma".into();
        app.draft_cursor = 0;
        app.edit_draft(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(app.draft_cursor, 5, "Alt+Right moves a word forward");
        app.edit_draft(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.draft, "alpha");

        // Ctrl+U clears everything before the cursor.
        app.draft = "alpha beta".into();
        app.draft_cursor = 5;
        app.edit_draft(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.draft, " beta");
    }

    #[test]
    fn mouse_input_resolves_against_the_rendered_hitboxes() {
        use ratatui::layout::Rect;
        let mut app = App::default();
        let first = conversation("First chat", Some(2));
        let second = conversation("Second chat", Some(1));
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.selected = Some(first.id.clone());

        let hits = HitMap {
            sidebar: Rect::new(0, 0, 20, 10),
            sidebar_rows: vec![
                (Rect::new(1, 1, 18, 1), first.id.clone()),
                (Rect::new(1, 2, 18, 1), second.id.clone()),
            ],
            transcript: Rect::new(21, 1, 60, 20),
            composer: Rect::new(21, 24, 60, 1),
            ..Default::default()
        };

        // A click on the second row selects that chat and focuses the list.
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 5, 2, &hits);
        assert_eq!(app.selected, Some(second.id.clone()));
        assert_eq!(app.focus, Focus::Sidebar);

        // The wheel over the transcript scrolls messages, not the selection.
        app.handle_mouse(MouseEventKind::ScrollUp, 30, 5, &hits);
        assert!(
            app.is_scrolled_back(),
            "wheel over the transcript scrolls it"
        );
        assert_eq!(app.selected, Some(second.id.clone()));

        // The wheel over the chat list moves the selection instead.
        app.handle_mouse(MouseEventKind::ScrollUp, 5, 2, &hits);
        assert_eq!(
            app.selected,
            Some(first.id.clone()),
            "wheel moves selection"
        );

        // Clicking the composer hands focus back to it.
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 30, 24, &hits);
        assert_eq!(app.focus, Focus::Composer);
    }

    fn typed(app: &mut App, text: &str) {
        for character in text.chars() {
            draft_key(app, KeyCode::Char(character));
        }
    }

    #[test]
    fn popup_enter_runs_mode_commands_and_autofills_argument_commands() {
        let mut app = App::default();
        app.set_conversations(vec![conversation("a", Some(1))]);
        typed(&mut app, "/se");
        assert!(app.command_menu);
        draft_key(&mut app, KeyCode::Enter);
        assert!(
            app.settings_open,
            "the highlighted /settings runs on the first Enter"
        );

        let mut app = App::default();
        app.set_conversations(vec![conversation("a", Some(1))]);
        typed(&mut app, "/nick");
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.draft, "/nickname ", "an argument command autofills");
        assert!(!app.command_menu);
        assert!(app.nicknames.is_empty());

        // With arguments typed the popup is closed, so Enter runs the command line.
        typed(&mut app, "Maya");
        assert!(!app.command_menu);
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.nicknames.values().next().map(String::as_str),
            Some("Maya")
        );
    }

    #[test]
    fn contacts_are_unique_local_and_nickname_editing_returns_to_chat() {
        let direct = conversation("person@example.test", Some(3));
        let alice = direct.participants[0].handle.clone();
        let mut group = conversation("group", Some(2));
        group.kind = ConversationKind::Group;
        group.participants = vec![
            Participant {
                handle: alice.clone(),
                display_name: None,
                is_me: false,
            },
            Participant {
                handle: Handle::new(HandleKind::Email, "other@example.test").unwrap(),
                display_name: None,
                is_me: false,
            },
            Participant {
                handle: Handle::new(HandleKind::Email, "me@example.test").unwrap(),
                display_name: None,
                is_me: true,
            },
        ];
        let mut app = App::default();
        app.set_conversations(vec![direct.clone(), group]);
        app.selected = Some(direct.id.clone());
        assert_eq!(app.contact_handles().len(), 2);

        typed(&mut app, "/contacts");
        draft_key(&mut app, KeyCode::Enter);
        assert!(app.contacts_open);
        assert_eq!(app.selected, Some(direct.id.clone()));
        assert!(app.draft.is_empty());
        assert_eq!(app.contact_label(&alice), "person@example.test");

        let alice_index = app
            .contact_handles()
            .iter()
            .position(|handle| handle == &alice)
            .unwrap();
        app.contacts_selection = alice_index;
        draft_key(&mut app, KeyCode::Enter);
        assert!(app.contacts_editing.is_some());
        typed(&mut app, "Maya");
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.contact_label(&alice), "Maya");
        assert_eq!(app.conversation_name(&direct), "Maya");
        assert!(app.contacts_open);
        assert!(app.draft.is_empty());

        let alice_index = app
            .contact_handles()
            .iter()
            .position(|handle| handle == &alice)
            .unwrap();
        app.contacts_selection = alice_index;
        draft_key(&mut app, KeyCode::Enter);
        app.draft = "clear".into();
        app.draft_cursor = 5;
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.contact_label(&alice), "person@example.test");
        assert!(!app.nicknames.contains_key(&nickname_key(&alice)));

        draft_key(&mut app, KeyCode::Esc);
        assert!(!app.contacts_open);
        assert_eq!(app.focus, Focus::Composer);
        assert!(app.composing);
        assert_eq!(app.selected, Some(direct.id));
    }

    #[test]
    fn contacts_navigation_scrolls_independently_and_clamps() {
        let conversations = (0..15)
            .map(|index| conversation(&format!("person{index:02}@example.test"), None))
            .collect();
        let mut app = App::default();
        app.set_conversations(conversations);
        app.begin_contacts();
        draft_key(&mut app, KeyCode::Down);
        assert_eq!(app.contacts_selection, 1);
        draft_key(&mut app, KeyCode::PageDown);
        assert_eq!(app.contacts_selection, 11);
        draft_key(&mut app, KeyCode::PageDown);
        assert_eq!(app.contacts_selection, 14);
        draft_key(&mut app, KeyCode::PageUp);
        assert_eq!(app.contacts_selection, 4);
        draft_key(&mut app, KeyCode::Home);
        assert_eq!(app.contacts_selection, 0);
    }

    #[test]
    fn phone_nickname_keys_ignore_display_punctuation() {
        let formatted = Handle::new(HandleKind::Tel, "+1 (555) 000-0009").unwrap();
        let normalized = Handle::new(HandleKind::Tel, "+15550000009").unwrap();
        assert_eq!(nickname_key(&formatted), nickname_key(&normalized));
    }

    #[test]
    fn local_conversation_rename_beats_contact_nickname_and_engine_title() {
        let mut direct = conversation("person@example.test", Some(1));
        direct.title = Some("Work chat".into());
        let handle = direct.participants[0].handle.clone();
        let mut app = App::default();
        app.nicknames.insert(nickname_key(&handle), "Maya".into());
        assert_eq!(app.conversation_name(&direct), "Maya");
        app.titles
            .insert(conversation_key(&direct.id), "Pinned name".into());
        assert_eq!(app.conversation_name(&direct), "Pinned name");
    }

    #[test]
    fn merged_conversation_aliases_preserve_selection_names_and_hidden_state() {
        let legacy = conversation("legacy-id", Some(2));
        let mut canonical = conversation("stable-id", Some(1));
        canonical.participants = legacy.participants.clone();
        canonical.aliases = vec![legacy.id.clone()];
        let legacy_key = conversation_key(&legacy.id);
        let mut app = App {
            selected: Some(legacy.id.clone()),
            ..Default::default()
        };
        app.titles.insert(legacy_key.clone(), "Saved name".into());
        app.hidden.insert(legacy_key);
        app.set_conversations(vec![canonical.clone()]);

        assert_eq!(app.selected, Some(canonical.id.clone()));
        assert_eq!(app.conversation_name(&canonical), "Saved name");
        assert!(app.visible_ids().is_empty());
        app.unhide_all();
        assert_eq!(app.notice.as_deref(), Some("Restored 1 hidden chat."));
        assert_eq!(app.visible_ids(), vec![canonical.id.clone()]);
        app.begin_rename(canonical.id.clone());
        app.draft.clear();
        app.draft_cursor = 0;
        draft_key(&mut app, KeyCode::Enter);
        assert!(!app.titles.contains_key(&conversation_key(&legacy.id)));
        assert_eq!(app.conversation_name(&canonical), "legacy-id");
        app.close_conversation(canonical.id.clone());
        assert!(app.visible_ids().is_empty());
        assert!(app.hidden.contains(&conversation_key(&canonical.id)));
        assert!(app.hidden.contains(&conversation_key(&legacy.id)));
        app.unhide_all();
        assert_eq!(app.notice.as_deref(), Some("Restored 1 hidden chat."));
    }

    #[test]
    fn context_menu_renames_and_closes_without_deleting_messages() {
        let mut app = App::default();
        let first = conversation("first", Some(2));
        let second = conversation("second", Some(1));
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.upsert(first.id.clone(), message(&first, "m1", 1, "hello"));

        // Rename: a local title beats the engine name, and an empty one clears it.
        app.open_context_menu(first.id.clone());
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.renaming, Some(first.id.clone()));
        assert_eq!(
            app.draft, "first",
            "the prompt is seeded with the current name"
        );
        app.draft = "Pinned".into();
        app.draft_cursor = 6;
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.conversation_name(&first), "Pinned");
        app.begin_rename(first.id.clone());
        app.draft.clear();
        app.draft_cursor = 0;
        draft_key(&mut app, KeyCode::Enter);
        assert_eq!(app.conversation_name(&first), "first");

        // Esc leaves the menu without acting.
        app.open_context_menu(first.id.clone());
        draft_key(&mut app, KeyCode::Esc);
        assert!(app.context_menu.is_none());
        assert!(app.renaming.is_none());

        // Close hides the chat and moves the selection, but keeps its messages.
        app.selected = Some(first.id.clone());
        app.open_context_menu(first.id.clone());
        draft_key(&mut app, KeyCode::Down);
        draft_key(&mut app, KeyCode::Enter);
        assert!(!app.visible_ids().contains(&first.id));
        assert_eq!(app.selected, Some(second.id.clone()));
        assert_eq!(app.messages.get(&first.id).map(Vec::len), Some(1));

        typed(&mut app, "/unhide");
        draft_key(&mut app, KeyCode::Enter);
        assert!(app.visible_ids().contains(&first.id));
    }

    #[test]
    fn right_click_on_a_chat_opens_its_context_menu() {
        use ratatui::layout::Rect;
        let mut app = App::default();
        let first = conversation("first", Some(2));
        let second = conversation("second", Some(1));
        app.set_conversations(vec![first.clone(), second.clone()]);
        let hits = HitMap {
            sidebar_rows: vec![
                (Rect::new(0, 1, 20, 2), first.id.clone()),
                (Rect::new(0, 3, 20, 2), second.id.clone()),
            ],
            ..Default::default()
        };
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 4, 4, &hits);
        assert_eq!(app.selected, Some(second.id.clone()));
        assert_eq!(
            app.context_menu
                .as_ref()
                .map(|menu| menu.conversation.clone()),
            Some(second.id.clone())
        );
    }

    #[test]
    fn day_labels_name_the_weekday_and_date() {
        let (_, _, label) = day_label(1_700_000_000_000);
        assert!(label.contains(", "), "{label}");
        assert!(
            ["Monday", "Tuesday", "Wednesday"]
                .iter()
                .any(|day| label.starts_with(day)),
            "{label}"
        );
        assert_eq!(
            day_label(1_700_000_000_000).1,
            day_label(1_700_000_060_000).1
        );
    }
}
