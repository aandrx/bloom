//! UI state for the Bloom TUI. Rendering lives in `ui.rs`.

use crate::composer::wrap_draft;
use crate::daemon::{ConnectionState, Snapshot, HISTORY_LIMIT};
use crate::drafts::DraftMap;
use crate::ui::{Hit, HitMap};
use bloom_domain::{
    Conversation, ConversationId, ConversationKind, Handle, HandleKind, Message, MessageId,
    Participant, TransportId,
};
use bloom_ipc::{
    ConversationSyncStatus, DraftKey, DurableDraft, EventKind, MessageSearchHit, ParticipantFilter,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

/// Rows a single PageUp or PageDown moves through the transcript.
const SCROLL_PAGE: usize = 10;

pub const HELP_ROW_COUNT: usize = 19;

/// Messages requested per older-history page.
pub const OLDER_PAGE_LIMIT: u32 = 200;
/// Maximum number of message-search hits requested at once.
pub const SEARCH_RESULT_LIMIT: u32 = 50;
/// Maximum number of paged-back messages retained per conversation, in addition
/// to the newest window loaded by a snapshot.
pub const OLDER_HISTORY_BUDGET: usize = 1000;
/// Maximum number of composer undo steps kept for the active draft owner.
const COMPOSER_UNDO_LIMIT: usize = 50;

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
    Search {
        request: u64,
        query: String,
        participant: Option<ParticipantFilter>,
    },
    FetchAttachment {
        token: String,
    },
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

/// A durable copy of a submitted body kept until its send is acknowledged. It
/// belongs to the original owner, not to whatever composer is active afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
#[doc(hidden)]
pub struct ParkedSendDraft {
    pub owner: DraftOwner,
    pub attempt: u64,
    pub saved: SavedDraft,
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

/// The store orders messages ascending by `(created_at, id)`. Never assume a
/// loaded vector is already sorted before deriving or merging history.
/// Matches the daemon's ordering exactly: `created_at`, then the stable JSON encoding
/// of the message id, which is what the store indexes and what a page cursor means.
fn message_order(a: &Message, b: &Message) -> std::cmp::Ordering {
    a.created_at.cmp(&b.created_at).then_with(|| {
        serde_json::to_string(a.id())
            .unwrap_or_default()
            .cmp(&serde_json::to_string(b.id()).unwrap_or_default())
    })
}

fn oldest_message(messages: &[Message]) -> Option<&Message> {
    messages.iter().min_by(|a, b| message_order(a, b))
}

/// The same store ordering applied to a stored read position, which keeps only a
/// timestamp and message id rather than a whole message.
fn position_order(created_at: i64, id: &MessageId) -> (i64, String) {
    (created_at, serde_json::to_string(id).unwrap_or_default())
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

impl DraftOwner {
    /// Map a composer owner onto the exact durable IPC key it can persist under.
    /// `openbubbles` is the daemon's current single transport, not a general
    /// owner-to-transport mapping; multi-transport owners are out of scope.
    #[doc(hidden)]
    pub fn durable_key(&self) -> Option<DraftKey> {
        match self {
            Self::Conversation(owner) => Some(DraftKey::Conversation {
                owner: owner.clone(),
            }),
            Self::NewChat(Some(address)) => Some(DraftKey::NewChat {
                transport: TransportId::new("openbubbles").expect("valid transport"),
                address: address.clone(),
            }),
            Self::NewChat(None) => None,
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedDraft {
    text: String,
    cursor: usize,
    pasted_body: bool,
    uncertain: bool,
}

impl SavedDraft {
    fn durable(&self) -> DurableDraft {
        DurableDraft {
            body: self.text.clone(),
            cursor: self.cursor.min(self.text.graphemes(true).count()),
            pasted_body: self.pasted_body,
            uncertain: self.uncertain,
        }
    }
}

/// A composer snapshot captured before an edit. Restoring it keeps text, cursor
/// and literal-paste provenance together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct ComposerSnapshot {
    pub text: String,
    pub cursor: usize,
    pub pasted_body: bool,
    pub uncertain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum EditKind {
    Insert,
    Delete,
    Paste,
    Multiline,
    Clear,
    Complete,
}

#[derive(Debug, Clone)]
pub struct SuspendedComposition {
    saved: SavedDraft,
    owner: Option<DraftOwner>,
}

/// Modal state for searching stored text and reported attachment filenames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub hits: Vec<MessageSearchHit>,
    pub selected: usize,
    pub loading: bool,
    pub completed: bool,
    pub error: Option<String>,
    /// The in-flight request this state is waiting for, if any.
    pub request: Option<u64>,
    /// The query whose results are currently on display.
    pub completed_query: Option<String>,
    /// The exact participant scope for the body query, if any.
    pub participant: Option<ParticipantFilter>,
    /// The scope whose results are currently on display.
    pub completed_participant: Option<ParticipantFilter>,
    /// The nested "Chats with" picker, when open.
    pub picker: Option<SearchParticipantPicker>,
}

/// One pickable search scope. `None` is the leading "All chats" choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchParticipantCandidate {
    filter: Option<ParticipantFilter>,
    label: String,
}

impl SearchParticipantCandidate {
    pub fn new(filter: Option<ParticipantFilter>, label: impl Into<String>) -> Self {
        Self {
            filter,
            label: label.into(),
        }
    }

    /// The exact scope this row commits. `None` means All chats.
    pub fn filter(&self) -> Option<&ParticipantFilter> {
        self.filter.as_ref()
    }

    pub fn into_filter(self) -> Option<ParticipantFilter> {
        self.filter
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn transport(&self) -> Option<&TransportId> {
        self.filter.as_ref().map(|filter| &filter.transport)
    }

    pub fn handle(&self) -> Option<&Handle> {
        self.filter.as_ref().map(|filter| &filter.handle)
    }

    pub fn handle_kind(&self) -> Option<&HandleKind> {
        self.handle().map(Handle::kind)
    }

    pub fn raw_handle(&self) -> Option<&str> {
        self.handle().map(Handle::value)
    }

    pub fn is_all_chats(&self) -> bool {
        self.filter.is_none()
    }

    fn matches_query(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        if self.label.to_lowercase().contains(query) {
            return true;
        }
        let Some(filter) = self.filter.as_ref() else {
            return false;
        };
        filter.handle.value().to_lowercase().contains(query)
            || filter.transport.as_str().to_lowercase().contains(query)
    }
}

fn handle_kind_rank(kind: Option<&HandleKind>) -> u8 {
    match kind {
        None => 0,
        Some(HandleKind::Tel) => 1,
        Some(HandleKind::Email) => 2,
        Some(HandleKind::Opaque) => 3,
    }
}

/// The nested participant picker. The selection is an exact filter identity, not
/// an index into a list that can change underneath it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchParticipantPicker {
    pub query: String,
    selected: Option<ParticipantFilter>,
}

impl SearchParticipantPicker {
    pub fn selected(&self) -> Option<&ParticipantFilter> {
        self.selected.as_ref()
    }
}

pub struct App {
    pub theme: Theme,
    /// Terminal-detected theme used when the preference is automatic.
    pub system_theme: Theme,
    /// None means follow the terminal theme; Some pins a user preference.
    pub theme_override: Option<Theme>,
    pub compact_sidebar: bool,
    pub terminal_width: u16,
    pub chat_drawer: Option<ChatDrawer>,
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
    /// Conversations where the user has paged back beyond the newest window.
    pub older_loaded: HashSet<ConversationId>,
    /// Conversations whose paged-back history is exhausted.
    pub older_exhausted: HashSet<ConversationId>,
    /// The conversation with an older-page request in flight, at most one.
    pub older_loading: Option<ConversationId>,
    /// The oldest message that still belongs to a conversation's newest window.
    /// Everything strictly older is paged-back history subject to the budget.
    pub older_window_floor: HashMap<ConversationId, MessageId>,
    /// The last failed older-page load.
    pub older_error: Option<String>,
    /// Set when a scroll gesture reaches the oldest loaded row and older history
    /// may still exist; the shell consumes it to start one page request.
    pub older_requested: bool,
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
    /// The last send outcome was uncertain; kept with the draft until it is sent.
    #[doc(hidden)]
    pub draft_uncertain: bool,
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
    /// Composer undo snapshots for the active draft owner, oldest first.
    #[doc(hidden)]
    pub undo_history: VecDeque<ComposerSnapshot>,
    /// Composer redo snapshots, discarded on the next new edit.
    #[doc(hidden)]
    pub redo_history: VecDeque<ComposerSnapshot>,
    #[doc(hidden)]
    pub last_edit_kind: Option<EditKind>,
    /// Display rows scrolled back from the newest message. Zero pins the transcript
    /// to the newest content, which is what a chat client should do by default.
    pub scroll: usize,
    #[doc(hidden)]
    pub reading_anchor: Option<crate::transcript::ReadingAnchor>,
    #[doc(hidden)]
    pub transcript_viewport: Option<crate::transcript::TranscriptViewport>,
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
    /// Newest attachment token selected by `/attachment`, consumed by the shell.
    pub pending_attachment: Option<String>,
    /// Token currently awaiting a fetch result, used to ignore stale replies.
    pub attachment_in_flight: Option<String>,
    pub sync_in_flight: bool,
    pub send_feedback: Option<SendFeedback>,
    pub recoverable_send: Option<RecoveryDraft>,
    /// Durable draft for an in-flight send, keyed by its original owner.
    #[doc(hidden)]
    pub parked_send: Option<ParkedSendDraft>,
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
    /// The newest message the user has seen in each conversation. Local
    /// navigation state only: never a read receipt and never remote read state.
    pub read_positions: HashMap<ConversationId, (i64, MessageId)>,
    /// The right-click menu for a conversation, when open.
    pub context_menu: Option<ContextMenu>,
    /// The conversation whose local title the composer is editing.
    pub renaming: Option<ConversationId>,
    /// The message-search modal, when open or reaching a hit.
    pub search: Option<SearchState>,
    /// Message the transcript is paging back to after a search hit was opened.
    #[doc(hidden)]
    pub search_jump: Option<(ConversationId, MessageId)>,
    /// Bounded exact context for a search hit outside normal loaded history.
    #[doc(hidden)]
    pub search_context: Option<SearchContext>,
    /// Monotonic generation for exact-context requests.
    #[doc(hidden)]
    pub search_context_generation: u64,
    /// A query a slash command asked the shell to run, with its captured scope.
    #[doc(hidden)]
    pub pending_search: Option<(u64, String, Option<ParticipantFilter>)>,
    /// Monotonic id for message-search replies.
    pub search_request: u64,
}

/// A fixed preview around one search hit, separate from normal loaded history.
/// It owns at most [`bloom_ipc::MAX_MESSAGE_CONTEXT_LIMIT`] rows and never
/// replaces or extends `App::messages`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchContext {
    /// Replies with another generation are stale and must be ignored.
    pub generation: u64,
    pub conversation: ConversationId,
    pub target: MessageId,
    /// True while the exact lookup is in flight; `messages` is empty then.
    pub loading: bool,
    /// True once the shell has taken the request, so it is spawned at most once.
    pub requested: bool,
    pub messages: Vec<Message>,
}

/// Local unread navigation state for one conversation. `shown` counts only loaded
/// messages; `capped` means the count is bounded by the newest loaded page and may
/// understate how much is unread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnreadCount {
    pub shown: usize,
    pub capped: bool,
}

/// Navigation is a separate input owner. Highlighting never retargets a draft;
/// only an explicit open commits the candidate conversation.
#[derive(Clone, Debug)]
pub struct ChatDrawer {
    pub query: String,
    pub selected: Option<ConversationId>,
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
            terminal_width: 100,
            chat_drawer: None,
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
            older_loaded: HashSet::new(),
            older_exhausted: HashSet::new(),
            older_loading: None,
            older_window_floor: HashMap::new(),
            older_error: None,
            older_requested: false,
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
            draft_uncertain: false,
            draft_cursor: 0,
            draft_vertical_column: None,
            drafts: HashMap::new(),
            active_draft_owner: None,
            suspended_composition: None,
            undo_history: VecDeque::new(),
            redo_history: VecDeque::new(),
            last_edit_kind: None,
            scroll: 0,
            reading_anchor: None,
            transcript_viewport: None,
            resync_conversations: false,
            filter: None,
            composing: true,
            pending_address: None,
            new_conversation: false,
            error: None,
            notice: None,
            sync_requested: false,
            pending_attachment: None,
            attachment_in_flight: None,
            sync_in_flight: false,
            send_feedback: None,
            recoverable_send: None,
            parked_send: None,
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
            read_positions: HashMap::new(),
            context_menu: None,
            renaming: None,
            search: None,
            search_jump: None,
            search_context: None,
            search_context_generation: 0,
            pending_search: None,
            search_request: 0,
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
            // Carry paged state across an identity alias before merging. The
            // snapshot names the canonical conversation while the cached messages and
            // the paged flag may still sit under a retired id, and `set_conversations`
            // only migrates after this merge has already run.
            if let Some(canonical) = snapshot
                .conversations
                .iter()
                .find(|conversation| conversation.id == id)
            {
                for alias in canonical.aliases.clone() {
                    // An alias that is still a listed conversation belongs to a live
                    // chat of its own; moving its cache would empty that chat.
                    if snapshot.conversations.iter().any(|other| other.id == alias) {
                        continue;
                    }
                    self.migrate_older_state(&alias, &canonical.id);
                    let retired = self.messages.remove(&alias);
                    if let Some(retired) = retired {
                        // The canonical id may already hold an older cache; union the
                        // two instead of discarding either one.
                        let mut merged = self.messages.remove(&canonical.id).unwrap_or_default();
                        merged.extend(retired);
                        let mut seen = HashSet::new();
                        merged.retain(|message| seen.insert(message.id().clone()));
                        merged.sort_by(message_order);
                        self.messages.insert(canonical.id.clone(), merged);
                    }
                }
            }
            let latest_live = self
                .latest_live_revision
                .get(&id)
                .copied()
                .unwrap_or_default();
            if snapshot.revision > latest_live {
                let incoming = snapshot.history;
                let history = self.merge_snapshot_history(&id, incoming);
                self.messages.insert(id.clone(), history);
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

    /// Combine an incoming authoritative window with paged-back history. Only
    /// conversations that the user explicitly paged keep anything outside the
    /// window; everyone else still replaces history wholesale.
    fn merge_snapshot_history(
        &mut self,
        id: &ConversationId,
        incoming: Vec<Message>,
    ) -> Vec<Message> {
        // Every snapshot records where its newest window begins, even for a thread
        // the user never paged back, so the budget always has a boundary to measure.
        let window_oldest = oldest_message(&incoming).cloned();
        match &window_oldest {
            Some(oldest) => {
                self.older_window_floor
                    .insert(id.clone(), oldest.id().clone());
            }
            None => {
                self.older_window_floor.remove(id);
            }
        }
        if !self.older_loaded.contains(id) {
            return incoming;
        }
        let Some(window_oldest) = window_oldest else {
            // The store says the thread is empty, so there is nothing to retain.
            self.older_loaded.remove(id);
            self.older_exhausted.remove(id);
            if self.older_loading.as_ref() == Some(id) {
                self.older_loading = None;
            }
            return incoming;
        };
        let existing = self.messages.remove(id).unwrap_or_default();
        let mut seen: HashSet<MessageId> = incoming.iter().map(|m| m.id().clone()).collect();
        let mut older: Vec<Message> = existing
            .into_iter()
            .filter(|message| message_order(message, &window_oldest).is_lt())
            .filter(|message| seen.insert(message.id().clone()))
            .collect();
        older.sort_by(message_order);
        if older.len() > OLDER_HISTORY_BUDGET {
            older.drain(..older.len() - OLDER_HISTORY_BUDGET);
        }
        older.extend(incoming);
        older
    }

    /// Whether an older history page can be requested for the selected thread.
    pub fn can_load_older(&self) -> bool {
        if self.search_context_active() {
            return false;
        }
        let Some(id) = self.selected.as_ref() else {
            return false;
        };
        if self.older_loading.is_some() || self.older_exhausted.contains(id) {
            return false;
        }
        self.messages
            .get(id)
            .is_some_and(|messages| !messages.is_empty())
    }

    /// Mark the selected conversation as loading and return the cursor for the
    /// page before its true oldest loaded message.
    pub fn begin_load_older(&mut self) -> Option<(ConversationId, bloom_ipc::HistoryCursor)> {
        if !self.can_load_older() {
            return None;
        }
        let id = self.selected.clone()?;
        let oldest = oldest_message(self.messages.get(&id)?)?.clone();
        let cursor = bloom_ipc::HistoryCursor {
            created_at: oldest.created_at,
            id: oldest.id().clone(),
        };
        self.older_loading = Some(id.clone());
        self.older_error = None;
        self.older_requested = false;
        Some((id, cursor))
    }

    /// Prepend an older page, ignoring results for a conversation that is no
    /// longer the one loading. Newest-window content is left untouched.
    pub fn finish_load_older(
        &mut self,
        conversation_id: ConversationId,
        page: Vec<Message>,
        requested: u32,
    ) {
        self.older_requested = false;
        if self.older_loading.as_ref() != Some(&conversation_id) {
            return;
        }
        self.older_loading = None;
        let short_page = page.len() < requested as usize;
        let existing = self.messages.remove(&conversation_id).unwrap_or_default();
        let mut seen: HashSet<MessageId> = existing.iter().map(|m| m.id().clone()).collect();
        let oldest = existing.iter().min_by(|a, b| message_order(a, b));
        let mut older: Vec<Message> = page
            .into_iter()
            .filter(|message| oldest.is_none_or(|oldest| message_order(message, oldest).is_lt()))
            .filter(|message| seen.insert(message.id().clone()))
            .collect();
        older.sort_by(message_order);
        let floor = self
            .older_window_floor
            .get(&conversation_id)
            .and_then(|floor| existing.iter().find(|message| message.id() == floor))
            .cloned();
        let oldest_before = oldest_message(&existing).cloned();
        let fetched_older = !older.is_empty();
        let mut merged = older;
        merged.extend(existing);
        // Sort before trimming: `upsert` only orders by timestamp, so tie groups are
        // not guaranteed to be in store order yet.
        merged.sort_by(message_order);
        if let Some(floor) = floor {
            let older_count =
                merged.partition_point(|message| message_order(message, &floor).is_lt());
            if older_count > OLDER_HISTORY_BUDGET {
                merged.drain(..older_count - OLDER_HISTORY_BUDGET);
            }
        }
        // A page that the budget immediately trims away leaves the oldest loaded
        // message unchanged. Treat that as the end of useful paging: otherwise the
        // same cursor is requested forever and every result is discarded.
        let reached_budget = fetched_older
            && oldest_before
                .as_ref()
                .zip(oldest_message(&merged))
                .is_some_and(|(before, after)| before.id() == after.id());
        self.messages.insert(conversation_id.clone(), merged);
        self.older_loaded.insert(conversation_id.clone());
        self.older_error = None;
        if short_page || reached_budget {
            self.older_exhausted.insert(conversation_id.clone());
        }
    }

    /// Record a failed older-page load without disturbing the loaded history.
    pub fn fail_load_older(&mut self, conversation_id: ConversationId, message: String) {
        self.older_requested = false;
        if self.older_loading.as_ref() != Some(&conversation_id) {
            return;
        }
        let showing = self.selected.as_ref() == Some(&conversation_id);
        self.older_loading = None;
        // A failure for a chat the user already left must not colour the next one.
        if showing {
            self.older_error = Some(message);
        }
    }

    /// Carry paged-back history across an identity alias, like drafts do.
    fn migrate_older_state(&mut self, from: &ConversationId, to: &ConversationId) {
        if from == to {
            return;
        }
        if self.older_loaded.remove(from) {
            self.older_loaded.insert(to.clone());
        }
        if self.older_exhausted.remove(from) {
            self.older_exhausted.insert(to.clone());
        }
        // The in-flight reply still carries the retired id and will be ignored, so
        // drop the slot instead of rewriting it: a rewritten id could never match
        // and would block every later page load.
        if self.older_loading.as_ref() == Some(from) {
            self.older_loading = None;
        }
        if let Some(floor) = self.older_window_floor.remove(from) {
            self.older_window_floor.entry(to.clone()).or_insert(floor);
        }
    }

    pub fn set_conversations(&mut self, mut conversations: Vec<Conversation>) {
        // Read markers belong to every known conversation, not just the open one.
        // Merge aliases before selection reconciliation, including after reload.
        for conversation in &conversations {
            for alias in &conversation.aliases {
                if alias == &conversation.id {
                    continue;
                }
                if let Some(position) = self.read_positions.remove(alias) {
                    let advances =
                        self.read_positions
                            .get(&conversation.id)
                            .is_none_or(|current| {
                                position_order(position.0, &position.1)
                                    > position_order(current.0, &current.1)
                            });
                    if advances {
                        self.read_positions
                            .insert(conversation.id.clone(), position);
                    }
                }
            }
        }
        if let Some(selected) = self.selected.clone() {
            if let Some(canonical) = conversations
                .iter()
                .find(|conversation| conversation.aliases.contains(&selected))
            {
                // An identity alias must carry the open draft with it, not strand it
                // under the retired row.
                let canonical_id = canonical.id.clone();
                self.migrate_draft_owner(&selected, &canonical_id);
                self.migrate_older_state(&selected, &canonical_id);
                self.migrate_search_context_alias(&selected, &canonical_id);
                // An alias is not navigation. Keep the semantic reading context
                // through the gap before the canonical identity's history loads.
                self.selected = Some(canonical_id.clone());
                if let Some(anchor) = self.reading_anchor.as_mut() {
                    anchor.conversation = canonical_id.clone();
                }
                if let Some(view) = self.transcript_viewport.as_mut() {
                    view.conversation = canonical_id;
                }
            }
        }
        if let Some(drawer) = self.chat_drawer.as_mut() {
            if let Some(canonical) = drawer.selected.as_ref().and_then(|candidate| {
                conversations
                    .iter()
                    .find(|conversation| conversation.aliases.contains(candidate))
            }) {
                // Carry the candidate forward without committing navigation,
                // including successive snapshots that expose only the last alias.
                drawer.selected = Some(canonical.id.clone());
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
            // Changing threads retires any exact-context lookup and its reply.
            self.clear_search_context();
            self.abandon_search_jump();
            self.older_requested = false;
            self.older_error = None;
            // A page still in flight belongs to the chat being left; its result is
            // ignored, so the slot must not block the next chat.
            self.older_loading = None;
            self.scroll_to_newest();
            self.transcript_viewport = None;
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
        self.clear_undo_history();
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
                        uncertain: self.draft_uncertain,
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
        self.draft_uncertain = saved.uncertain;
        self.draft_vertical_column = None;
        self.command_menu = false;
        self.update_command_menu();
    }

    /// Keep the submitted body durable until the send result arrives. The active
    /// buffer is retired separately, so the parked copy cannot look like a delete.
    fn park_send_draft(&mut self, owner: DraftOwner, body: String, attempt: u64) {
        // A deferred failure keeps its own parked copy. Move it into the normal
        // draft map before this send parks, so a later send cannot overwrite it.
        if let Some(previous) = self.parked_send.take() {
            if !previous.saved.text.is_empty() {
                self.drafts.entry(previous.owner).or_insert(previous.saved);
            }
        }
        self.parked_send = Some(ParkedSendDraft {
            owner,
            attempt,
            saved: SavedDraft {
                cursor: body.graphemes(true).count(),
                text: body,
                pasted_body: self.pasted_body,
                uncertain: self.draft_uncertain,
            },
        });
    }

    /// A quit that could not settle a send leaves the parked draft marked
    /// uncertain, so a restored composer never implies confirmed delivery.
    pub fn mark_pending_send_uncertain(&mut self) {
        if self.pending_send_attempt.is_some() {
            if let Some(parked) = self.parked_send.as_mut() {
                parked.saved.uncertain = true;
            }
            self.draft_uncertain = true;
        }
    }

    fn clear_parked_send(&mut self, attempt: u64) {
        if self
            .parked_send
            .as_ref()
            .is_some_and(|parked| parked.attempt == attempt)
        {
            self.parked_send = None;
        }
    }

    /// Retire the active buffer and its saved copy, e.g. after a message is sent.
    fn retire_active_draft(&mut self) {
        self.clear_undo_history();
        if let Some(owner) = self.active_draft_owner.clone() {
            self.drafts.remove(&owner);
        }
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_uncertain = false;
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

    /// The active owner as an exact durable key, if this buffer may be persisted.
    #[doc(hidden)]
    pub fn current_persistable_owner(&self) -> Option<DraftKey> {
        self.active_draft_owner
            .as_ref()
            .and_then(DraftOwner::durable_key)
            .filter(DraftKey::is_valid)
    }

    /// Every session draft that should exist in the durable store. An absent key
    /// means delete. The active buffer overrides any parked copy, and a suspended
    /// modal composition is included while its owner is parked.
    #[doc(hidden)]
    pub fn desired_drafts(&self) -> DraftMap {
        fn merge(map: &mut DraftMap, owner: &DraftOwner, saved: &SavedDraft) {
            let Some(key) = owner.durable_key().filter(DraftKey::is_valid) else {
                return;
            };
            if saved.text.is_empty() {
                map.remove(&key);
            } else {
                // Keep even invalid content so the coordinator can hold the prior
                // persisted value instead of mistaking it for a delete intent.
                map.insert(key, saved.durable());
            }
        }
        fn merge_if_present(map: &mut DraftMap, owner: &DraftOwner, saved: &SavedDraft) {
            let Some(key) = owner.durable_key().filter(DraftKey::is_valid) else {
                return;
            };
            if !saved.text.is_empty() && !map.contains_key(&key) {
                map.insert(key, saved.durable());
            }
        }

        let mut map = DraftMap::new();
        for (owner, saved) in &self.drafts {
            merge(&mut map, owner, saved);
        }
        if let Some(owner) = &self.active_draft_owner {
            // An empty active buffer is an explicit clear, but it must not erase a
            // parked copy for the same owner while its send is still in flight.
            if self.draft.is_empty() {
                if !self
                    .parked_send
                    .as_ref()
                    .is_some_and(|parked| parked.owner == *owner)
                {
                    merge(
                        &mut map,
                        owner,
                        &SavedDraft {
                            text: self.draft.clone(),
                            cursor: self.draft_cursor,
                            pasted_body: self.pasted_body,
                            uncertain: self.draft_uncertain,
                        },
                    );
                }
            } else {
                merge(
                    &mut map,
                    owner,
                    &SavedDraft {
                        text: self.draft.clone(),
                        cursor: self.draft_cursor,
                        pasted_body: self.pasted_body,
                        uncertain: self.draft_uncertain,
                    },
                );
            }
        }
        if let Some(snapshot) = &self.suspended_composition {
            if let Some(owner) = &snapshot.owner {
                merge(&mut map, owner, &snapshot.saved);
            }
        }
        if let Some(parked) = &self.parked_send {
            merge_if_present(&mut map, &parked.owner, &parked.saved);
        }
        map
    }

    /// Apply a loaded draft only to an empty active composer that still owns the
    /// exact key and has no newer local copy. Returns whether anything changed.
    #[doc(hidden)]
    pub fn apply_loaded_draft(&mut self, key: DraftKey, draft: DurableDraft) -> bool {
        // The active empty buffer is authoritative for saving, but it must not
        // clobber a newer parked or suspended body for the same owner.
        let has_local_copy = self.drafts.iter().any(|(owner, saved)| {
            owner.durable_key().as_ref() == Some(&key) && !saved.text.is_empty()
        }) || self.suspended_composition.as_ref().is_some_and(|snapshot| {
            snapshot
                .owner
                .as_ref()
                .and_then(DraftOwner::durable_key)
                .as_ref()
                == Some(&key)
                && !snapshot.saved.text.is_empty()
        });
        if self.current_persistable_owner().as_ref() != Some(&key)
            || !self.draft.is_empty()
            || !draft.is_valid()
            || has_local_copy
            || self.desired_drafts().contains_key(&key)
        {
            return false;
        }
        self.clear_undo_history();
        self.draft = draft.body;
        self.draft_cursor = draft.cursor.min(self.draft.graphemes(true).count());
        self.pasted_body = draft.pasted_body;
        self.draft_uncertain = draft.uncertain;
        self.draft_vertical_column = None;
        self.command_menu = false;
        self.update_command_menu();
        true
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
        self.clear_undo_history();
        self.suspended_composition = Some(SuspendedComposition {
            owner,
            saved: SavedDraft {
                text: std::mem::take(&mut self.draft),
                cursor: self.draft_cursor,
                pasted_body: self.pasted_body,
                uncertain: self.draft_uncertain,
            },
        });
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_uncertain = false;
        self.command_menu = false;
    }

    fn close_preferences(&mut self) {
        self.settings_open = false;
        self.theme_panel_open = false;
        self.composing = true;
        self.resume_composer_draft();
    }

    /// Capture gestures against the displayed geometry, even when an event arrives
    /// before the next coalesced redraw.
    fn anchor_scroll(&mut self) {
        self.reading_anchor = None;
        if self.scroll > 0 {
            if let Some(view) = self.transcript_viewport.as_ref() {
                if self.selected.as_ref() == Some(&view.conversation) && !self.new_conversation {
                    self.scroll = self.scroll.min(view.max_scroll());
                    if self.scroll > 0 {
                        self.reading_anchor = view.anchor_at(view.max_scroll() - self.scroll);
                    }
                }
            }
        }
    }

    /// Resolve using geometry from this frame's actual transcript formatter.
    pub fn transcript_top(&self, view: &crate::transcript::TranscriptViewport) -> usize {
        view.top_for(self.scroll, self.reading_anchor.as_ref())
    }

    /// Record only a successfully displayed frame. Returns whether a read marker
    /// advanced, requiring a follow-up redraw of badges. Rendering stays immutable.
    pub fn record_transcript(
        &mut self,
        view: Option<crate::transcript::TranscriptViewport>,
    ) -> bool {
        if let Some(view) = view {
            self.scroll = view.back();
            self.reading_anchor = if self.scroll > 0 {
                view.anchor_at(view.top)
            } else {
                None
            };
            self.transcript_viewport = Some(view);
            return self.mark_selected_read_if_following();
        } else if self.new_conversation || self.selected.is_none() {
            self.scroll_to_newest();
            self.transcript_viewport = None;
        }
        // A tiny fallback frame has no transcript; retain its last reading anchor
        // and do not mark unseen messages read.
        false
    }

    /// True when a panel or popup consumes input instead of the transcript.
    fn overlay_owns_input(&self) -> bool {
        self.chat_drawer.is_some()
            || self.help_open
            || self.settings_open
            || self.theme_panel_open
            || self.contacts_open
            || self.renaming.is_some()
            || self.context_menu.is_some()
            || self.filter_query().is_some()
            || self.command_menu
            || self.search.is_some()
    }

    /// True when the recorded viewport is the selected thread and pinned to its
    /// oldest loaded row.
    fn transcript_at_oldest(&self) -> bool {
        let Some(view) = self.transcript_viewport.as_ref() else {
            return false;
        };
        self.selected.as_ref() == Some(&view.conversation)
            && !self.new_conversation
            // The renderer may step one row down from the very top so a byline
            // does not sit alone on the last visible row.
            && (self.scroll >= view.max_scroll() || view.top <= 1)
    }

    /// Ask the shell for another page when a scroll gesture reaches the oldest row.
    ///
    /// Reaching the top is itself the request: the affordance would otherwise be
    /// drawn from the previous frame's geometry and only appear on the same gesture
    /// that already triggered the load.
    fn note_older_request(&mut self) {
        if !self.overlay_owns_input() && self.transcript_at_oldest() && self.can_load_older() {
            self.older_requested = true;
        }
    }

    /// Scroll the transcript back through older messages, in display rows.
    pub fn scroll_back(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
        self.anchor_scroll();
        self.note_older_request();
    }

    /// Scroll forward towards the newest message. Zero means pinned to the newest.
    pub fn scroll_forward(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
        self.anchor_scroll();
    }

    /// Request the oldest content. The renderer clamps this to what exists.
    pub fn scroll_to_oldest(&mut self) {
        self.scroll = usize::MAX;
        self.anchor_scroll();
        self.note_older_request();
    }

    /// Return to the newest normal history without changing composition. This is
    /// also the End/Latest exit from a search context.
    pub fn scroll_to_newest(&mut self) {
        self.scroll = 0;
        self.reading_anchor = None;
        self.transcript_viewport = None;
        self.clear_search_context();
        self.abandon_search_jump();
    }

    /// True while the transcript is showing something other than the newest
    /// normal message. A search context stays scrolled back even when its short
    /// preview clamps to zero.
    pub fn is_scrolled_back(&self) -> bool {
        self.scroll > 0 || self.search_context_active()
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
        let before = self.begin_edit();
        self.draft.replace_range(start..end, &suggestion.emoji);
        self.draft_cursor = self.draft[..start + suggestion.emoji.len()]
            .graphemes(true)
            .count();
        self.emoji_selection = 0;
        self.update_command_menu();
        self.record_step(EditKind::Complete, before);
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

    /// Open the message-search modal. Only overlay state is cleared; the
    /// recipient's draft is parked and restored unchanged on close. A new-message
    /// flow owns the composer, so search refuses rather than opening invisibly.
    pub fn open_search(&mut self) {
        if self.new_conversation || self.pending_address.is_some() {
            self.notice = Some("Finish or cancel the new message first.".into());
            return;
        }
        if self.filter_query().is_some() {
            self.cancel_filter();
        }
        self.suspend_composer_draft();
        self.leader_pending = false;
        self.help_open = false;
        self.settings_open = false;
        self.theme_panel_open = false;
        self.contacts_open = false;
        self.contacts_editing = None;
        self.command_menu = false;
        self.context_menu = None;
        self.renaming = None;
        self.chat_drawer = None;
        self.search = Some(SearchState::default());
        self.focus = Focus::Composer;
        self.composing = false;
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_uncertain = false;
        self.error = None;
    }

    pub fn close_search(&mut self) {
        self.search = None;
        self.composing = true;
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_uncertain = false;
        self.resume_composer_draft();
    }

    pub fn search_input(&mut self, character: char) {
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            picker.query.push(character);
            picker.selected = None;
            return;
        }
        if let Some(state) = self.search.as_mut() {
            state.query.push(character);
        }
        self.search_edited();
    }

    pub fn search_backspace(&mut self) {
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            if let Some((start, _)) = picker.query.grapheme_indices(true).next_back() {
                picker.query.truncate(start);
            }
            picker.selected = None;
            return;
        }
        if let Some(state) = self.search.as_mut() {
            if let Some((start, _)) = state.query.grapheme_indices(true).next_back() {
                state.query.truncate(start);
            }
        }
        self.search_edited();
    }

    pub fn search_delete_word(&mut self) {
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            let trimmed = picker.query.trim_end();
            let start = trimmed
                .char_indices()
                .rev()
                .find(|(_, character)| character.is_whitespace())
                .map_or(0, |(index, character)| index + character.len_utf8());
            picker.query.truncate(start);
            picker.selected = None;
            return;
        }
        if let Some(state) = self.search.as_mut() {
            let trimmed = state.query.trim_end();
            let start = trimmed
                .char_indices()
                .rev()
                .find(|(_, character)| character.is_whitespace())
                .map_or(0, |(index, character)| index + character.len_utf8());
            state.query.truncate(start);
        }
        self.search_edited();
    }

    pub fn search_clear(&mut self) {
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            picker.query.clear();
            picker.selected = None;
            return;
        }
        if let Some(state) = self.search.as_mut() {
            state.query.clear();
        }
        self.search_edited();
    }

    /// Editing the query invalidates every result the old query produced, so a
    /// later Enter cannot jump an unrelated hit.
    fn search_edited(&mut self) {
        if let Some(state) = self.search.as_mut() {
            state.request = None;
            state.loading = false;
            state.error = None;
            state.hits.clear();
            state.completed = false;
            state.completed_query = None;
            state.completed_participant = None;
            state.selected = 0;
        }
    }

    /// Append pasted text to the query while search owns the input. A nested
    /// picker receives its own query text and never edits the body query.
    pub fn search_paste(&mut self, input: &str) {
        let clean = sanitize_paste(input).replace('\n', " ");
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            picker.query.push_str(&clean);
            picker.selected = None;
            return;
        }
        if let Some(state) = self.search.as_mut() {
            state.query.push_str(&clean);
        }
        self.search_edited();
    }

    pub fn search_step(&mut self, delta: isize) {
        if let Some(state) = self.search.as_mut() {
            let last = state.hits.len().saturating_sub(1);
            state.selected = state.selected.saturating_add_signed(delta).min(last);
        }
    }

    pub fn search_select(&mut self, index: usize) {
        if let Some(state) = self.search.as_mut() {
            state.selected = index.min(state.hits.len().saturating_sub(1));
        }
    }

    pub fn search_editable(&self) -> bool {
        self.search.is_some()
    }

    pub fn search_results_visible(&self) -> bool {
        self.search.as_ref().is_some_and(|state| {
            state.picker.is_none()
                && !state.hits.is_empty()
                && state.completed_query.as_deref() == Some(state.query.trim())
                && state.participant == state.completed_participant
        })
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(|state| state.query.as_str())
    }

    /// The active participant scope, or `None` for an unscoped query.
    pub fn search_participant_scope(&self) -> Option<&ParticipantFilter> {
        self.search
            .as_ref()
            .and_then(|state| state.participant.as_ref())
    }

    pub fn search_participant_picker_open(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|state| state.picker.is_some())
    }

    pub fn search_participant_picker_query(&self) -> Option<&str> {
        self.search
            .as_ref()
            .and_then(|state| state.picker.as_ref())
            .map(|picker| picker.query.as_str())
    }

    /// Open the nested picker on the current scope. It never starts or reruns a
    /// search, and it never touches the body query or recipient draft.
    pub fn open_search_participant_picker(&mut self) -> bool {
        let Some(state) = self.search.as_mut() else {
            return false;
        };
        let selected = state.participant.clone();
        state.picker = Some(SearchParticipantPicker {
            query: String::new(),
            selected,
        });
        true
    }

    /// Cancel the inner picker while keeping Search, its body query, its scope and
    /// its draft state exactly as they were.
    pub fn close_search_participant_picker(&mut self) {
        if let Some(state) = self.search.as_mut() {
            state.picker = None;
        }
    }

    /// Every known non-self participant choice, deduplicated by exact transport
    /// and handle, plus the leading All chats row. Labels are not identity.
    fn search_participant_options(&self) -> Vec<SearchParticipantCandidate> {
        let mut options = vec![SearchParticipantCandidate::new(None, "All chats")];
        let mut seen = HashSet::new();
        for conversation in &self.conversations {
            for participant in &conversation.participants {
                if participant.is_me {
                    continue;
                }
                let key = (
                    conversation.id.transport().clone(),
                    participant.handle.clone(),
                );
                if !seen.insert(key) {
                    continue;
                }
                let label = self.search_participant_label(participant);
                options.push(SearchParticipantCandidate::new(
                    Some(ParticipantFilter {
                        transport: conversation.id.transport().clone(),
                        handle: participant.handle.clone(),
                    }),
                    label,
                ));
            }
        }
        let mut rest = options.split_off(1);
        rest.sort_by(|left, right| {
            left.label
                .to_lowercase()
                .cmp(&right.label.to_lowercase())
                .then_with(|| {
                    left.transport()
                        .map(TransportId::as_str)
                        .cmp(&right.transport().map(TransportId::as_str))
                })
                .then_with(|| {
                    handle_kind_rank(left.handle_kind()).cmp(&handle_kind_rank(right.handle_kind()))
                })
                .then_with(|| left.raw_handle().cmp(&right.raw_handle()))
        });
        options.extend(rest);
        options
    }

    /// The picker's currently visible candidates, including All chats.
    pub fn search_participant_candidates(&self) -> Vec<SearchParticipantCandidate> {
        let Some(query) = self.search_participant_picker_query() else {
            return Vec::new();
        };
        let query = query.trim().to_lowercase();
        let mut options = self.search_participant_options();
        let all_chats = options.remove(0);
        options.retain(|candidate| candidate.matches_query(&query));
        let mut visible = vec![all_chats];
        visible.extend(options);
        visible
    }

    pub fn selected_search_participant_candidate(&self) -> Option<SearchParticipantCandidate> {
        let visible = self.search_participant_candidates();
        let selected = self
            .search
            .as_ref()
            .and_then(|state| state.picker.as_ref())
            .map(|picker| picker.selected.clone())?;
        visible
            .iter()
            .find(|candidate| candidate.filter().cloned() == selected)
            .cloned()
    }

    /// Move the picker selection by identity, clamped to the visible candidates.
    pub fn step_search_participant(&mut self, delta: isize) {
        let visible = self.search_participant_candidates();
        if visible.is_empty() {
            return;
        }
        let Some(picker) = self.search.as_ref().and_then(|state| state.picker.as_ref()) else {
            return;
        };
        let current = visible
            .iter()
            .position(|candidate| candidate.filter() == picker.selected.as_ref())
            .unwrap_or(0) as isize;
        let last = visible.len().saturating_sub(1) as isize;
        let next = current.saturating_add(delta).clamp(0, last) as usize;
        let filter = visible[next].filter().cloned();
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            picker.selected = filter;
        }
    }

    /// Select a visible candidate by position. The stored selection is still the
    /// exact filter identity, so the later list can change without retargeting it.
    pub fn select_search_participant(&mut self, index: usize) {
        let visible = self.search_participant_candidates();
        let index = index.min(visible.len().saturating_sub(1));
        let Some(filter) = visible
            .get(index)
            .map(|candidate| candidate.filter().cloned())
        else {
            return;
        };
        if let Some(picker) = self.search.as_mut().and_then(|state| state.picker.as_mut()) {
            picker.selected = filter;
        }
    }

    /// Commit an exact optional scope. Unknown or removed participants are
    /// rejected instead of falling back to All chats. A changed scope keeps the
    /// body query but invalidates every result of the old scope.
    pub fn commit_search_participant(&mut self, filter: Option<ParticipantFilter>) -> bool {
        if self.search.is_none() {
            return false;
        }
        let known = self.search_participant_options();
        let valid = match filter.as_ref() {
            None => true,
            Some(wanted) => known
                .iter()
                .any(|candidate| candidate.filter() == Some(wanted)),
        };
        if !valid {
            return false;
        }
        let changed = self
            .search
            .as_ref()
            .is_some_and(|state| state.participant != filter);
        if let Some(state) = self.search.as_mut() {
            state.participant = filter;
            state.picker = None;
        }
        if changed {
            self.search_edited();
        }
        true
    }

    /// Commit whatever the picker currently highlights.
    pub fn commit_selected_search_participant(&mut self) -> bool {
        let Some(candidate) = self.selected_search_participant_candidate() else {
            return false;
        };
        self.commit_search_participant(candidate.into_filter())
    }

    fn search_participant_label(&self, participant: &Participant) -> String {
        if self
            .nicknames
            .contains_key(&nickname_key(&participant.handle))
        {
            return self.contact_label(&participant.handle);
        }
        participant
            .display_name
            .clone()
            .unwrap_or_else(|| participant.handle.value().to_owned())
    }

    /// Start a search. An empty or whitespace-only query never enters loading.
    /// The returned id must accompany the reply so stale results are dropped.
    pub fn begin_search(&mut self) -> Option<(u64, String)> {
        let request = self.search_request;
        let state = self.search.as_mut()?;
        let query = state.query.trim().to_owned();
        if query.is_empty() {
            return None;
        }
        self.search_request = self.search_request.wrapping_add(1);
        state.request = Some(request);
        state.loading = true;
        state.completed = false;
        state.completed_query = None;
        state.error = None;
        state.hits.clear();
        state.selected = 0;
        Some((request, query))
    }

    pub fn finish_search(&mut self, request: u64, hits: Vec<MessageSearchHit>) {
        if let Some(state) = self.search.as_mut() {
            if state.request != Some(request) {
                return;
            }
            state.loading = false;
            state.completed = true;
            state.hits = hits;
            state.selected = 0;
            state.completed_query = Some(state.query.trim().to_owned());
            state.completed_participant = state.participant.clone();
        }
    }

    pub fn fail_search(&mut self, request: u64, message: String) {
        if let Some(state) = self.search.as_mut() {
            if state.request != Some(request) {
                return;
            }
            state.loading = false;
            state.completed = true;
            state.error = Some(message);
        }
    }

    pub fn selected_search_hit(&self) -> Option<MessageSearchHit> {
        self.search
            .as_ref()
            .and_then(|state| state.hits.get(state.selected).cloned())
    }

    /// Commit the selected hit: close the modal, open its conversation, and
    /// remember the message the transcript must reach. Loading the normal thread
    /// still owns its draft; a hit already in ordinary history anchors at once.
    pub fn begin_search_jump(&mut self) -> Option<(ConversationId, MessageId)> {
        let hit = self.selected_search_hit()?;
        let previous = self.selected.clone();
        let conversation = hit.conversation_id;
        let message = hit.message_id;
        // A replacement hit retires any older exact-context request.
        self.clear_search_context();
        self.abandon_search_jump();
        self.close_search();
        if previous.as_ref() != Some(&conversation) {
            self.select(Some(conversation.clone()));
        }
        // Paged history stays with its own chat, exactly as a normal switch does.
        self.search_jump = Some((conversation.clone(), message.clone()));
        self.reach_search_jump();
        Some((conversation, message))
    }

    pub fn search_jump_pending(&self) -> Option<(&ConversationId, &MessageId)> {
        let (conversation, message) = self.search_jump.as_ref()?;
        (self.selected.as_ref() == Some(conversation)).then_some((conversation, message))
    }

    /// Finish a pending jump once its message is loaded, anchoring the transcript
    /// on it rather than on the tail.
    pub fn reach_search_jump(&mut self) -> bool {
        let Some((conversation, message)) = self.search_jump.clone() else {
            return false;
        };
        if self.selected.as_ref() != Some(&conversation) {
            return false;
        }
        let found = self
            .messages
            .get(&conversation)
            .is_some_and(|messages| messages.iter().any(|row| row.id() == &message));
        if !found {
            return false;
        }
        self.search_jump = None;
        self.clear_search_context();
        self.reading_anchor = Some(crate::transcript::ReadingAnchor {
            conversation,
            message,
            row: 0,
        });
        self.scroll = 1;
        self.transcript_viewport = None;
        true
    }

    pub fn abandon_search_jump(&mut self) {
        self.search_jump = None;
    }

    /// Continue a pending search jump. A hit in ordinary loaded history anchors
    /// directly; anything else starts exactly one bounded context lookup instead
    /// of paging through intermediate history.
    pub fn advance_search_jump(&mut self) {
        let Some((conversation, message)) = self.search_jump.clone() else {
            return;
        };
        if self.selected.as_ref() != Some(&conversation) {
            self.abandon_search_jump();
            self.clear_search_context();
            return;
        }
        if self.reach_search_jump() {
            return;
        }
        if self.search_context.as_ref().is_some_and(|context| {
            context.conversation == conversation && context.target == message
        }) {
            return;
        }
        self.start_search_context(conversation, message);
    }

    /// True while the selected transcript is showing a search context preview.
    pub fn search_context_active(&self) -> bool {
        !self.new_conversation
            && self
                .search_context
                .as_ref()
                .is_some_and(|context| self.selected.as_ref() == Some(&context.conversation))
    }

    /// The one buffer the transcript formatter, layout cache and viewport use.
    /// Loading context is deliberately empty, never the unrelated latest history.
    pub fn displayed_messages(&self) -> &[Message] {
        if self.search_context_active() {
            return self
                .search_context
                .as_ref()
                .map(|context| context.messages.as_slice())
                .unwrap_or_default();
        }
        self.selected
            .as_ref()
            .and_then(|id| self.messages.get(id))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The conversation whose rows `displayed_messages` describes, including a
    /// loading context whose messages are not painted yet.
    pub fn displayed_transcript_conversation(&self) -> Option<&ConversationId> {
        if self.new_conversation {
            return None;
        }
        if self.search_context_active() {
            return self.selected.as_ref();
        }
        self.selected
            .as_ref()
            .filter(|id| self.messages.contains_key(*id))
    }

    /// Take the pending exact-context lookup so the shell spawns it once.
    pub fn begin_search_context_request(&mut self) -> Option<(u64, ConversationId, MessageId)> {
        let context = self.search_context.as_mut()?;
        if !context.loading || context.requested {
            return None;
        }
        context.requested = true;
        Some((
            context.generation,
            context.conversation.clone(),
            context.target.clone(),
        ))
    }

    fn start_search_context(&mut self, conversation: ConversationId, target: MessageId) {
        self.search_context_generation = self.search_context_generation.wrapping_add(1);
        self.search_context = Some(SearchContext {
            generation: self.search_context_generation,
            conversation,
            target,
            loading: true,
            requested: false,
            messages: Vec::new(),
        });
    }

    fn clear_search_context(&mut self) {
        self.search_context = None;
    }

    /// Carry an exact-context lookup across an identity alias by restarting the
    /// same target under a new generation and canonical identity.
    fn migrate_search_context_alias(&mut self, from: &ConversationId, to: &ConversationId) {
        if from == to {
            return;
        }
        if let Some((conversation, _)) = self.search_jump.as_mut() {
            if conversation == from {
                *conversation = to.clone();
            }
        }
        let restart = self
            .search_context
            .as_ref()
            .is_some_and(|context| &context.conversation == from);
        if !restart {
            return;
        }
        self.search_context_generation = self.search_context_generation.wrapping_add(1);
        let generation = self.search_context_generation;
        if let Some(context) = self.search_context.as_mut() {
            context.generation = generation;
            context.conversation = to.clone();
            context.loading = true;
            context.requested = false;
            context.messages.clear();
        }
    }

    /// Apply an exact-context reply only when its generation, conversation and
    /// target still match. Invalid rows become an honest, recoverable notice.
    pub fn finish_message_context(
        &mut self,
        generation: u64,
        conversation: ConversationId,
        target: MessageId,
        messages: Vec<Message>,
    ) -> bool {
        if !self.context_reply_matches(generation, &conversation, &target) {
            return false;
        }
        let valid = !messages.is_empty()
            && messages.len() <= bloom_ipc::MAX_MESSAGE_CONTEXT_LIMIT as usize
            && messages.iter().any(|message| message.id() == &target)
            && messages
                .iter()
                .all(|message| message.conversation_id() == &conversation);
        if !valid {
            self.reject_message_context(
                generation,
                &conversation,
                &target,
                "That search result is no longer available. End or Latest returns to newest history.",
            );
            return false;
        }
        if let Some(context) = self.search_context.as_mut() {
            context.messages = messages;
            context.loading = false;
        }
        self.search_jump = None;
        self.reading_anchor = Some(crate::transcript::ReadingAnchor {
            conversation,
            message: target,
            row: 0,
        });
        self.scroll = 1;
        self.transcript_viewport = None;
        true
    }

    /// Cancel a failed exact-context lookup. Stale replies are ignored.
    pub fn fail_message_context(
        &mut self,
        generation: u64,
        conversation: ConversationId,
        target: MessageId,
    ) {
        self.reject_message_context(
            generation,
            &conversation,
            &target,
            "Search context could not be loaded. End or Latest returns to newest history.",
        );
    }

    fn context_reply_matches(
        &self,
        generation: u64,
        conversation: &ConversationId,
        target: &MessageId,
    ) -> bool {
        self.search_context.as_ref().is_some_and(|context| {
            context.generation == generation
                && &context.conversation == conversation
                && &context.target == target
                && self.selected.as_ref() == Some(conversation)
        })
    }

    fn reject_message_context(
        &mut self,
        generation: u64,
        conversation: &ConversationId,
        target: &MessageId,
        notice: &str,
    ) -> bool {
        if !self.context_reply_matches(generation, conversation, target) {
            return false;
        }
        self.scroll_to_newest();
        self.notice = Some(notice.into());
        true
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
        // A new chat must not expose or retarget the previous chat's context.
        self.clear_search_context();
        self.abandon_search_jump();
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
        self.draft_uncertain = false;
        self.resume_composer_draft();
    }

    /// Abandon a draft or the new-conversation flow. The composer stays active,
    /// because it is always active; `Esc` should not take typing away.
    pub fn cancel_composition(&mut self) {
        // An explicit discard is final for this draft: undo must not bring it back.
        self.clear_undo_history();
        self.command_menu = false;
        self.theme_panel_open = false;
        self.new_conversation = false;
        self.pending_address = None;
        self.composing = true;
        // A cancelled new-chat scratch is discarded, then the selected chat's draft returns.
        self.draft.clear();
        self.draft_cursor = 0;
        self.pasted_body = false;
        self.draft_uncertain = false;
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

    pub fn set_terminal_width(&mut self, width: u16) {
        self.terminal_width = width;
        let uses_drawer = crate::ui::uses_chat_drawer(width);
        if uses_drawer && self.context_menu.is_some() {
            self.close_context_menu();
            self.focus = Focus::Composer;
        }
        if uses_drawer
            && self.focus == Focus::Sidebar
            && self.context_menu.is_none()
            && self.renaming.is_none()
            && self.filter.is_none()
            && !self.settings_open
            && !self.theme_panel_open
            && !self.contacts_open
            && !self.help_open
            && !self.command_menu
        {
            self.focus = Focus::Composer;
            self.open_chat_drawer();
        }
    }

    pub fn open_chat_drawer(&mut self) {
        self.leader_pending = false;
        if self.chat_drawer.is_none() {
            self.chat_drawer = Some(ChatDrawer {
                query: String::new(),
                selected: self.selected.clone(),
            });
        }
    }

    pub fn drawer_conversations(&self) -> Vec<&Conversation> {
        let query = self
            .chat_drawer
            .as_ref()
            .map_or("", |drawer| drawer.query.trim());
        let needle = query.to_lowercase();
        self.conversations
            .iter()
            .filter(|conversation| {
                !self.conversation_is_hidden(conversation)
                    && (self
                        .conversation_name(conversation)
                        .to_lowercase()
                        .contains(&needle)
                        || conversation_matches(conversation, query))
            })
            .collect()
    }

    pub fn drawer_selection(&self) -> Option<ConversationId> {
        let rows = self.drawer_conversations();
        self.chat_drawer
            .as_ref()
            .and_then(|drawer| drawer.selected.as_ref())
            .filter(|selected| rows.iter().any(|row| &row.id == *selected))
            .cloned()
            .or_else(|| rows.first().map(|row| row.id.clone()))
    }

    fn drawer_step(&mut self, step: isize) {
        let rows = self.drawer_conversations();
        let selected = self.drawer_selection();
        let index = rows
            .iter()
            .position(|row| Some(&row.id) == selected.as_ref())
            .unwrap_or(0);
        let next = rows
            .get(
                index
                    .saturating_add_signed(step)
                    .min(rows.len().saturating_sub(1)),
            )
            .map(|row| row.id.clone());
        if let Some(drawer) = self.chat_drawer.as_mut() {
            drawer.selected = next;
        }
    }

    fn confirm_chat_drawer(&mut self) {
        let selected = self.drawer_selection();
        self.confirm_chat_drawer_id(selected);
    }

    fn confirm_chat_drawer_id(&mut self, requested: Option<ConversationId>) {
        // A click belongs to the last painted frame. Resolve it against the
        // current filtered rows, including a canonical alias, or ignore it.
        let Some(id) = requested.and_then(|requested| {
            self.drawer_conversations()
                .into_iter()
                .find(|conversation| {
                    conversation.id == requested || conversation.aliases.contains(&requested)
                })
                .map(|conversation| conversation.id.clone())
        }) else {
            return;
        };
        self.chat_drawer = None;
        self.close_preferences();
        if self.contacts_open {
            self.close_contacts();
        }
        if self.renaming.is_some() {
            self.cancel_rename();
        }
        self.cancel_filter();
        // Park even an unfinished new-chat body under its own owner. Choosing
        // an existing chat must neither discard it nor send it elsewhere.
        self.retarget_draft(None);
        self.new_conversation = false;
        self.pending_address = None;
        self.command_menu = false;
        self.help_open = false;
        self.context_menu = None;
        self.select(Some(id));
        self.resume_composer_draft();
        self.focus = Focus::Composer;
        self.composing = true;
    }

    fn drawer_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => self.chat_drawer = None,
            KeyCode::Enter => self.confirm_chat_drawer(),
            KeyCode::Up => self.drawer_step(-1),
            KeyCode::Down => self.drawer_step(1),
            KeyCode::PageUp => self.drawer_step(-10),
            KeyCode::PageDown => self.drawer_step(10),
            KeyCode::Home => self.drawer_step(isize::MIN),
            KeyCode::End => self.drawer_step(isize::MAX),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.chat_drawer = None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(drawer) = self.chat_drawer.as_mut() {
                    drawer.query.clear();
                    drawer.selected = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(drawer) = self.chat_drawer.as_mut() {
                    if let Some((index, _)) = drawer.query.grapheme_indices(true).next_back() {
                        drawer.query.truncate(index);
                    }
                    drawer.selected = None;
                }
            }
            KeyCode::Char(character)
                if !character.is_control()
                    && !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
            {
                if let Some(drawer) = self.chat_drawer.as_mut() {
                    drawer.query.push(character);
                    drawer.selected = None;
                }
            }
            _ => {}
        }
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

    fn composer_snapshot(&self) -> ComposerSnapshot {
        ComposerSnapshot {
            text: self.draft.clone(),
            cursor: self.draft_cursor,
            pasted_body: self.pasted_body,
            uncertain: self.draft_uncertain,
        }
    }

    fn clear_undo_history(&mut self) {
        self.undo_history.clear();
        self.redo_history.clear();
        self.last_edit_kind = None;
    }

    /// Record the state a completed gesture started from. Called after the draft
    /// changed, so a rejected edit cannot add a step or discard the redo history.
    /// Consecutive edits of the same kind coalesce; a paste is always its own step.
    fn record_step(&mut self, kind: EditKind, before: ComposerSnapshot) {
        let after = self.composer_snapshot();
        if before == after {
            return;
        }
        let standalone = matches!(kind, EditKind::Paste | EditKind::Complete);
        if !standalone && self.last_edit_kind == Some(kind) {
            return;
        }
        if self.undo_history.len() == COMPOSER_UNDO_LIMIT {
            self.undo_history.pop_front();
        }
        self.undo_history.push_back(before);
        self.redo_history.clear();
        self.last_edit_kind = Some(kind);
    }

    /// Capture the state before an edit, for `record_step`.
    fn begin_edit(&mut self) -> ComposerSnapshot {
        self.composer_snapshot()
    }

    fn restore_composer_snapshot(&mut self, snapshot: ComposerSnapshot) {
        self.draft = snapshot.text;
        self.draft_cursor = snapshot.cursor.min(self.draft.graphemes(true).count());
        self.pasted_body = snapshot.pasted_body;
        self.draft_uncertain = snapshot.uncertain;
        self.draft_vertical_column = None;
        // Normalise last: an empty draft must leave literal-paste mode, or a later
        // `/command` would be sent as text.
        self.update_command_menu();
    }

    fn undo_composer(&mut self) -> bool {
        let Some(previous) = self.undo_history.pop_back() else {
            return false;
        };
        if self.redo_history.len() == COMPOSER_UNDO_LIMIT {
            self.redo_history.pop_front();
        }
        self.redo_history.push_back(self.composer_snapshot());
        self.restore_composer_snapshot(previous);
        self.last_edit_kind = None;
        true
    }

    fn redo_composer(&mut self) -> bool {
        let Some(next) = self.redo_history.pop_back() else {
            return false;
        };
        if self.undo_history.len() == COMPOSER_UNDO_LIMIT {
            self.undo_history.pop_front();
        }
        self.undo_history.push_back(self.composer_snapshot());
        self.restore_composer_snapshot(next);
        self.last_edit_kind = None;
        true
    }

    /// The number of available undo and redo steps. Exposed for the key-path tests.
    #[doc(hidden)]
    pub fn undo_depths(&self) -> (usize, usize) {
        (self.undo_history.len(), self.redo_history.len())
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
        let before = self.begin_edit();
        match key.code {
            KeyCode::Backspace if control || alt => {
                self.delete_word_before_cursor();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Backspace => {
                self.delete_before_cursor();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Left if control || alt => {
                self.last_edit_kind = None;
                self.move_word(false);
            }
            KeyCode::Right if control || alt => {
                self.last_edit_kind = None;
                self.move_word(true);
            }
            KeyCode::Left => {
                self.last_edit_kind = None;
                self.move_draft_cursor(-1);
            }
            KeyCode::Right => {
                self.last_edit_kind = None;
                self.move_draft_cursor(1);
            }
            KeyCode::Home => {
                self.last_edit_kind = None;
                self.draft_cursor = 0;
            }
            KeyCode::End => {
                self.last_edit_kind = None;
                self.draft_cursor = self.draft.graphemes(true).count();
            }
            KeyCode::Char('a') if control => {
                self.last_edit_kind = None;
                self.draft_cursor = 0;
            }
            KeyCode::Char('e') if control => {
                self.last_edit_kind = None;
                self.draft_cursor = self.draft.graphemes(true).count();
            }
            KeyCode::Char('w') if control || alt => {
                self.delete_word_before_cursor();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Char('u') if control => {
                self.clear_before_cursor();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Char('k') if control => {
                self.kill_to_end();
                self.record_step(EditKind::Delete, before);
            }
            KeyCode::Char(character) if !control && !alt => {
                self.insert_draft(character);
                self.record_step(EditKind::Insert, before);
            }
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
            self.draft_uncertain = false;
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
            ("/search", "Search stored text and filenames"),
            ("/new", "Start a new direct chat"),
            ("/filter", "Filter your conversations"),
            ("/compact", "Toggle sidebar density"),
            ("/unhide", "Restore chats hidden with Close"),
            ("/unread", "Jump to the first unread message"),
            ("/cancel", "Close the composer"),
            ("/attachment", "Save the newest attachment in this chat"),
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

    /// Select the newest loaded attachment that carries a sidecar fetch token.
    /// The action is explicit: `/attachment` never downloads on its own.
    fn begin_attachment_fetch(&mut self) {
        if self.pending_attachment.is_some() || self.attachment_in_flight.is_some() {
            self.notice = Some("Attachment fetch is already in progress.".into());
            self.draft.clear();
            self.draft_cursor = 0;
            return;
        }
        let Some(conversation_id) = self.selected.as_ref() else {
            self.error = Some("Open a chat before fetching an attachment.".into());
            self.notice = None;
            self.draft.clear();
            self.draft_cursor = 0;
            return;
        };
        let newest = self
            .messages
            .get(conversation_id)
            .into_iter()
            .flatten()
            .flat_map(|message| {
                message.parts().iter().filter_map(move |part| {
                    let bloom_domain::MessagePart::Attachment(attachment) = part else {
                        return None;
                    };
                    let token = attachment.fetch_token()?;
                    Some((message.created_at, token.to_owned()))
                })
            })
            .max_by_key(|(created_at, _)| *created_at);
        let Some((_, token)) = newest else {
            self.error = Some("No fetchable attachment is loaded in this chat.".into());
            self.draft.clear();
            self.draft_cursor = 0;
            return;
        };
        self.error = None;
        self.notice = Some("Fetching the newest attachment...".into());
        self.draft.clear();
        self.draft_cursor = 0;
        self.pending_attachment = Some(token);
    }

    pub fn finish_attachment_fetch(&mut self, token: &str, result: Result<(String, u64), String>) {
        if self.attachment_in_flight.as_deref() != Some(token) {
            return;
        }
        self.attachment_in_flight = None;
        match result {
            Ok((path, bytes)) => {
                self.error = None;
                self.notice = Some(format!("Attachment saved to {path} ({bytes} bytes)."));
            }
            Err(message) => {
                self.notice = None;
                self.error = Some(message);
            }
        }
    }

    fn execute_search_command(&mut self) {
        let query = self
            .draft
            .strip_prefix("/search")
            .unwrap_or_default()
            .trim()
            .to_owned();
        self.error = None;
        self.open_search();
        if query.is_empty() {
            return;
        }
        for character in query.chars() {
            self.search_input(character);
        }
        self.pending_search = self.begin_search().map(|(request, query)| {
            let participant = self
                .search
                .as_ref()
                .and_then(|state| state.participant.clone());
            (request, query, participant)
        });
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
            ["/search", ..] => self.execute_search_command(),
            ["/attachment"] => self.begin_attachment_fetch(),
            ["/new"] => {
                self.error = None;
                self.retire_active_draft();
                self.begin_new_conversation();
            }
            ["/unread"] => {
                self.error = None;
                self.jump_to_first_unread_or_notice();
                self.draft.clear();
                self.draft_cursor = 0;
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
        if let Some(token) = self.pending_attachment.take() {
            self.attachment_in_flight = Some(token.clone());
            Action::FetchAttachment { token }
        } else if let Some((request, query, participant)) = self.pending_search.take() {
            Action::Search {
                request,
                query,
                participant,
            }
        } else if std::mem::take(&mut self.sync_requested) && !self.sync_in_flight {
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
                self.clear_parked_send(attempt);
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
                    // Keep the parked copy while recovery is deferred; only a
                    // restored composer (or a later send) retires it.
                    recovery.uncertain = error.uncertain;
                    if recovery.target == conversation
                        && self.selected == recovery.origin
                        && self.focus == recovery.origin_focus
                        && self.composing
                        && self.recovery_prompt_available()
                        && self.draft.is_empty()
                    {
                        self.clear_parked_send(attempt);
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
        self.clear_parked_send(recovery.attempt);
        self.clear_undo_history();
        self.draft = recovery.body;
        // A recovered body is always message data, even if it begins with '/'.
        self.pasted_body = true;
        self.draft_uncertain = recovery.uncertain;
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
        if let Some(drawer) = self.chat_drawer.as_mut() {
            drawer
                .query
                .push_str(&sanitize_paste(input).replace('\n', " "));
            drawer.selected = None;
            return;
        }
        if self.search.is_some() {
            self.search_paste(input);
            return;
        }
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
            // A pasted address is still an edit of this draft and must be undoable.
            let kind = if self.renaming.is_some() || self.contacts_editing.is_some() {
                None
            } else {
                Some(EditKind::Paste)
            };
            self.insert_paste(&clean.replace('\n', " "), false, kind);
        } else {
            self.focus = Focus::Composer;
            self.insert_paste(&clean, true, Some(EditKind::Paste));
        }
    }

    fn insert_paste(&mut self, text: &str, body: bool, kind: Option<EditKind>) {
        if text.is_empty() {
            return;
        }
        let before = self.begin_edit();
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
        if let Some(kind) = kind {
            self.record_step(kind, before);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.chat_drawer.is_some() {
            self.drawer_key(key);
            return Action::None;
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_search();
            return Action::None;
        }
        if self.draft.is_empty() {
            self.pasted_body = false;
            self.draft_uncertain = false;
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
                self.insert_paste("\n", true, Some(EditKind::Multiline));
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
                let before = self.begin_edit();
                self.draft.clear();
                self.draft_cursor = 0;
                self.command_menu = false;
                self.record_step(EditKind::Clear, before);
                return Action::None;
            }
            return Action::Quit;
        }
        self.notice = None;
        // Status lines are transient: the next interaction acknowledges them, so a
        // page failure is never silently replaced by an arriving snapshot before it
        // has been on screen.
        self.older_error = None;
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
            if crate::ui::uses_chat_drawer(self.terminal_width) {
                self.open_chat_drawer();
                return Action::None;
            }
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
                KeyCode::Char('u') => self.jump_to_first_unread_or_notice(),
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
                KeyCode::Char('z' | 'Z')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.undo_composer();
                    return Action::None;
                }
                KeyCode::Char('y' | 'Y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.redo_composer();
                    return Action::None;
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
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('z' | 'Z') if control => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        self.redo_composer();
                    } else {
                        self.undo_composer();
                    }
                    return Action::None;
                }
                KeyCode::Char('y' | 'Y') if control => {
                    self.redo_composer();
                    return Action::None;
                }
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
                                    let attempt = self.next_send_attempt;
                                    self.next_send_attempt += 1;
                                    self.park_send_draft(
                                        DraftOwner::Conversation(conversation.clone()),
                                        body.clone(),
                                        attempt,
                                    );
                                    self.retire_active_draft();
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
                                let attempt = self.next_send_attempt;
                                self.next_send_attempt += 1;
                                self.park_send_draft(
                                    DraftOwner::NewChat(Some(address.clone())),
                                    body.clone(),
                                    attempt,
                                );
                                self.retire_active_draft();
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

    fn search_key(&mut self, key: KeyEvent) -> Action {
        if self.search_participant_picker_open() {
            return self.search_participant_key(key);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_search(),
            KeyCode::Char('c') if control => self.close_search(),
            KeyCode::Char('p') if control => {
                self.open_search_participant_picker();
            }
            KeyCode::Enter if self.search_results_visible() => {
                if self.begin_search_jump().is_some() {
                    self.advance_search_jump();
                }
            }
            KeyCode::Enter => {
                if let Some((request, query)) = self.begin_search() {
                    let participant = self
                        .search
                        .as_ref()
                        .and_then(|state| state.participant.clone());
                    return Action::Search {
                        request,
                        query,
                        participant,
                    };
                }
            }
            KeyCode::Up => self.search_step(-1),
            KeyCode::Down => self.search_step(1),
            KeyCode::Backspace => self.search_backspace(),
            KeyCode::Char('w') if control => self.search_delete_word(),
            KeyCode::Char('u') if control => self.search_clear(),
            KeyCode::Char(character)
                if !control
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.search_input(character);
            }
            _ => {}
        }
        Action::None
    }

    /// The inner picker owns every key while it is open. It edits only its own
    /// query, commits only an exact known identity, and never closes Search.
    fn search_participant_key(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_search_participant_picker(),
            KeyCode::Char('p') if control => self.close_search_participant_picker(),
            KeyCode::Char('c') if control => self.close_search_participant_picker(),
            KeyCode::Enter => {
                self.commit_selected_search_participant();
            }
            KeyCode::Up => self.step_search_participant(-1),
            KeyCode::Down => self.step_search_participant(1),
            KeyCode::PageUp => self.step_search_participant(-10),
            KeyCode::PageDown => self.step_search_participant(10),
            KeyCode::Home => self.select_search_participant(0),
            KeyCode::End => self.select_search_participant(usize::MAX),
            KeyCode::Backspace => self.search_backspace(),
            KeyCode::Char('w') if control => self.search_delete_word(),
            KeyCode::Char('u') if control => self.search_clear(),
            KeyCode::Char(character)
                if !control
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                self.search_input(character);
            }
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
        if self.chat_drawer.is_some() {
            match kind {
                MouseEventKind::ScrollUp => self.drawer_step(-(WHEEL_ROWS as isize)),
                MouseEventKind::ScrollDown => self.drawer_step(WHEEL_ROWS as isize),
                MouseEventKind::Down(MouseButton::Left) => match hits.at(column, row) {
                    Some(Hit::DrawerChat(id)) => {
                        self.confirm_chat_drawer_id(Some(id));
                    }
                    Some(Hit::CloseDrawer) => self.chat_drawer = None,
                    _ => {}
                },
                _ => {}
            }
            return Action::None;
        }
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
        if self.search.is_some() {
            // Search is modal for the pointer too: only its own rows and wheel
            // reach it, so a header or pane click cannot stack an overlay on top.
            if self.search_participant_picker_open() {
                // Painted identities, not current row indices, own clicks. The
                // outer Search modal still consumes events outside this picker.
                match kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        if hits
                            .popup
                            .is_some_and(|area| area.contains((column, row).into())) =>
                    {
                        let step = if kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        };
                        self.step_search_participant(step);
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(Hit::SearchParticipant(filter)) = hits.at(column, row) {
                            self.commit_search_participant(filter);
                        }
                    }
                    _ => {}
                }
                return Action::None;
            }
            match kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    if hits
                        .popup
                        .is_some_and(|area| area.contains((column, row).into()))
                    {
                        let step = if kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        };
                        self.search_step(step);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(Hit::SearchRow(index)) = hits.at(column, row) {
                        self.search_select(index);
                        if self.begin_search_jump().is_some() {
                            self.advance_search_jump();
                        }
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        // Wheel ownership is resolved from the same painted layers as clicks.
        // Background panels must not receive a gesture through a popup.
        if matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
            && hits.overlay_contains(column, row)
        {
            let step = if kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            if hits
                .context_menu
                .is_some_and(|area| area.contains((column, row).into()))
            {
                self.context_step(step);
            } else if hits
                .menu
                .is_some_and(|area| area.contains((column, row).into()))
            {
                self.command_selection = self
                    .command_selection
                    .saturating_add_signed(step)
                    .min(self.command_suggestions().len().saturating_sub(1));
            } else if self.contacts_open {
                self.contacts_step(step * WHEEL_ROWS as isize);
            } else if self.new_conversation && self.pending_address.is_none() {
                self.new_contact_step(step * WHEEL_ROWS as isize);
            } else if self.search.is_some() {
                self.search_step(step);
            } else if self.filter_query().is_some() {
                self.select_step(step * WHEEL_ROWS as isize);
            } else if self.theme_panel_open {
                self.theme_panel_selection = self
                    .theme_panel_selection
                    .saturating_add_signed(step)
                    .min(2);
            } else if !self.emoji_suggestions().is_empty() {
                self.emoji_selection = self
                    .emoji_selection
                    .saturating_add_signed(step)
                    .min(self.emoji_suggestions().len().saturating_sub(1));
            }
            return Action::None;
        }
        match kind {
            MouseEventKind::ScrollUp => {
                if hits.sidebar.contains((column, row).into()) {
                    self.select_step(-1);
                } else {
                    self.scroll_back(WHEEL_ROWS);
                }
            }
            MouseEventKind::ScrollDown => {
                if hits.sidebar.contains((column, row).into()) {
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
                            self.last_edit_kind = None;
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
                Some(Hit::HeaderChats) => self.open_chat_drawer(),
                Some(Hit::Latest) => self.scroll_to_newest(),
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

    /// Replace the local read positions from a loaded config. Keys that cannot be
    /// parsed into a conversation and message identity are ignored, so a stale or
    /// unknown key is harmless.
    pub fn apply_read_positions(
        &mut self,
        stored: &HashMap<String, crate::config::StoredReadPosition>,
    ) {
        self.read_positions.clear();
        for (key, position) in stored {
            let Some((conversation_transport, conversation_value)) = key.split_once('|') else {
                continue;
            };
            let Ok(conversation_transport) = TransportId::new(conversation_transport) else {
                continue;
            };
            let Ok(conversation) = ConversationId::new(conversation_transport, conversation_value)
            else {
                continue;
            };
            let Ok(message_transport) = TransportId::new(position.transport.clone()) else {
                continue;
            };
            let Ok(message) = MessageId::new(message_transport, position.value.clone()) else {
                continue;
            };
            self.read_positions
                .insert(conversation, (position.created_at, message));
        }
    }

    /// The local read positions in the shape the saved config stores.
    pub fn read_positions_for_config(&self) -> HashMap<String, crate::config::StoredReadPosition> {
        self.read_positions
            .iter()
            .map(|(conversation, (created_at, message))| {
                (
                    conversation_key(conversation),
                    crate::config::StoredReadPosition {
                        created_at: *created_at,
                        transport: message.transport().as_str().to_owned(),
                        value: message.value().to_owned(),
                    },
                )
            })
            .collect()
    }

    /// Record a message as the newest seen for its conversation. Positions only
    /// move forward, so marking an older message read changes nothing.
    pub fn mark_read(&mut self, conversation: ConversationId, message: Message) -> bool {
        let advanced = self
            .read_positions
            .get(&conversation)
            .is_none_or(|(created_at, id)| {
                position_order(message.created_at, message.id()) > position_order(*created_at, id)
            });
        if advanced {
            self.read_positions
                .insert(conversation, (message.created_at, message.id().clone()));
        }
        advanced
    }

    /// How many loaded messages are newer than the stored position, and whether
    /// that count is bounded by the newest loaded page.
    pub fn unread_count(&self, conversation: &ConversationId) -> Option<UnreadCount> {
        let (created_at, id) = self.read_positions.get(conversation)?;
        let messages = self.messages.get(conversation)?;
        if messages.is_empty() {
            return None;
        }
        let position = position_order(*created_at, id);
        let shown = messages
            .iter()
            .filter(|message| {
                !message.from_me && position_order(message.created_at, message.id()) > position
            })
            .count();
        let capped = messages.len() >= HISTORY_LIMIT as usize
            && messages
                .iter()
                .all(|message| position_order(message.created_at, message.id()) > position);
        Some(UnreadCount { shown, capped })
    }

    /// The oldest loaded message newer than the stored position.
    pub fn first_unread(&self, conversation: &ConversationId) -> Option<&Message> {
        let (created_at, id) = self.read_positions.get(conversation)?;
        let position = position_order(*created_at, id);
        self.messages
            .get(conversation)?
            .iter()
            .filter(|message| {
                !message.from_me && position_order(message.created_at, message.id()) > position
            })
            .min_by(|a, b| message_order(a, b))
    }

    /// Mark the newest loaded message read only when the transcript is following
    /// it. Reading back through history must not clear anything.
    pub fn mark_selected_read_if_following(&mut self) -> bool {
        if self.new_conversation
            || self.scroll != 0
            || self.overlay_owns_input()
            || self.search_context_active()
            || self.search_jump_pending().is_some()
        {
            return false;
        }
        let Some(conversation) = self.selected.clone() else {
            return false;
        };
        let Some(newest) = self
            .messages
            .get(&conversation)
            .and_then(|messages| messages.iter().max_by(|a, b| message_order(a, b)))
            .cloned()
        else {
            return false;
        };
        self.mark_read(conversation, newest)
    }

    fn jump_to_first_unread_or_notice(&mut self) {
        if !self.jump_to_first_unread() {
            self.notice = Some("No unread messages.".into());
        }
    }

    /// Anchor the transcript on the first unread message, the same way a search
    /// jump anchors a hit. Returns false when the selected chat has nothing unread.
    pub fn jump_to_first_unread(&mut self) -> bool {
        // Explicit navigation supersedes an asynchronous search destination and
        // exits a ready context before anchoring normal history.
        self.abandon_search_jump();
        self.clear_search_context();
        self.older_requested = false;
        let Some(conversation) = self.selected.clone() else {
            return false;
        };
        let Some(message) = self.first_unread(&conversation).map(|row| row.id().clone()) else {
            return false;
        };
        self.reading_anchor = Some(crate::transcript::ReadingAnchor {
            conversation,
            message,
            row: 0,
        });
        self.scroll = 1;
        true
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
        matches!(name, "/nickname" | "/theme" | "/search")
    }

    /// Run (or autofill) the highlighted command from the popup.
    fn activate_selected_command(&mut self) {
        let suggestions = self.command_suggestions();
        match suggestions.get(self.command_selection).copied() {
            Some((name, _)) if Self::command_needs_argument(name) => {
                if name == "/search" && self.draft.trim() == "/search" {
                    self.execute_command();
                } else {
                    self.complete_command();
                }
            }
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
                // Keep a deliberate reading anchor. Zero alone follows the tail;
                // delivery updates and new arrivals must not steal reading focus.
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
            bloom_domain::MessagePart::Attachment(attachment) => attachment
                .file_name()
                .filter(|name| !name.is_empty())
                .map(|name| format!("[attachment: {}]", name.escape_debug()))
                .unwrap_or_else(|| "[attachment]".into()),
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
    fn attachment_command_fetches_only_the_newest_token_and_reports_the_path() {
        const OLD: &str = "0123456789abcdef0123456789abcdef";
        const NEW: &str = "fedcba9876543210fedcba9876543210";
        let chat = conversation("attachment@example.test", Some(20));
        let attachment = |token: &str| {
            bloom_domain::MessagePart::Attachment(
                bloom_domain::AttachmentRef::new(
                    format!("local-metadata:{token}"),
                    Some("image/jpeg".into()),
                    Some("photo.jpg".into()),
                    Some(3),
                )
                .unwrap()
                .with_fetch_token(Some(token.into()))
                .unwrap(),
            )
        };
        let mut older = message(&chat, "older", 10, "older");
        older.parts = vec![attachment(OLD)];
        let mut newer = message(&chat, "newer", 20, "newer");
        newer.parts = vec![attachment(NEW)];
        let mut app = App {
            selected: Some(chat.id.clone()),
            ..Default::default()
        };
        app.messages.insert(chat.id.clone(), vec![older, newer]);
        app.draft = "/attachment".into();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::FetchAttachment { token: NEW.into() }
        );
        app.finish_attachment_fetch(NEW, Ok(("/tmp/photo.jpg".into(), 3)));
        assert!(app
            .notice
            .as_deref()
            .is_some_and(|text| text.contains("photo.jpg")));
        assert!(app.error.is_none());
        app.finish_attachment_fetch(NEW, Err("stale".into()));
        assert!(app.error.is_none(), "a duplicate result must be ignored");
        app.attachment_in_flight = Some(NEW.into());
        app.finish_attachment_fetch(NEW, Err("unavailable".into()));
        assert!(app
            .error
            .as_deref()
            .is_some_and(|text| text.contains("unavailable")));
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

    #[test]
    fn older_cursor_uses_the_true_oldest_even_when_unsorted() {
        let mut app = App::default();
        let chat = conversation("cursor", Some(30));
        app.messages.insert(
            chat.id.clone(),
            vec![
                message(&chat, "b", 20, "b"),
                message(&chat, "c", 30, "c"),
                message(&chat, "a", 10, "a"),
            ],
        );
        app.select(Some(chat.id.clone()));

        let (id, cursor) = app.begin_load_older().expect("cursor");
        assert_eq!(id, chat.id);
        assert_eq!(cursor.created_at, 10);
        assert_eq!(cursor.id.value(), "a");
        assert!(app.begin_load_older().is_none(), "one request at a time");
    }

    #[test]
    fn older_page_prepends_in_order_deduplicates_and_keeps_reading_state() {
        let mut app = App::default();
        let chat = conversation("prepend", Some(300));
        let window_two = message(&chat, "w2", 200, "window-2");
        let window_three = message(&chat, "w3", 300, "window-3");
        app.messages.insert(
            chat.id.clone(),
            vec![window_two.clone(), window_three.clone()],
        );
        app.select(Some(chat.id.clone()));
        app.scroll = 5;
        app.reading_anchor = Some(crate::transcript::ReadingAnchor {
            conversation: chat.id.clone(),
            message: window_two.id().clone(),
            row: 3,
        });

        let (id, _) = app.begin_load_older().expect("cursor");
        app.finish_load_older(
            id,
            vec![
                message(&chat, "old", 100, "older"),
                message(&chat, "w2", 200, "duplicate"),
            ],
            OLDER_PAGE_LIMIT,
        );

        let texts: Vec<String> = app.messages[&chat.id].iter().map(message_text).collect();
        assert_eq!(texts, vec!["older", "window-2", "window-3"]);
        assert_eq!(app.scroll, 5);
        let kept_anchor = app.reading_anchor.as_ref().expect("anchor kept");
        assert_eq!(kept_anchor.conversation, chat.id);
        assert_eq!(kept_anchor.message, window_two.id().clone());
        assert_eq!(kept_anchor.row, 3);
    }

    #[test]
    fn a_page_trimmed_by_the_budget_stops_paging_instead_of_repeating() {
        let mut app = App::default();
        let chat = conversation("budget-stop", Some(10_000_000));
        app.set_conversations(vec![chat.clone()]);
        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "window", 10_000_000, "window")],
        });
        // Fill the paged budget, then ask for one more page.
        for round in 0..5i64 {
            let page: Vec<Message> = (0..200i64)
                .map(|offset| {
                    let at = 1_000_000 - (round + 1) * 200 + offset;
                    message(&chat, &format!("r{round}-{offset}"), at, "paged")
                })
                .collect();
            let (id, _) = app.begin_load_older().expect("cursor");
            app.finish_load_older(id, page, OLDER_PAGE_LIMIT);
        }
        assert!(app.can_load_older(), "the budget was exactly filled");

        let extra: Vec<Message> = (0..200i64)
            .map(|offset| message(&chat, &format!("extra-{offset}"), 900_000 + offset, "paged"))
            .collect();
        let (id, _) = app.begin_load_older().expect("cursor");
        let before = app.messages[&chat.id].len();
        app.finish_load_older(id, extra, OLDER_PAGE_LIMIT);

        assert_eq!(
            app.messages[&chat.id].len(),
            before,
            "a page the budget discards changes nothing"
        );
        assert!(
            !app.can_load_older(),
            "a discarded page must end paging instead of repeating the same request"
        );
    }

    #[test]
    fn a_page_failure_stays_readable_until_the_next_interaction() {
        let mut app = App::default();
        let chat = conversation("stale-error", Some(50));
        app.set_conversations(vec![chat.clone()]);
        app.selected = Some(chat.id.clone());
        app.messages
            .insert(chat.id.clone(), vec![message(&chat, "w", 50, "window")]);
        let (id, _) = app.begin_load_older().expect("cursor");
        app.fail_load_older(id, "Older messages could not be loaded.".into());
        assert!(app.older_error.is_some());

        // A reconciliation landing straight after the failure must not erase the
        // line before it has been on screen.
        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "w", 50, "window")],
        });
        assert!(
            app.older_error.is_some(),
            "an arriving snapshot must not consume the failure message"
        );

        // The next input acknowledges it.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.older_error, None);
    }

    #[test]
    fn leaving_a_chat_releases_the_page_request_slot() {
        let mut app = App::default();
        let first = conversation("first", Some(10));
        let second = conversation("second", Some(10));
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.selected = Some(first.id.clone());
        app.messages
            .insert(first.id.clone(), vec![message(&first, "a", 10, "a")]);
        app.messages
            .insert(second.id.clone(), vec![message(&second, "b", 10, "b")]);

        app.begin_load_older().expect("cursor");
        assert!(!app.can_load_older());

        app.select(Some(second.id.clone()));
        assert!(
            app.can_load_older(),
            "the next chat must not inherit the previous chat's in-flight page"
        );
    }

    #[test]
    fn paged_history_survives_an_alias_that_arrives_with_the_snapshot() {
        let legacy = conversation("legacy-paged", Some(20));
        let mut canonical = conversation("canonical-paged", Some(20));
        canonical.aliases = vec![legacy.id.clone()];
        let mut app = App::default();
        app.set_conversations(vec![legacy.clone()]);
        app.selected = Some(legacy.id.clone());
        app.messages.insert(
            legacy.id.clone(),
            vec![
                message(&legacy, "paged", 10, "paged older"),
                message(&legacy, "window", 20, "window"),
            ],
        );
        app.older_loaded.insert(legacy.id.clone());
        app.older_window_floor.insert(
            legacy.id.clone(),
            MessageId::new(legacy.id.transport().clone(), "window").unwrap(),
        );

        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(canonical.id.clone()),
            conversations: vec![canonical.clone()],
            history: vec![message(&canonical, "window", 20, "window")],
        });

        let texts: Vec<String> = app.messages[&canonical.id]
            .iter()
            .map(message_text)
            .collect();
        assert_eq!(
            texts,
            vec!["paged older", "window"],
            "the first snapshot after an alias must not drop paged history"
        );
        assert!(app.older_loaded.contains(&canonical.id));
        assert!(!app.messages.contains_key(&legacy.id));
    }

    #[test]
    fn alias_migration_unions_two_caches_instead_of_dropping_one() {
        let legacy = conversation("legacy-union", Some(20));
        let mut canonical = conversation("canonical-union", Some(20));
        canonical.aliases = vec![legacy.id.clone()];
        let mut app = App::default();
        app.set_conversations(vec![legacy.clone()]);
        app.selected = Some(legacy.id.clone());
        // The canonical id already holds a cache from an earlier read, and the
        // paged copy sits under the retired id the snapshot aliases away.
        app.messages.insert(
            canonical.id.clone(),
            vec![message(&canonical, "window", 20, "window")],
        );
        app.messages.insert(
            legacy.id.clone(),
            vec![
                message(&legacy, "paged", 10, "paged older"),
                message(&legacy, "window", 20, "window"),
            ],
        );
        app.older_loaded.insert(legacy.id.clone());
        app.older_window_floor.insert(
            legacy.id.clone(),
            MessageId::new(legacy.id.transport().clone(), "window").unwrap(),
        );

        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(canonical.id.clone()),
            conversations: vec![canonical.clone()],
            history: vec![message(&canonical, "window", 20, "window")],
        });

        let texts: Vec<String> = app.messages[&canonical.id]
            .iter()
            .map(message_text)
            .collect();
        assert_eq!(
            texts,
            vec!["paged older", "window"],
            "neither cache may be discarded, and the duplicate is removed"
        );
    }

    #[test]
    fn an_alias_that_is_also_a_live_chat_keeps_its_own_history() {
        let retired = conversation("retired-live", Some(20));
        let mut canonical = conversation("canonical-live", Some(20));
        canonical.aliases = vec![retired.id.clone()];
        let mut app = App::default();
        app.set_conversations(vec![retired.clone(), canonical.clone()]);
        app.selected = Some(retired.id.clone());
        app.messages.insert(
            retired.id.clone(),
            vec![message(&retired, "own", 10, "retired own")],
        );

        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(canonical.id.clone()),
            conversations: vec![retired.clone(), canonical.clone()],
            history: vec![message(&canonical, "window", 20, "window")],
        });

        assert!(
            app.messages.contains_key(&retired.id),
            "a chat that is still listed keeps its cache"
        );
        assert_eq!(message_text(&app.messages[&retired.id][0]), "retired own");
    }

    #[test]
    fn short_older_page_is_exhausted_and_still_prepends() {
        let mut app = App::default();
        let chat = conversation("short", Some(200));
        app.messages
            .insert(chat.id.clone(), vec![message(&chat, "w", 200, "window")]);
        app.select(Some(chat.id.clone()));

        let (id, _) = app.begin_load_older().expect("cursor");
        app.finish_load_older(
            id,
            vec![message(&chat, "old", 100, "older")],
            OLDER_PAGE_LIMIT,
        );

        assert!(app.older_exhausted.contains(&chat.id));
        let ids: Vec<&str> = app.messages[&chat.id]
            .iter()
            .map(|message| message.id().value())
            .collect();
        assert_eq!(ids, vec!["old", "w"]);
        assert!(!app.can_load_older(), "exhausted history cannot page again");
    }

    #[test]
    fn snapshot_replaces_newest_window_but_retains_paged_history() {
        let mut app = App::default();
        let chat = conversation("retain", Some(20));
        app.messages.insert(
            chat.id.clone(),
            vec![message(&chat, "window", 20, "original")],
        );
        app.select(Some(chat.id.clone()));
        let (id, _) = app.begin_load_older().expect("cursor");
        app.finish_load_older(
            id,
            vec![message(&chat, "old", 10, "paged")],
            OLDER_PAGE_LIMIT,
        );
        assert!(app.older_loaded.contains(&chat.id));

        app.apply_snapshot(Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![
                message(&chat, "window", 20, "replaced"),
                message(&chat, "new", 30, "newest"),
            ],
        });

        let texts: Vec<String> = app.messages[&chat.id].iter().map(message_text).collect();
        assert_eq!(texts, vec!["paged", "replaced", "newest"]);
    }

    #[test]
    fn older_history_budget_keeps_the_newest_paged_messages() {
        let mut app = App::default();
        let chat = conversation("budget", Some(10_000_000));
        // History reaches the app through a snapshot, which also records the
        // newest-window boundary the budget is measured against.
        app.set_conversations(vec![chat.clone()]);
        app.apply_snapshot(crate::daemon::Snapshot {
            revision: crate::daemon::next_revision(),
            visible: Some(chat.id.clone()),
            conversations: vec![chat.clone()],
            history: vec![message(&chat, "window", 10_000_000, "window")],
        });

        for round in 0..6i64 {
            let page: Vec<Message> = (0..200i64)
                .map(|offset| {
                    let at = 1_000_000 - (round + 1) * 200 + offset;
                    message(&chat, &format!("r{round}-{offset}"), at, "paged")
                })
                .collect();
            let (id, _) = app.begin_load_older().expect("cursor");
            app.finish_load_older(id, page, OLDER_PAGE_LIMIT);
        }

        let ids: Vec<&str> = app.messages[&chat.id]
            .iter()
            .map(|message| message.id().value())
            .collect();
        assert_eq!(ids.len(), OLDER_HISTORY_BUDGET + 1);
        assert_eq!(
            ids.first(),
            Some(&"r4-0"),
            "oldest page beyond budget trimmed"
        );
        assert!(ids.iter().all(|id| !id.starts_with("r5-")));
        assert_eq!(ids.last(), Some(&"window"));
    }

    #[test]
    fn stale_older_result_does_not_clear_current_loading() {
        let mut app = App::default();
        let first = conversation("first", Some(20));
        let second = conversation("second", Some(20));
        app.messages
            .insert(first.id.clone(), vec![message(&first, "a", 10, "a")]);
        app.messages
            .insert(second.id.clone(), vec![message(&second, "b", 10, "b")]);
        app.select(Some(first.id.clone()));
        let (id, _) = app.begin_load_older().expect("cursor");
        assert_eq!(id, first.id);

        app.finish_load_older(
            second.id.clone(),
            vec![message(&second, "old", 5, "old")],
            1,
        );

        assert_eq!(app.older_loading, Some(first.id.clone()));
        assert_eq!(app.messages[&second.id].len(), 1);
        assert!(!app.older_loaded.contains(&second.id));
    }

    #[test]
    fn alias_migration_carries_older_page_state() {
        let mut app = App::default();
        let legacy = conversation("legacy", Some(20));
        let mut canonical = conversation("canonical", Some(20));
        canonical.aliases = vec![legacy.id.clone()];
        let floor = message(&legacy, "floor", 10, "floor");
        app.selected = Some(legacy.id.clone());
        app.older_loaded.insert(legacy.id.clone());
        app.older_exhausted.insert(legacy.id.clone());
        app.older_loading = Some(legacy.id.clone());
        app.older_window_floor
            .insert(legacy.id.clone(), floor.id().clone());

        app.set_conversations(vec![canonical.clone()]);

        assert_eq!(app.selected, Some(canonical.id.clone()));
        assert!(app.older_loaded.contains(&canonical.id));
        assert!(app.older_exhausted.contains(&canonical.id));
        assert_eq!(app.older_window_floor.get(&canonical.id), Some(floor.id()));
        assert!(!app.older_loaded.contains(&legacy.id));
        assert!(!app.older_exhausted.contains(&legacy.id));
        assert!(!app.older_window_floor.contains_key(&legacy.id));
        assert_eq!(
            app.older_loading, None,
            "an in-flight reply still carries the retired id, so the slot is dropped"
        );

        // The dropped slot must not wedge paging for the canonical conversation.
        app.messages.insert(
            canonical.id.clone(),
            vec![message(&canonical, "only", 30, "only")],
        );
        app.older_exhausted.remove(&canonical.id);
        assert!(app.can_load_older());
        assert!(app.begin_load_older().is_some());
    }

    fn search_hit(conversation: &Conversation, id: &str, snippet: &str) -> MessageSearchHit {
        MessageSearchHit {
            conversation_id: conversation.id.clone(),
            message_id: MessageId::new(conversation.id.transport().clone(), id).unwrap(),
            created_at: 1,
            snippet: snippet.into(),
        }
    }

    fn start_context_lookup(
        app: &mut App,
        hit: MessageSearchHit,
    ) -> (u64, ConversationId, MessageId) {
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search");
        app.finish_search(request, vec![hit]);
        app.begin_search_jump().expect("jump");
        app.advance_search_jump();
        app.begin_search_context_request().expect("context request")
    }

    #[test]
    fn search_modal_parks_and_restores_the_recipient_draft() {
        let mut app = App::default();
        let chat = conversation("search-draft@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        app.draft = "unfinished literal /body".into();
        app.draft_cursor = app.draft.graphemes(true).count();
        app.pasted_body = true;
        let cursor = app.draft_cursor;
        let selected = app.selected.clone();

        app.open_search();

        assert!(app.search.is_some());
        assert!(app.draft.is_empty());
        assert_eq!(app.selected, selected);
        assert!(matches!(
            app.suspended_composition
                .as_ref()
                .and_then(|composition| composition.owner.as_ref()),
            Some(DraftOwner::Conversation(id)) if id == &chat.id
        ));

        app.close_search();

        assert!(app.search.is_none());
        assert_eq!(app.selected, selected);
        assert_eq!(app.draft, "unfinished literal /body");
        assert_eq!(app.draft_cursor, cursor);
        assert!(app.pasted_body);
    }

    #[test]
    fn empty_or_whitespace_search_query_does_not_load() {
        let mut app = App::default();
        app.open_search();

        assert_eq!(app.begin_search(), None);
        assert!(!app.search.as_ref().expect("search state").loading);

        app.search_input(' ');
        app.search_input('\t');

        assert_eq!(app.begin_search(), None);
        assert!(!app.search.as_ref().expect("search state").loading);
    }

    #[test]
    fn completed_search_selects_the_first_hit_and_navigation_clamps() {
        let mut app = App::default();
        let chat = conversation("search-hits@example.test", Some(10));
        let first = search_hit(&chat, "m1", "first needle");
        let second = search_hit(&chat, "m2", "second needle");
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }

        let (request, query) = app.begin_search().expect("search starts");
        assert_eq!(query, "needle");
        assert!(app.search.as_ref().expect("search state").loading);
        app.finish_search(request, vec![first, second]);

        assert!(!app.search.as_ref().expect("search state").loading);
        assert_eq!(app.search.as_ref().expect("search state").selected, 0);
        assert_eq!(app.search.as_ref().expect("search state").hits.len(), 2);
        assert!(app.search_results_visible());
        assert_eq!(
            app.selected_search_hit()
                .map(|hit| hit.message_id.value().to_owned()),
            Some("m1".into())
        );

        app.search_step(-1);
        assert_eq!(app.search.as_ref().expect("search state").selected, 0);
        app.search_step(1);
        assert_eq!(app.search.as_ref().expect("search state").selected, 1);
        app.search_step(1);
        assert_eq!(app.search.as_ref().expect("search state").selected, 1);
        app.search_step(-1);
        assert_eq!(app.search.as_ref().expect("search state").selected, 0);
        app.search_select(99);
        assert_eq!(app.search.as_ref().expect("search state").selected, 1);
    }

    #[test]
    fn search_jump_selects_the_hit_conversation_and_reaches_the_message() {
        let mut app = App::default();
        let source = conversation("search-source@example.test", Some(20));
        let target = conversation("search-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        let hit = search_hit(&target, "needle", "a needle in history");
        let expected = (target.id.clone(), hit.message_id.clone());
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search starts");
        app.finish_search(request, vec![hit]);

        assert_eq!(app.begin_search_jump(), Some(expected.clone()));
        assert_eq!(app.selected, Some(target.id.clone()));
        assert_eq!(app.search_jump_pending(), Some((&expected.0, &expected.1)));
        assert!(!app.search_editable());
        assert!(!app.reach_search_jump());

        app.upsert(
            target.id.clone(),
            message(&target, "needle", 1, "a needle in history"),
        );

        assert!(app.reach_search_jump());
        assert!(app.search_jump_pending().is_none());
        let anchor = app.reading_anchor.as_ref().expect("reading anchor");
        assert_eq!(anchor.conversation, target.id);
        assert_eq!(anchor.message, expected.1);
        assert_eq!(anchor.row, 0);
        assert!(app.scroll > 0);
    }

    #[test]
    fn search_jump_within_the_selected_chat_keeps_drafts_and_paging() {
        let mut app = App::default();
        let chat = conversation("search-same@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        app.draft = "keep this draft".into();
        app.draft_cursor = app.draft.graphemes(true).count();
        app.pasted_body = true;
        let cursor = app.draft_cursor;
        app.older_loaded.insert(chat.id.clone());
        let hit = search_hit(&chat, "m1", "same chat needle");
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search starts");
        app.finish_search(request, vec![hit]);

        let jumped = app.begin_search_jump().expect("jump");

        assert_eq!(jumped.0, chat.id);
        assert_eq!(app.selected, Some(chat.id.clone()));
        assert_eq!(app.draft, "keep this draft");
        assert_eq!(app.draft_cursor, cursor);
        assert!(app.pasted_body);
        assert!(app.older_loaded.contains(&chat.id));
    }

    #[test]
    fn abandoning_a_search_jump_clears_pending_navigation() {
        let mut app = App::default();
        let source = conversation("search-abandon-source@example.test", Some(20));
        let target = conversation("search-abandon-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        let hit = search_hit(&target, "needle", "needle");
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search starts");
        app.finish_search(request, vec![hit]);
        app.begin_search_jump().expect("jump");

        app.abandon_search_jump();

        assert!(app.search_jump_pending().is_none());
        app.upsert(target.id.clone(), message(&target, "needle", 1, "needle"));
        assert!(!app.reach_search_jump());
        assert!(app.reading_anchor.is_none());
    }

    #[test]
    fn search_refuses_to_open_during_a_new_message() {
        let mut app = App::default();
        app.begin_new_conversation();
        app.draft = "an unsent new message".into();
        app.draft_cursor = app.draft.graphemes(true).count();

        app.open_search();

        assert!(app.search.is_none());
        assert!(app.new_conversation);
        assert_eq!(app.draft, "an unsent new message");
        assert!(
            app.notice.is_some(),
            "the refusal explains itself instead of failing silently"
        );
    }

    #[test]
    fn opening_search_dismisses_an_open_filter() {
        let mut app = App::default();
        app.set_conversations(vec![conversation("filtered@example.test", Some(10))]);
        app.begin_filter();
        assert!(app.filter.is_some());

        app.open_search();

        assert!(app.search.is_some());
        assert!(app.filter.is_none(), "the filter must not hide search");
    }

    #[test]
    fn a_stale_search_reply_is_ignored() {
        let mut app = App::default();
        let chat = conversation("stale-search@example.test", Some(10));
        app.open_search();
        for character in "first".chars() {
            app.search_input(character);
        }
        let (stale, _) = app.begin_search().expect("first search");
        for character in "second".chars() {
            app.search_input(character);
        }
        let (current, _) = app.begin_search().expect("second search");
        assert_ne!(stale, current);

        app.finish_search(stale, vec![search_hit(&chat, "old", "stale hit")]);
        assert!(app.search.as_ref().expect("search").hits.is_empty());
        assert!(!app.search_results_visible());

        app.finish_search(current, vec![search_hit(&chat, "new", "current hit")]);
        assert_eq!(
            app.search.as_ref().expect("search").hits[0].snippet,
            "current hit"
        );
        assert!(app.search_results_visible());

        app.fail_search(stale, "old failure".into());
        assert!(app.search.as_ref().expect("search").error.is_none());
    }

    #[test]
    fn editing_the_query_clears_previous_hits_and_enter_searches_again() {
        let mut app = App::default();
        let chat = conversation("edit-search@example.test", Some(10));
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search");
        app.finish_search(request, vec![search_hit(&chat, "hit", "a needle")]);
        assert!(app.search_results_visible());

        app.search_input('x');

        assert!(!app.search_results_visible(), "old hits no longer apply");
        assert!(app.search.as_ref().expect("search").hits.is_empty());
        assert!(
            matches!(
                app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Search { .. }
            ),
            "Enter searches the edited query instead of opening the stale hit"
        );
    }

    #[test]
    fn a_jump_outside_normal_history_starts_one_bounded_context_lookup() {
        let mut app = App::default();
        let source = conversation("wait-source@example.test", Some(20));
        let target = conversation("wait-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        let normal = message(&target, "other", 50, "unrelated normal row");
        app.messages.insert(target.id.clone(), vec![normal.clone()]);
        let hit = search_hit(&target, "needle", "needle");
        let expected = hit.message_id.clone();

        let (generation, conversation, message_id) = start_context_lookup(&mut app, hit);

        assert_eq!(conversation, target.id);
        assert_eq!(message_id, expected);
        assert!(app.search_context_active());
        assert!(app.search_context.as_ref().expect("context").loading);
        assert!(
            app.displayed_messages().is_empty(),
            "loading paints no history"
        );
        assert_eq!(app.messages.get(&target.id), Some(&vec![normal.clone()]));
        assert!(app.search_jump_pending().is_some());
        assert!(app.begin_search_context_request().is_none());
        assert!(!app.older_requested);
        assert!(!app.can_load_older(), "context disables older paging");

        let rows = vec![
            message(&target, "before", 10, "before"),
            message(&target, "needle", 20, "needle"),
            message(&target, "after", 30, "after"),
        ];
        assert!(app.finish_message_context(
            generation,
            conversation.clone(),
            expected.clone(),
            rows.clone()
        ));
        assert_eq!(app.displayed_messages(), rows.as_slice());
        assert_eq!(app.messages.get(&conversation), Some(&vec![normal]));
        assert!(!app.search_context.as_ref().expect("context").loading);
        assert!(app.search_jump_pending().is_none());
        assert_eq!(
            app.reading_anchor.as_ref().map(|anchor| &anchor.message),
            Some(&expected)
        );
    }

    #[test]
    fn search_paste_edits_the_query_without_touching_the_composer() {
        let mut app = App::default();
        let chat = conversation("paste-search@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        app.draft = "recipient draft".into();
        app.draft_cursor = app.draft.graphemes(true).count();
        app.open_search();

        app.paste("two words");

        assert_eq!(app.search_query(), Some("two words"));
        assert!(
            app.draft.is_empty(),
            "the recipient draft stays parked while searching"
        );
        app.close_search();
        assert_eq!(
            app.draft, "recipient draft",
            "closing search restores the recipient draft untouched"
        );
    }

    #[test]
    fn a_failed_context_lookup_restores_normal_history_with_a_notice() {
        let mut app = App::default();
        let source = conversation("never-source@example.test", Some(20));
        let target = conversation("never-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        let normal = message(&target, "other", 50, "normal transcript");
        app.messages.insert(target.id.clone(), vec![normal.clone()]);
        let hit = search_hit(&target, "needle", "needle");

        let (generation, conversation, message_id) = start_context_lookup(&mut app, hit);
        assert!(app.search_context_active());
        app.fail_message_context(generation, conversation.clone(), message_id);

        assert!(!app.search_context_active());
        assert!(app.search_jump_pending().is_none());
        assert_eq!(app.displayed_messages(), std::slice::from_ref(&normal));
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Search context could not be loaded")),
            "a failed exact lookup explains itself without raw IPC details"
        );
    }

    #[test]
    fn stale_context_replies_after_exit_selection_or_replacement_are_ignored() {
        let mut app = App::default();
        let source = conversation("late-source@example.test", Some(20));
        let target = conversation("late-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        let normal = message(&target, "other", 50, "normal");
        app.messages.insert(target.id.clone(), vec![normal.clone()]);
        let hit = search_hit(&target, "needle", "needle");
        let (first, conversation, message_id) = start_context_lookup(&mut app, hit.clone());

        app.scroll_to_newest();
        assert!(!app.search_context_active());
        assert!(!app.finish_message_context(
            first,
            conversation.clone(),
            message_id.clone(),
            vec![message(&target, "needle", 20, "late")]
        ));
        assert_eq!(app.displayed_messages(), std::slice::from_ref(&normal));

        let (second, conversation, message_id) = start_context_lookup(&mut app, hit);
        assert_ne!(first, second);
        app.select(Some(source.id.clone()));
        assert!(!app.finish_message_context(
            second,
            conversation.clone(),
            message_id.clone(),
            vec![message(&target, "needle", 20, "selected away")]
        ));
        assert!(app.search_context.is_none());

        app.select(Some(target.id.clone()));
        let (third, conversation, message_id) =
            start_context_lookup(&mut app, search_hit(&target, "needle", "needle"));
        // A replacement request gets a new generation while the old reply stays stale.
        let (fourth, newer_conversation, newer_message) =
            start_context_lookup(&mut app, search_hit(&target, "needle", "needle"));
        assert_ne!(third, fourth);
        assert!(!app.finish_message_context(
            third,
            conversation,
            message_id,
            vec![message(&target, "needle", 20, "old request")]
        ));
        assert!(app.finish_message_context(
            fourth,
            newer_conversation,
            newer_message,
            vec![message(&target, "needle", 20, "new request")]
        ));
    }

    #[test]
    fn stale_context_failures_after_loading_cancellation_leave_no_notice() {
        for exit in ["latest", "selection", "unread", "new-chat", "replacement"] {
            let mut app = App::default();
            let source = conversation("cancel-error-source", Some(20));
            let target = conversation("cancel-error-target", Some(10));
            app.set_conversations(vec![source.clone(), target.clone()]);
            app.select(Some(source.id.clone()));
            let (generation, conversation, target_id) =
                start_context_lookup(&mut app, search_hit(&target, "needle", "needle"));
            assert!(app.search_context.as_ref().unwrap().loading);
            match exit {
                "latest" => app.scroll_to_newest(),
                "selection" => app.select(Some(source.id.clone())),
                "unread" => {
                    app.jump_to_first_unread();
                }
                "new-chat" => app.begin_new_conversation(),
                "replacement" => {
                    start_context_lookup(&mut app, search_hit(&target, "replacement", "new hit"));
                }
                _ => unreachable!(),
            }
            let selected = app.selected.clone();
            let draft = app.draft.clone();
            let notice = app.notice.clone();
            let active_generation = app
                .search_context
                .as_ref()
                .map(|context| context.generation);
            app.fail_message_context(generation, conversation, target_id);
            assert_eq!(app.notice, notice, "late error after {exit}");
            assert_eq!(app.selected, selected);
            assert_eq!(app.draft, draft);
            assert_eq!(
                app.search_context
                    .as_ref()
                    .map(|context| context.generation),
                active_generation
            );
        }
    }

    #[test]
    fn cold_thread_hits_use_one_exact_context_lookup_without_waiting_for_snapshot() {
        let mut app = App::default();
        let target = conversation("cold-context-thread", Some(10));
        app.set_conversations(vec![target.clone()]);
        assert!(!app.messages.contains_key(&target.id));
        let (generation, conversation, target_id) =
            start_context_lookup(&mut app, search_hit(&target, "needle", "needle"));
        assert!(
            app.begin_search_context_request().is_none(),
            "only one lookup is started"
        );
        assert!(app.finish_message_context(
            generation,
            conversation,
            target_id,
            vec![message(&target, "needle", 20, "exact cold hit")]
        ));
        assert!(app.search_context_active());
        assert_eq!(app.displayed_messages()[0].id().value(), "needle");
        assert!(!app.mark_selected_read_if_following());
    }

    #[test]
    fn snapshots_and_live_arrivals_leave_ready_context_fixed() {
        let mut app = App::default();
        let source = conversation("fixed-source@example.test", Some(20));
        let target = conversation("fixed-target@example.test", Some(10));
        app.set_conversations(vec![source.clone(), target.clone()]);
        app.select(Some(source.id.clone()));
        app.messages.insert(
            target.id.clone(),
            vec![message(&target, "normal", 90, "normal latest")],
        );
        let hit = search_hit(&target, "needle", "needle");
        let (generation, conversation, message_id) = start_context_lookup(&mut app, hit);
        let context_rows = vec![
            message(&target, "before", 10, "context before"),
            message(&target, "needle", 20, "context hit"),
            message(&target, "after", 30, "context after"),
        ];
        assert!(app.finish_message_context(
            generation,
            conversation.clone(),
            message_id,
            context_rows.clone()
        ));

        app.apply_snapshot(Snapshot {
            revision: 1,
            conversations: vec![source, target.clone()],
            visible: Some(target.id.clone()),
            history: vec![message(&target, "reconciled", 100, "newest normal")],
        });
        app.apply_event(EventKind::Message {
            conversation_id: target.id.clone(),
            message: message(&target, "live", 110, "live normal"),
            push_sequence: None,
        });

        assert_eq!(app.displayed_messages(), context_rows.as_slice());
        assert!(app.search_context_active());
        assert!(!app.can_load_older());
    }

    #[test]
    fn alias_changes_restart_context_under_a_new_generation_and_canonical_id() {
        let mut app = App::default();
        let old = conversation("alias-old@example.test", Some(10));
        let mut canonical = conversation("alias-new@example.test", Some(10));
        canonical.aliases = vec![old.id.clone()];
        app.set_conversations(vec![old.clone()]);
        app.select(Some(old.id.clone()));
        let hit = search_hit(&old, "needle", "needle");
        let (old_generation, old_conversation, target) = start_context_lookup(&mut app, hit);

        app.set_conversations(vec![canonical.clone()]);

        let context = app.search_context.as_ref().expect("restarted context");
        assert_ne!(context.generation, old_generation);
        assert_eq!(context.conversation, canonical.id);
        assert_eq!(context.target, target);
        assert!(context.loading);
        assert!(!context.requested);
        assert!(!app.finish_message_context(
            old_generation,
            old_conversation,
            target.clone(),
            vec![message(&old, "needle", 20, "old reply")]
        ));
        let (new_generation, new_conversation, new_target) = app
            .begin_search_context_request()
            .expect("canonical request");
        assert_eq!(new_conversation, canonical.id);
        assert!(app.finish_message_context(
            new_generation,
            new_conversation,
            new_target,
            vec![message(&canonical, "needle", 20, "canonical reply")]
        ));
        assert_eq!(
            app.displayed_messages()
                .first()
                .map(Message::id)
                .map(MessageId::value),
            Some("needle")
        );
    }

    #[test]
    fn context_frames_never_advance_read_markers_even_when_short() {
        let mut app = App::default();
        let chat = conversation("read-context@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let newest = message(&chat, "newest", 90, "newest normal");
        app.messages.insert(chat.id.clone(), vec![newest]);
        let hit = search_hit(&chat, "target", "target");
        let (generation, conversation, target) = start_context_lookup(&mut app, hit);
        assert!(app.finish_message_context(
            generation,
            conversation.clone(),
            target.clone(),
            vec![message(&chat, "target", 10, "short context")]
        ));

        app.record_transcript(Some(crate::transcript::TranscriptViewport {
            conversation: chat.id.clone(),
            messages: vec![crate::transcript::MessageRows {
                id: target,
                start: 0,
                end: 2,
            }],
            rows: 2,
            height: 24,
            top: 0,
        }));

        assert_eq!(app.scroll, 0, "the short context clamps to scroll zero");
        assert!(app.is_scrolled_back(), "context still has a Latest exit");
        assert!(app.read_positions.is_empty());
        assert!(!app.mark_selected_read_if_following());
        assert!(app.read_positions.is_empty());
        app.scroll_to_oldest();
        assert!(!app.older_requested, "context never requests older pages");
        assert!(app.begin_load_older().is_none());
    }

    #[test]
    fn a_near_hit_already_in_normal_history_anchors_without_a_context_request() {
        let mut app = App::default();
        let chat = conversation("near-hit@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        let target = message(&chat, "needle", 20, "near hit");
        app.messages.insert(chat.id.clone(), vec![target.clone()]);
        let hit = search_hit(&chat, "needle", "near hit");
        app.open_search();
        for character in "needle".chars() {
            app.search_input(character);
        }
        let (request, _) = app.begin_search().expect("search");
        app.finish_search(request, vec![hit]);

        app.begin_search_jump().expect("jump");

        assert!(app.search_context.is_none());
        assert!(app.search_jump_pending().is_none());
        assert_eq!(
            app.reading_anchor.as_ref().map(|anchor| &anchor.message),
            Some(target.id())
        );
        assert!(app.begin_search_context_request().is_none());
    }

    #[test]
    fn latest_and_overlays_preserve_draft_owner_cursor_and_paste_provenance() {
        let mut app = App::default();
        let chat = conversation("context-draft@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        app.messages.insert(
            chat.id.clone(),
            vec![message(&chat, "normal", 90, "normal latest")],
        );
        app.draft = "literal /body draft".into();
        app.draft_cursor = 4;
        app.pasted_body = true;
        let hit = search_hit(&chat, "target", "target");
        let (generation, conversation, target) = start_context_lookup(&mut app, hit);
        assert!(app.finish_message_context(
            generation,
            conversation.clone(),
            target.clone(),
            vec![message(&chat, "target", 10, "context")]
        ));
        let owner = app.active_draft_owner.clone();

        app.open_search();
        app.close_search();
        assert!(
            app.search_context_active(),
            "closing an overlay keeps context"
        );
        assert_eq!(app.draft, "literal /body draft");
        assert_eq!(app.draft_cursor, 4);
        assert!(app.pasted_body);
        assert_eq!(app.active_draft_owner, owner);

        app.scroll_to_newest();
        assert!(!app.search_context_active());
        assert_eq!(app.displayed_messages()[0].id().value(), "normal");
        assert_eq!(app.draft, "literal /body draft");
        assert_eq!(app.draft_cursor, 4);
        assert!(app.pasted_body);
        assert_eq!(app.active_draft_owner, owner);

        let unread = message(&chat, "unread", 100, "incoming unread");
        app.messages.insert(chat.id.clone(), vec![unread.clone()]);
        app.mark_read(chat.id.clone(), message(&chat, "normal", 90, "seen"));
        let hit = search_hit(&chat, "target", "target");
        let (generation, conversation, target) = start_context_lookup(&mut app, hit);
        assert!(app.finish_message_context(
            generation,
            conversation,
            target,
            vec![message(&chat, "target", 10, "context")]
        ));
        assert!(app.jump_to_first_unread());
        assert!(
            !app.search_context_active(),
            "unread navigation exits context"
        );
        assert_eq!(
            app.reading_anchor.as_ref().map(|anchor| &anchor.message),
            Some(unread.id())
        );
        assert_eq!(app.draft, "literal /body draft");
        assert_eq!(app.draft_cursor, 4);
        assert!(app.pasted_body);
    }

    #[test]
    fn opening_search_closes_other_overlays() {
        let mut app = App::default();
        let chat = conversation("search-overlay@example.test", Some(10));
        app.set_conversations(vec![chat]);

        app.help_open = true;
        app.open_search();
        assert!(!app.help_open);

        app.open_settings();
        assert!(app.settings_open);
        app.open_search();
        assert!(!app.settings_open);

        app.begin_contacts();
        assert!(app.contacts_open);
        app.open_search();
        assert!(!app.contacts_open);
        assert!(!app.help_open);
        assert!(!app.settings_open);
        assert!(!app.theme_panel_open);
    }
    #[test]
    fn ctrl_f_opens_search_and_its_keys_stay_out_of_the_composer() {
        let mut app = App::default();
        let chat = conversation("search-keys@example.test", Some(10));
        app.set_conversations(vec![chat.clone()]);
        app.select(Some(chat.id.clone()));
        app.draft = "parked draft".into();
        app.draft_cursor = app.draft.graphemes(true).count();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL,)),
            Action::None
        );
        assert!(app.search_editable());
        assert!(app.draft.is_empty());

        for character in "alpha beta".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.search_query(), Some("alpha beta"));
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.search_query(), Some("alpha "));
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.search_query(), Some(""));

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)),
            Action::None,
            "Ctrl+C closes search instead of quitting"
        );
        assert!(!app.search_editable());
        assert_eq!(app.draft, "parked draft");

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.search_editable());
        assert_eq!(app.draft, "parked draft");
    }
    #[test]
    fn slash_search_opens_empty_or_runs_the_argument_query() {
        let mut app = App::default();
        for character in "/search".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert!(app.command_menu);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert!(app.search_editable());
        assert_eq!(app.search_query(), Some(""));
        app.close_search();

        app.draft = "/search needle".into();
        app.draft_cursor = app.draft.graphemes(true).count();
        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(action, Action::Search { ref query, .. } if query == "needle"),
            "an argument runs the query straight away"
        );
        assert_eq!(app.search_query(), Some("needle"));
        assert!(app.search.as_ref().expect("search state").loading);
    }

    fn read_app() -> (App, Conversation) {
        let conversation = conversation("read-chat", None);
        let mut app = App::default();
        app.select(Some(conversation.id.clone()));
        (app, conversation)
    }

    #[test]
    fn a_new_read_marker_requests_one_followup_badge_redraw_not_a_loop() {
        let (mut app, conversation) = read_app();
        let newest = message(&conversation, "painted", 10, "shown");
        app.messages
            .insert(conversation.id.clone(), vec![newest.clone()]);
        let view = crate::transcript::TranscriptViewport {
            conversation: conversation.id.clone(),
            messages: vec![crate::transcript::MessageRows {
                id: newest.id().clone(),
                start: 0,
                end: 1,
            }],
            rows: 1,
            height: 1,
            top: 0,
        };
        assert!(
            app.record_transcript(Some(view.clone())),
            "new marker changes badges"
        );
        assert!(
            !app.record_transcript(Some(view)),
            "unchanged marker must not redraw forever"
        );
        assert!(
            !app.record_transcript(None),
            "no transcript means no marker redraw"
        );
    }

    #[test]
    fn snapshots_and_missing_transcript_frames_do_not_mark_unseen_messages_read() {
        let (mut app, conversation) = read_app();
        let newest = message(&conversation, "m1", 10, "unseen");
        app.apply_snapshot(Snapshot {
            revision: 1,
            conversations: vec![conversation.clone()],
            visible: Some(conversation.id.clone()),
            history: vec![newest],
        });
        assert!(app.read_positions.is_empty());
        app.record_transcript(None);
        assert!(app.read_positions.is_empty());
    }

    #[test]
    fn read_positions_never_move_backwards() {
        let (mut app, conversation) = read_app();
        let older = message(&conversation, "m1", 10, "one");
        let newer = message(&conversation, "m2", 20, "two");
        assert!(app.mark_read(conversation.id.clone(), newer.clone()));
        assert!(!app.mark_read(conversation.id.clone(), older));
        assert_eq!(
            app.read_positions.get(&conversation.id),
            Some(&(20, newer.id().clone()))
        );
    }

    #[test]
    fn unread_counts_ignore_the_stored_position_and_anything_older() {
        let (mut app, conversation) = read_app();
        let rows = vec![
            message(&conversation, "m1", 10, "one"),
            message(&conversation, "m2", 20, "two"),
            message(&conversation, "m3", 30, "three"),
            message(&conversation, "m4", 40, "four"),
        ];
        app.messages.insert(conversation.id.clone(), rows.clone());
        assert_eq!(app.unread_count(&conversation.id), None);

        assert!(app.mark_read(conversation.id.clone(), rows[0].clone()));
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: 3,
                capped: false,
            })
        );

        assert!(app.mark_read(conversation.id.clone(), rows[2].clone()));
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: 1,
                capped: false,
            })
        );
    }

    #[test]
    fn unread_counts_are_capped_only_by_a_full_all_unread_page() {
        let (mut app, conversation) = read_app();
        let rows: Vec<_> = (0..HISTORY_LIMIT)
            .map(|index| {
                message(
                    &conversation,
                    &format!("m{index}"),
                    1_000 + i64::from(index),
                    "unread",
                )
            })
            .collect();
        app.messages.insert(conversation.id.clone(), rows.clone());
        app.read_positions.insert(
            conversation.id.clone(),
            (
                0,
                MessageId::new(conversation.id.transport().clone(), "floor").unwrap(),
            ),
        );
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: HISTORY_LIMIT as usize,
                capped: true,
            })
        );

        assert!(app.mark_read(conversation.id.clone(), rows[0].clone()));
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: HISTORY_LIMIT as usize - 1,
                capped: false,
            })
        );
    }

    #[test]
    fn first_unread_is_the_oldest_unread_message() {
        let (mut app, conversation) = read_app();
        let rows = vec![
            message(&conversation, "m1", 10, "one"),
            message(&conversation, "m2", 20, "two"),
            message(&conversation, "m3", 30, "three"),
        ];
        app.messages.insert(conversation.id.clone(), rows.clone());
        assert!(app.mark_read(conversation.id.clone(), rows[0].clone()));
        assert_eq!(
            app.first_unread(&conversation.id).map(Message::id),
            Some(rows[1].id())
        );

        assert!(app.mark_read(conversation.id.clone(), rows[2].clone()));
        assert!(app.first_unread(&conversation.id).is_none());
    }

    #[test]
    fn marking_read_follows_the_newest_only_while_following() {
        let (mut app, conversation) = read_app();
        let rows = vec![
            message(&conversation, "m1", 10, "one"),
            message(&conversation, "m2", 20, "two"),
        ];
        app.messages.insert(conversation.id.clone(), rows.clone());

        app.scroll = 3;
        assert!(!app.mark_selected_read_if_following());
        assert!(app.read_positions.is_empty());

        app.scroll = 0;
        assert!(app.mark_selected_read_if_following());
        assert_eq!(
            app.read_positions.get(&conversation.id),
            Some(&(20, rows[1].id().clone()))
        );
    }

    #[test]
    fn jump_to_first_unread_anchors_the_oldest_unread() {
        let (mut app, conversation) = read_app();
        let rows = vec![
            message(&conversation, "m1", 10, "one"),
            message(&conversation, "m2", 20, "two"),
            message(&conversation, "m3", 30, "three"),
        ];
        app.messages.insert(conversation.id.clone(), rows.clone());
        assert!(app.mark_read(conversation.id.clone(), rows[0].clone()));

        assert!(app.jump_to_first_unread());
        assert!(app.scroll > 0);
        let anchor = app.reading_anchor.as_ref().expect("reading anchor");
        assert_eq!(anchor.conversation, conversation.id);
        assert_eq!(anchor.message, rows[1].id().clone());
        assert_eq!(anchor.row, 0);

        assert!(app.mark_read(conversation.id.clone(), rows[2].clone()));
        app.scroll = 0;
        app.reading_anchor = None;
        assert!(!app.jump_to_first_unread());
    }

    #[test]
    fn unread_review_aliases_preserve_the_newest_read_position_even_when_not_selected() {
        let old = conversation("old-read-chat", None);
        let mut canonical = conversation("canonical-read-chat", None);
        canonical.aliases = vec![old.id.clone()];
        for old_is_newer in [false, true] {
            let mut app = App::default();
            let old_at = if old_is_newer { 30 } else { 10 };
            let old_message = message(&old, "old-marker", old_at, "seen");
            let canonical_message = message(&canonical, "canonical-marker", 20, "seen");
            app.mark_read(old.id.clone(), old_message.clone());
            app.mark_read(canonical.id.clone(), canonical_message.clone());
            // A config reload also has no selection before metadata arrives.
            let mut restored = App::default();
            restored.apply_read_positions(&app.read_positions_for_config());
            restored.set_conversations(vec![canonical.clone()]);
            let expected = if old_is_newer {
                &old_message
            } else {
                &canonical_message
            };
            assert_eq!(
                restored.read_positions.get(&canonical.id),
                Some(&(expected.created_at, expected.id().clone()))
            );
            assert!(!restored.read_positions.contains_key(&old.id));
            assert!(!restored
                .read_positions_for_config()
                .contains_key(&App::conversation_key(&old.id)));
        }
    }

    #[test]
    fn unread_review_own_messages_are_not_unread_or_navigation_targets() {
        let (mut app, conversation) = read_app();
        let seen = message(&conversation, "seen", 10, "seen");
        let mut sent = message(&conversation, "sent", 20, "own outgoing");
        sent.from_me = true;
        let incoming = message(&conversation, "incoming", 30, "new inbound");
        app.mark_read(conversation.id.clone(), seen.clone());
        app.messages
            .insert(conversation.id.clone(), vec![seen, sent]);
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: 0,
                capped: false
            })
        );
        assert!(app.first_unread(&conversation.id).is_none());
        app.messages
            .get_mut(&conversation.id)
            .unwrap()
            .push(incoming.clone());
        assert_eq!(
            app.unread_count(&conversation.id),
            Some(UnreadCount {
                shown: 1,
                capped: false
            })
        );
        assert_eq!(
            app.first_unread(&conversation.id).map(Message::id),
            Some(incoming.id())
        );
    }

    #[test]
    fn unread_review_navigation_cancels_an_older_search_jump() {
        let (mut app, conversation) = read_app();
        let rows = vec![
            message(&conversation, "seen", 10, "seen"),
            message(&conversation, "unread", 20, "unread"),
            message(&conversation, "old-search-target", 30, "target"),
        ];
        app.mark_read(conversation.id.clone(), rows[0].clone());
        app.messages.insert(conversation.id.clone(), rows.clone());
        app.search_jump = Some((conversation.id.clone(), rows[2].id().clone()));
        app.older_requested = true;
        assert!(app.jump_to_first_unread());
        app.advance_search_jump();
        assert!(app.search_jump.is_none());
        assert!(!app.older_requested);
        assert_eq!(
            app.reading_anchor.as_ref().map(|anchor| &anchor.message),
            Some(rows[1].id())
        );
    }

    #[test]
    fn read_positions_roundtrip_through_config_and_ignore_unknown_keys() {
        let (mut app, conversation) = read_app();
        let newest = message(&conversation, "m1", 10, "one");
        assert!(app.mark_read(conversation.id.clone(), newest.clone()));

        let stored = app.read_positions_for_config();
        assert_eq!(
            stored.get(&App::conversation_key(&conversation.id)),
            Some(&crate::config::StoredReadPosition {
                created_at: 10,
                transport: conversation.id.transport().as_str().to_owned(),
                value: "m1".to_owned(),
            })
        );

        let mut with_unknown = stored.clone();
        with_unknown.insert(
            "not-a-known-key".into(),
            crate::config::StoredReadPosition {
                created_at: 1,
                transport: "x".into(),
                value: "y".into(),
            },
        );
        let mut reloaded = App::default();
        reloaded.apply_read_positions(&with_unknown);
        assert_eq!(
            reloaded.read_positions.get(&conversation.id),
            Some(&(10, newest.id().clone()))
        );
        assert_eq!(reloaded.read_positions.len(), 1);
    }
    #[test]
    fn draft_owner_maps_to_exact_durable_ipc_keys() {
        let chat = conversation("Exact@Example.test", Some(10));
        match DraftOwner::Conversation(chat.id.clone()).durable_key() {
            Some(DraftKey::Conversation { owner }) => assert_eq!(owner, chat.id),
            other => panic!("unexpected conversation key: {other:?}"),
        }
        match DraftOwner::NewChat(Some("Exact@Example.test".into())).durable_key() {
            Some(DraftKey::NewChat { transport, address }) => {
                assert_eq!(transport.as_str(), "openbubbles");
                assert_eq!(address, "Exact@Example.test");
            }
            other => panic!("unexpected new-chat key: {other:?}"),
        }
        assert_eq!(DraftOwner::NewChat(None).durable_key(), None);
    }

    #[test]
    fn uncertainty_survives_retarget_modal_alias_and_recovery() {
        let first = conversation("uncertain-first@example.test", Some(10));
        let second = conversation("uncertain-second@example.test", Some(20));
        let mut app = App::default();
        app.set_conversations(vec![first.clone(), second.clone()]);
        app.select(Some(first.id.clone()));
        app.draft = "uncertain body".into();
        app.draft_cursor = 4;
        app.pasted_body = true;
        app.draft_uncertain = true;

        app.select(Some(second.id.clone()));
        assert!(!app.draft_uncertain);
        app.select(Some(first.id.clone()));
        assert_eq!(app.draft, "uncertain body");
        assert_eq!(app.draft_cursor, 4);
        assert!(app.pasted_body);
        assert!(app.draft_uncertain);

        app.open_search();
        assert!(app
            .suspended_composition
            .as_ref()
            .is_some_and(|snapshot| snapshot.saved.uncertain));
        app.close_search();
        assert!(app.draft_uncertain);
        assert_eq!(app.draft, "uncertain body");

        let old = app.active_draft_owner.clone().expect("active owner");
        let replacement = conversation("uncertain-aliased@example.test", Some(30));
        let new_id = replacement.id.clone();
        app.migrate_draft_owner(&first.id, &new_id);
        assert_eq!(
            app.active_draft_owner,
            Some(DraftOwner::Conversation(new_id.clone()))
        );
        assert_ne!(app.active_draft_owner, Some(old));
        assert!(app.draft_uncertain);

        app.restore_send_draft(RecoveryDraft {
            attempt: 1,
            target: None,
            origin: Some(new_id),
            origin_focus: Focus::Composer,
            address: None,
            body: "failed body".into(),
            uncertain: true,
        });
        assert_eq!(app.draft, "failed body");
        assert!(app.pasted_body);
        assert!(app.draft_uncertain);
        app.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert!(
            app.draft_uncertain,
            "editing must not retry or clear uncertainty"
        );
    }

    #[test]
    fn desired_drafts_include_active_parked_and_suspended_copies() {
        let active = conversation("desired-active@example.test", Some(10));
        let parked = conversation("desired-parked@example.test", Some(20));
        let suspended = conversation("desired-suspended@example.test", Some(30));
        let mut app = App::default();
        app.set_conversations(vec![active.clone(), parked.clone(), suspended.clone()]);
        app.select(Some(active.id.clone()));
        app.draft = "active body".into();
        app.draft_cursor = 6;
        app.draft_uncertain = true;
        app.drafts.insert(
            DraftOwner::Conversation(parked.id.clone()),
            SavedDraft {
                text: "parked body".into(),
                cursor: 6,
                pasted_body: true,
                uncertain: true,
            },
        );
        app.suspended_composition = Some(SuspendedComposition {
            owner: Some(DraftOwner::Conversation(suspended.id.clone())),
            saved: SavedDraft {
                text: "suspended body".into(),
                cursor: 9,
                pasted_body: false,
                uncertain: true,
            },
        });

        let desired = app.desired_drafts();
        assert_eq!(
            desired
                .get(&DraftKey::Conversation {
                    owner: active.id.clone()
                })
                .expect("active draft")
                .body,
            "active body"
        );
        assert_eq!(
            desired
                .get(&DraftKey::Conversation {
                    owner: parked.id.clone()
                })
                .expect("parked draft")
                .body,
            "parked body"
        );
        assert_eq!(
            desired
                .get(&DraftKey::Conversation {
                    owner: suspended.id.clone()
                })
                .expect("suspended draft")
                .body,
            "suspended body"
        );
        assert_eq!(
            app.current_persistable_owner(),
            Some(DraftKey::Conversation {
                owner: active.id.clone()
            })
        );

        let active_key = DraftKey::Conversation {
            owner: active.id.clone(),
        };
        app.draft.clear();
        assert!(!app.desired_drafts().contains_key(&active_key));
    }

    #[test]
    fn loaded_draft_restores_only_an_empty_unowned_active_composer() {
        let chat = conversation("loaded@example.test", Some(10));
        let other = conversation("loaded-other@example.test", Some(20));
        let key = DraftKey::Conversation {
            owner: chat.id.clone(),
        };
        let mut app = App::default();
        app.set_conversations(vec![chat.clone(), other.clone()]);
        app.select(Some(chat.id.clone()));
        let loaded = DurableDraft {
            body: "e\u{301}x".into(),
            cursor: 2,
            pasted_body: true,
            uncertain: true,
        };
        assert!(app.apply_loaded_draft(key.clone(), loaded.clone()));
        assert_eq!(app.draft, "e\u{301}x");
        assert_eq!(app.draft_cursor, app.draft.graphemes(true).count());
        assert!(app.pasted_body);
        assert!(app.draft_uncertain);

        assert!(!app.apply_loaded_draft(key.clone(), loaded.clone()));
        app.draft = "local wins".into();
        assert!(!app.apply_loaded_draft(key.clone(), loaded.clone()));
        assert_eq!(app.draft, "local wins");

        app.draft.clear();
        app.select(Some(other.id.clone()));
        assert!(!app.apply_loaded_draft(key.clone(), loaded.clone()));
        assert!(app.draft.is_empty());

        app.select(Some(chat.id.clone()));
        app.drafts.insert(
            DraftOwner::Conversation(chat.id.clone()),
            SavedDraft {
                text: "newer local".into(),
                cursor: 1,
                pasted_body: false,
                uncertain: false,
            },
        );
        assert!(!app.apply_loaded_draft(key, loaded));
        assert!(app.draft.is_empty());
    }
}
