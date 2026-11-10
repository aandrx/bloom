//! Key-path tests: the real `handle_key` semantics, not just the underlying
//! `App` methods. The `q`-quits-while-filtering bug and the two-step composer
//! both lived in the event loop, where method-level tests could not see them.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::{DraftKey, EventKind, MessageSearchHit, ParticipantFilter};
use bloom_tui::{
    app::{Action, App, DraftOwner, SendPhase, UnreadCount},
    daemon::Snapshot,
    KeyCode, KeyEvent, KeyModifiers,
};

fn transport() -> TransportId {
    TransportId::new("iMessage").unwrap()
}

fn conversation(title: &str, participants: usize) -> Conversation {
    let id = ConversationId::new(transport(), title.to_lowercase().replace(' ', "-")).unwrap();
    let mut people = vec![Participant {
        handle: Handle::new(HandleKind::Email, "me@example.invalid").unwrap(),
        display_name: None,
        is_me: true,
    }];
    for index in 1..participants {
        people.push(Participant {
            handle: Handle::new(
                HandleKind::Email,
                format!(
                    "{}-{index}@example.invalid",
                    title.to_lowercase().replace(' ', "")
                ),
            )
            .unwrap(),
            display_name: None,
            is_me: false,
        });
    }
    Conversation {
        id,
        remote_id: format!("iMessage;-;{title}"),
        kind: if participants > 2 {
            ConversationKind::Group
        } else {
            ConversationKind::Direct
        },
        title: Some(title.to_owned()),
        participants: people,
        created_at: 1,
        last_message_at: Some(10),
        aliases: Vec::new(),
        preview: None,
    }
}

fn message(conversation: &Conversation, id: &str) -> Message {
    Message::new(
        MessageId::new(transport(), id).unwrap(),
        conversation.id.clone(),
        None,
        Handle::new(HandleKind::Opaque, "Maya").unwrap(),
        vec![MessagePart::Text(format!("body {id}"))],
        1_700_000_000_000,
        None,
        DeliveryState::Delivered,
        false,
        None,
        Vec::new(),
        None,
    )
    .unwrap()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn app_with(titles: &[&str]) -> App {
    let mut app = App::default();
    let conversations: Vec<Conversation> =
        titles.iter().map(|title| conversation(title, 2)).collect();
    app.set_conversations(conversations);
    app
}

#[test]
fn q_is_text_and_ctrl_c_clears_before_quitting() {
    let mut app = app_with(&["Quinn Callahan", "Jules Lee"]);

    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.draft, "q");
    assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::None);
    assert!(app.draft.is_empty());
    assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);

    app.begin_filter();
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.filter_query(), Some("q"));
    assert_eq!(app.handle_key(key(KeyCode::Char('u'))), Action::None);
    assert_eq!(app.filter_query(), Some("qu"));
    assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::None);
    assert_eq!(app.filter_query(), Some(""));
    assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);
}

#[test]
fn q_is_text_while_composing_a_message() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.draft, "q");
}

#[test]
fn wrapped_composer_uses_vertical_keys_to_move_the_draft_cursor() {
    let mut app = app_with(&["Weekend crew"]);
    app.draft = "abcdefghijkl".into();
    app.draft_cursor = 12;
    app.composing = true;
    app.composer_columns = 10;
    app.composer_prefix_columns = 3;

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.draft_cursor, 5);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.draft_cursor, 12);
}

#[test]
fn wrapped_composer_preserves_preferred_column_across_short_lines() {
    let mut app = app_with(&["Weekend crew"]);
    app.draft = "123456789 ABCDEFGH x".into();
    app.draft_cursor = 5;
    app.composing = true;
    app.composer_columns = 12;
    app.composer_prefix_columns = 3;

    app.handle_key(key(KeyCode::Down));
    let long_line_cursor = app.draft_cursor;
    app.handle_key(key(KeyCode::Down));
    assert!(app.draft_cursor > long_line_cursor);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.draft_cursor, long_line_cursor);
}

#[test]
fn slash_picker_draft_stays_with_its_chat_after_sidebar_switching() {
    let mut app = app_with(&["Weekend crew", "Jules Lee"]);
    let original = app.selected.clone();
    app.draft = "/new".into();
    app.draft_cursor = 4;
    app.command_menu = true;
    app.focus = bloom_tui::app::Focus::Sidebar;

    app.handle_key(key(KeyCode::Down));
    assert_ne!(app.selected, original);
    assert_eq!(
        app.draft, "",
        "a command draft must not leak into another chat"
    );
    assert!(!app.command_menu);

    // Returning to the original chat restores its in-progress command.
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.selected, original);
    assert_eq!(app.draft, "/new");
    assert!(app.command_menu);

    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.draft, "/ne");
    assert!(app.command_menu);
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
    app.handle_key(key(KeyCode::Char('w')));
    assert_eq!(app.draft, "/new");

    app.draft = "/".into();
    app.draft_cursor = 1;
    app.command_menu = true;
    app.focus = bloom_tui::app::Focus::Sidebar;
    assert_eq!(app.handle_key(key(KeyCode::Tab)), Action::None);
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
    app.handle_key(key(KeyCode::Backspace));
    assert!(app.draft.is_empty());
    assert!(!app.command_menu);
}

#[test]
fn sync_command_returns_an_action_and_reports_truthful_outcomes() {
    use bloom_ipc::ConversationSyncStatus;

    let mut app = app_with(&["Weekend crew"]);
    app.composing = true;
    for character in "/sync".chars() {
        assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
    }
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::SyncConversations
    );
    assert!(!app.sync_requested);
    assert!(app.draft.is_empty());
    app.sync_in_flight = true;
    app.session = "Failed".into();
    assert_eq!(bloom_tui::app::status_text(&app).as_deref(), Some("Failed"));
    assert_eq!(
        bloom_tui::app::status_tone(&app),
        bloom_tui::app::StatusTone::Failure
    );
    app.session = "Active".into();
    assert_eq!(
        bloom_tui::app::status_text(&app).as_deref(),
        Some("Syncing")
    );
    assert_eq!(
        app.notice.as_deref(),
        Some("Syncing conversation-list metadata...")
    );

    app.finish_sync(Ok((ConversationSyncStatus::Complete, 12)));
    assert_eq!(
        app.notice.as_deref(),
        Some("Sync complete: 12 conversations listed.")
    );
    assert!(!app.sync_in_flight);
    app.finish_sync(Ok((ConversationSyncStatus::Limited, 8)));
    assert_eq!(
        app.notice.as_deref(),
        Some("Sync limited after 8 conversations; older chats may be missing.")
    );
    app.finish_sync(Ok((ConversationSyncStatus::Failed, 0)));
    assert!(app
        .error
        .as_deref()
        .unwrap()
        .contains("sync failed after 0 conversations"));
    app.finish_sync(Err("Conversation sync unavailable.".into()));
    assert_eq!(app.error.as_deref(), Some("Conversation sync unavailable."));
}

#[test]
fn slash_picker_completes_commands_and_never_sends_command_text() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    for character in "/th".chars() {
        assert_eq!(app.handle_key(key(KeyCode::Char(character))), Action::None);
    }
    assert!(app.command_menu);
    assert_eq!(
        app.command_suggestions(),
        vec![("/theme", "Choose system, dark, or light theme")]
    );

    // The first Enter opens the theme choices without applying a default.
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.theme_panel_open);
    assert!(!app.command_menu);
    assert!(app.draft.is_empty());
    assert_eq!(app.handle_key(key(KeyCode::Down)), Action::None);
    assert_eq!(app.theme_panel_selection, 1);
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(!app.theme_panel_open);
    assert_eq!(app.theme_override, Some(bloom_tui::app::Theme::Dark));
    assert!(app.draft.is_empty());

    // Explicit theme arguments remain one-step commands.
    for character in "/theme light".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.theme_override, Some(bloom_tui::app::Theme::Light));
    for character in "/theme auto".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.theme_override, None);

    // Unknown leading-slash input stays local even after the menu closes.
    for character in "/unknown".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    let draft = app.draft.clone();
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, draft);
    assert!(app.error.as_deref().unwrap().contains("Unknown command"));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, draft);
}

#[test]
fn slash_picker_dismisses_on_backspace_and_wraps_selection() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    app.handle_key(key(KeyCode::Char('/')));
    assert!(app.command_menu);
    let count = app.command_suggestions().len();
    assert!(count > 1);

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.command_selection, count - 1);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.command_selection, 0);

    app.handle_key(key(KeyCode::Backspace));
    assert!(app.draft.is_empty());
    assert!(!app.command_menu);
}

#[test]
fn contact_tabs_filter_to_only_saved_local_nicknames() {
    let mut app = app_with(&["Maya conversation", "Jules conversation"]);
    let handle = app.contact_handles()[0].clone();
    app.nicknames
        .insert(bloom_tui::app::nickname_key(&handle), "Maya".into());
    app.begin_contacts();
    assert_eq!(app.visible_contact_handles().len(), 2);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.contacts_tab, 1);
    assert_eq!(app.visible_contact_handles().len(), 1);
    assert_eq!(app.selected_contact_handle(), Some(handle));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.contacts_editing.is_some());
}

#[test]
fn contacts_search_matches_numbers_and_nicknames_case_insensitively() {
    let mut phone_chat = conversation("Phone Contact", 2);
    phone_chat.participants[1].handle = Handle::new(HandleKind::Tel, "+1 (555) 123-4567").unwrap();
    let phone = phone_chat.participants[1].handle.clone();
    let mut app = App::default();
    app.set_conversations(vec![phone_chat, conversation("Other Person", 2)]);
    app.nicknames
        .insert(bloom_tui::app::nickname_key(&phone), "Mira Violet".into());
    app.begin_contacts();

    for character in "551234".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.visible_contact_handles(), vec![phone.clone()]);
    assert_eq!(app.selected_contact_handle(), Some(phone.clone()));

    app.handle_key(key(KeyCode::Delete));
    for character in "mIrA vIo".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.visible_contact_handles(), vec![phone.clone()]);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.contacts_tab, 1);
    assert_eq!(app.visible_contact_handles(), vec![phone.clone()]);

    // Contacts are a modal search target: ordinary shortcut letters stay query text.
    app.handle_key(key(KeyCode::Char('n')));
    assert!(app.contacts_open);
    assert_eq!(app.contacts_query, "mIrA vIon");
    assert!(app.visible_contact_handles().is_empty());
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.contacts_editing.is_none());

    app.handle_key(key(KeyCode::Delete));
    assert!(app.contacts_query.is_empty());
    assert_eq!(app.visible_contact_handles(), vec![phone.clone()]);

    app.handle_key(key(KeyCode::Char('x')));
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.contacts_open);
    assert!(app.contacts_query.is_empty());
    app.begin_contacts();
    assert!(app.contacts_query.is_empty());
}

#[test]
fn contacts_search_preserves_selection_when_results_change_and_backspace_clears() {
    let mut app = app_with(&["Alpha Person", "Zed Person"]);
    app.begin_contacts();
    let selected = app.contact_handles()[1].clone();
    app.contacts_selection = 1;

    for character in "zedperson".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.contacts_selection, 0);
    assert_eq!(app.selected_contact_handle(), Some(selected.clone()));

    app.handle_key(key(KeyCode::Char('x')));
    assert!(app.visible_contact_handles().is_empty());
    assert_eq!(app.contacts_selection, 0);
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(
        app.selected_contact_handle(),
        app.contact_handles().get(1).cloned()
    );
    while !app.contacts_query.is_empty() {
        app.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(app.visible_contact_handles().len(), 2);
    assert_eq!(app.contacts_selection, 1);
    assert_eq!(app.selected_contact_handle(), Some(selected));
}

#[test]
fn opening_composer_popups_from_sidebar_focuses_the_composer() {
    let mut app = app_with(&["Weekend crew", "Jules Lee"]);
    app.focus = bloom_tui::app::Focus::Sidebar;
    app.begin_filter();
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.filter_query(), Some("j"));

    app.cancel_filter();
    app.focus = bloom_tui::app::Focus::Sidebar;
    app.begin_new_conversation();
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
    app.handle_key(key(KeyCode::Char('a')));
    assert_eq!(app.draft, "a");

    app.focus = bloom_tui::app::Focus::Sidebar;
    app.open_settings();
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
}

#[test]
fn new_contact_sources_rank_recent_and_all_contacts() {
    let mut older = conversation("Alpha old", 2);
    older.last_message_at = Some(1);
    let mut recent = conversation("Zulu recent", 2);
    recent.last_message_at = Some(100);
    let mut app = App::default();
    app.set_conversations(vec![older, recent]);
    app.begin_new_conversation();
    assert!(app.new_contact_suggestions()[0]
        .value()
        .starts_with("zulurecent"));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.new_contact_tab, 1);
    assert!(app.new_contact_suggestions()[0]
        .value()
        .starts_with("alphaold"));
}

#[test]
fn new_conversation_recommends_contacts_and_enters_the_body_step() {
    let mut app = app_with(&["Weekend crew", "Jules Lee"]);
    app.begin_new_conversation();
    assert_eq!(app.new_contact_suggestions().len(), 2);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.new_contact_selection, 1);
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(
        app.pending_address.as_deref(),
        Some("mailto:weekendcrew-1@example.invalid")
    );
    assert!(app.new_conversation);
    assert!(app.draft.is_empty());

    for character in "hello again".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Action::StartConversation {
            attempt: 1,
            address: "mailto:weekendcrew-1@example.invalid".into(),
            body: "hello again".into(),
        }
    );
}

#[test]
fn filter_tabs_limit_results_to_real_conversation_categories() {
    let mut app = App::default();
    let direct = conversation("Jules Lee", 2);
    let group = conversation("Weekend crew", 3);
    app.set_conversations(vec![direct.clone(), group.clone()]);
    app.begin_filter();
    assert_eq!(app.visible_ids().len(), 2);

    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.filter_category, 1);
    assert_eq!(app.visible_ids(), vec![direct.id]);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.filter_category, 2);
    assert_eq!(app.visible_ids(), vec![group.id.clone()]);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.filter_query(), None);
    assert_eq!(app.selected.as_ref(), Some(&group.id));
}

#[test]
fn refresh_cannot_select_a_chat_outside_the_active_filter() {
    let mut app = app_with(&["Alice", "Bob"]);
    app.begin_filter();
    app.filter_category = 1; // Direct only.
    let mut refreshed = app.conversations.clone();
    for chat in &mut refreshed {
        chat.kind = ConversationKind::Group;
    }
    app.set_conversations(refreshed);
    assert!(app.visible_conversations().is_empty());
    assert!(app.selected.is_none());
    app.cancel_filter();
    app.set_conversations(app.conversations.clone());
    assert!(app.selected.is_some());
}

#[test]
fn settings_switched_to_contacts_keeps_original_recipient_draft() {
    let mut app = app_with(&["Alice", "Bob"]);
    let alice = app.selected.clone().unwrap();
    app.paste("Private Alice");
    app.open_settings();
    app.begin_contacts();
    app.close_contacts();
    assert_eq!(app.draft, "Private Alice");
    app.open_settings();
    let bob = app
        .conversations
        .iter()
        .find(|c| c.id != alice)
        .unwrap()
        .id
        .clone();
    app.select(Some(bob));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.draft.is_empty());
    app.select(Some(alice));
    assert_eq!(app.draft, "Private Alice");
}

#[test]
fn modal_draft_suspend_preserves_body_cursor_and_literal_paste_provenance() {
    let mut app = app_with(&["Alice"]);
    app.paste("/new");
    app.paste(" literal body");
    app.draft_cursor = 5;
    app.open_settings();
    assert!(app.draft.is_empty());
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.draft, "/new literal body");
    assert_eq!(app.draft_cursor, 5);
    assert!(app.pasted_body);
    assert!(!app.command_menu);
}

#[test]
fn new_chat_body_does_not_follow_changed_recipient_and_modal_restores_matching_context() {
    let mut app = app_with(&["Alice"]);
    app.new_conversation = true;
    app.pending_address = Some("mailto:carol@example.invalid".into());
    app.draft = "/new Carol body".into();
    app.draft_cursor = 5;
    app.pasted_body = true;
    app.open_settings();
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.pending_address.as_deref(),
        Some("mailto:carol@example.invalid")
    );
    assert_eq!(app.draft, "/new Carol body");
    app.open_settings();
    app.pending_address = Some("mailto:dave@example.invalid".into());
    app.handle_key(key(KeyCode::Esc));
    assert!(app.draft.is_empty(), "Carol's body must not route to Dave");
}

#[test]
fn tab_switches_focus_while_panels_are_open() {
    let mut app = app_with(&["Weekend crew", "Jules Lee"]);
    app.open_settings();
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, bloom_tui::app::Focus::Sidebar);
    assert!(app.settings_open);
    let before = app.selected.clone();
    app.handle_key(key(KeyCode::Down));
    assert_ne!(app.selected, before);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);

    app.begin_contacts();
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, bloom_tui::app::Focus::Sidebar);
    assert!(app.contacts_open);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.focus, bloom_tui::app::Focus::Composer);
}

#[test]
fn emoji_shortcodes_autocomplete_at_the_cursor_without_losing_suffix_text() {
    let mut app = App {
        composing: true,
        draft: "hello :smi!".into(),
        draft_cursor: "hello :smi".chars().count(),
        ..Default::default()
    };
    let suggestions = app.emoji_suggestions();
    assert!(suggestions
        .iter()
        .any(|suggestion| suggestion.shortcode == "smile"));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, "hello 😄!");
    assert_eq!(app.draft_cursor, "hello 😄".chars().count());
    assert!(app.emoji_suggestions().is_empty());
}

#[test]
fn direct_theme_command_opens_choices_on_first_enter() {
    let mut app = App {
        composing: true,
        ..Default::default()
    };
    for character in "/theme".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(app.command_menu);
    app.handle_key(key(KeyCode::Enter));
    assert!(app.theme_panel_open);
    assert!(app.draft.is_empty());
}

#[test]
fn herdr_style_leader_chords_navigate_without_breaking_composition() {
    let mut app = app_with(&["Weekend crew", "Jules Lee", "Design systems"]);
    let first = app.selected.clone();
    app.handle_key(ctrl(KeyCode::Char('b')));
    assert!(app.leader_pending);
    app.handle_key(key(KeyCode::Char('l')));
    assert!(!app.leader_pending);
    assert_ne!(app.selected, first);

    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(app.selected, first);

    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('n')));
    assert_ne!(app.selected, first, "prefix+n selects the next chat");
    assert!(!app.new_conversation);

    // Ctrl+B is a leader even with the composer focused, because the composer is
    // always focused; the chord must not be swallowed as draft text.
    let mut composing = app_with(&["Jules Lee"]);
    composing.composing = true;
    composing.handle_key(ctrl(KeyCode::Char('b')));
    assert!(composing.leader_pending);
    assert!(
        composing.draft.is_empty(),
        "Ctrl+B is not typed into the draft"
    );
    composing.handle_key(key(KeyCode::Char('s')));
    assert!(composing.settings_open, "Ctrl+B s opens settings");
}

#[test]
fn drafts_follow_their_chat_even_when_a_snapshot_establishes_the_selection() {
    let alice = conversation("Alice", 2);
    let bob = conversation("Bob", 2);
    let mut app = App::default();
    // An initial snapshot selects the first chat before any key is pressed; that
    // selection must still establish draft ownership.
    app.apply_snapshot(Snapshot {
        token: None,
        revision: 1,
        visible: Some(alice.id.clone()),
        conversations: vec![alice.clone(), bob.clone()],
        history: Vec::new(),
    });
    for ch in "draft for alice".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.select_step(1);
    assert_eq!(
        app.draft, "",
        "a draft must never follow the user to another chat"
    );
    app.select_step(-1);
    assert_eq!(app.draft, "draft for alice");
}

#[test]
fn ctrl_p_selects_the_previous_chat_and_keeps_drafts_with_their_own_chat() {
    let mut app = app_with(&["Weekend crew", "Jules Lee", "Design systems"]);
    let first = app.selected.clone();
    app.select_step(1);
    let second = app.selected.clone();
    assert_ne!(second, first);
    app.composing = true;
    app.draft = "keep this draft".into();
    app.draft_cursor = app.draft.len();

    assert_eq!(app.handle_key(ctrl(KeyCode::Char('p'))), Action::None);
    assert_eq!(app.selected, first);
    assert_eq!(
        app.draft, "",
        "a draft must never follow the user to another chat"
    );

    app.select_step(1);
    assert_eq!(app.selected, second);
    assert_eq!(
        app.draft, "keep this draft",
        "returning to a chat restores its own draft"
    );
}

#[test]
fn slash_actions_open_new_chat_and_filter_or_toggle_the_sidebar() {
    let mut new_chat = app_with(&["Jules Lee"]);
    new_chat.composing = true;
    for character in "/new".chars() {
        new_chat.handle_key(key(KeyCode::Char(character)));
    }
    new_chat.handle_key(key(KeyCode::Enter));
    assert!(new_chat.new_conversation);
    assert!(new_chat.composing);
    assert_eq!(
        new_chat.composer_prompt(),
        bloom_tui::app::ComposerPrompt::Recipient
    );

    let mut filter = app_with(&["Jules Lee"]);
    filter.composing = true;
    for character in "/filter".chars() {
        filter.handle_key(key(KeyCode::Char(character)));
    }
    filter.handle_key(key(KeyCode::Enter));
    assert_eq!(filter.filter_query(), Some(""));
    assert!(!filter.composing);

    let mut compact = app_with(&["Jules Lee"]);
    compact.composing = true;
    for character in "/compact".chars() {
        compact.handle_key(key(KeyCode::Char(character)));
    }
    compact.handle_key(key(KeyCode::Enter));
    assert!(compact.compact_sidebar);
    assert!(compact.composing);
}

fn submit_pending(app: &mut App) -> (u64, Option<ConversationId>) {
    let (attempt, target) = match app.handle_key(key(KeyCode::Enter)) {
        Action::SendMessage {
            attempt,
            conversation,
            ..
        } => (attempt, Some(conversation)),
        Action::StartConversation { attempt, .. } => (attempt, None),
        _ => panic!("actual send expected"),
    };
    app.begin_send(attempt, target.clone());
    (attempt, target)
}

#[test]
fn pending_submission_preserves_exact_newer_draft_and_blocks_recovery() {
    let mut app = App {
        selected: Some(conversation("a", 1).id),
        draft: "original".into(),
        ..Default::default()
    };
    let (attempt, target) = submit_pending(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(
        app.draft.is_empty(),
        "An in-flight message is not a recovery draft"
    );
    app.draft = "  newer draft  ".into();
    app.draft_cursor = 3;
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, "  newer draft  ");
    assert_eq!(app.draft_cursor, 3);
    app.finish_send(attempt, target, Ok(()));
    assert_eq!(app.draft, "  newer draft  ");
}

#[test]
fn deferred_recovery_never_routes_original_body_into_another_recipient_prompt() {
    let a = conversation("a", 1).id;
    let mut app = App {
        selected: Some(a.clone()),
        draft: "for a only".into(),
        ..Default::default()
    };
    let (attempt, target) = submit_pending(&mut app);
    app.draft = "newer".into();
    app.finish_send(attempt, target, Err("rejected".into()));
    app.selected = Some(conversation("b", 1).id);
    app.draft.clear();
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(app.draft.is_empty());
    app.selected = Some(a.clone());
    app.new_conversation = true;
    app.pending_address = Some("mailto:wrong@example.invalid".into());
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(
        app.draft.is_empty(),
        "Do not restore into an unrelated recipient prompt"
    );
    app.new_conversation = false;
    app.pending_address = None;
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.draft, "for a only");
    match app.handle_key(key(KeyCode::Enter)) {
        Action::SendMessage {
            conversation, body, ..
        } => {
            assert_eq!(conversation, a);
            assert_eq!(body, "for a only");
        }
        _ => panic!("Recovered draft must retain its original target"),
    }
}

#[test]
fn deferred_failure_survives_a_later_successful_new_conversation() {
    let a = conversation("a", 1).id;
    let mut app = App {
        selected: Some(a.clone()),
        draft: "older failed body".into(),
        ..Default::default()
    };
    let (first, target) = submit_pending(&mut app);
    app.draft = "newer first message".into();
    app.finish_send(first, target, Err("rejected".into()));
    app.new_conversation = true;
    app.pending_address = Some("mailto:carol@example.invalid".into());
    let (second, target) = submit_pending(&mut app);
    app.finish_send(second, target, Ok(()));
    app.selected = Some(a);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.draft, "older failed body");
}

#[test]
fn failed_start_after_changing_chat_requires_explicit_recovery() {
    let mut app = App {
        selected: Some(conversation("a", 1).id),
        new_conversation: true,
        pending_address: Some("mailto:carol@example.invalid".into()),
        draft: "for carol".into(),
        ..Default::default()
    };
    let (attempt, target) = submit_pending(&mut app);
    app.selected = Some(conversation("b", 1).id);
    app.finish_send(attempt, target, Err("rejected".into()));
    assert!(
        app.draft.is_empty(),
        "Changed chat must not receive automatic restoration"
    );
    assert!(!app.new_conversation);
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.draft, "for carol");
    assert_eq!(
        app.pending_address.as_deref(),
        Some("mailto:carol@example.invalid")
    );
}

#[test]
fn stale_send_results_cannot_retire_a_new_pending_attempt() {
    let mut app = App {
        selected: Some(conversation("a", 1).id),
        draft: "first".into(),
        ..Default::default()
    };
    let (first, target) = submit_pending(&mut app);
    app.finish_send(first, target.clone(), Ok(()));
    app.draft = "second".into();
    let (second, second_target) = submit_pending(&mut app);
    app.finish_send(first, target, Err("late stale error".into()));
    assert_eq!(app.pending_send_attempt, Some(second));
    assert!(app.error.is_none());
    app.finish_send(second, second_target, Err("rejected".into()));
    assert_eq!(app.draft, "second");
}

#[test]
fn uncertain_failures_keep_warning_during_auto_and_explicit_recovery() {
    for deferred in [false, true] {
        let mut app = App {
            selected: Some(conversation("a", 1).id),
            draft: "uncertain body".into(),
            ..Default::default()
        };
        let (attempt, target) = submit_pending(&mut app);
        if deferred {
            app.draft = "newer text".into();
        }
        app.finish_send(
            attempt,
            target,
            Err(bloom_ipc::client::ClientError::Timeout.into()),
        );
        assert!(app.error.as_deref().unwrap().contains("uncertain"));
        assert!(!app.error.as_deref().unwrap().contains("not sent"));
        if deferred {
            assert_eq!(app.draft, "newer text");
            assert!(app.deferred_sends.front().unwrap().uncertain);
            app.draft.clear();
            app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
            assert!(app.notice.as_deref().unwrap().contains("uncertain"));
        }
        assert_eq!(app.draft, "uncertain body");
        assert!(
            app.pending_send_attempt.is_none(),
            "Recovery never automatically sends"
        );
    }
}

#[test]
fn failed_send_in_another_popup_waits_for_safe_explicit_recovery() {
    let mut app = App {
        selected: Some(conversation("a", 1).id),
        draft: "body".into(),
        ..Default::default()
    };
    let (attempt, target) = submit_pending(&mut app);
    app.settings_open = true;
    app.finish_send(attempt, target, Err("rejected".into()));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert!(app.draft.is_empty());
    assert_eq!(app.deferred_sends.len(), 1);
    app.settings_open = false;
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.draft, "body");
}

#[test]
fn new_chat_receipt_event_and_metadata_can_arrive_in_any_order() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let alice = conversation("Alice", 2);
        let carol = conversation("Carol", 2);
        let mut sent = message(&carol, "acknowledged-new");
        sent.from_me = true;
        let mut app = App::default();
        app.set_conversations(vec![alice.clone()]);
        app.new_conversation = true;
        app.pending_address = Some("mailto:carol@example.invalid".into());
        app.draft = "first Carol message".into();
        let (attempt, target) = submit_pending(&mut app);
        for step in order {
            match step {
                0 => app.finish_send_receipt(attempt, target.clone(), Ok(sent.id().clone())),
                1 => app.apply_event(EventKind::Message {
                    conversation_id: carol.id.clone(),
                    message: sent.clone(),
                    push_sequence: None,
                }),
                2 => app.set_conversations(vec![alice.clone(), carol.clone()]),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            app.selected.as_ref(),
            Some(&carol.id),
            "arrival order {order:?}"
        );
        assert_eq!(app.messages[&carol.id][0].id(), sent.id());
        assert_eq!(
            app.visible_send_feedback().unwrap().conversation.as_ref(),
            Some(&carol.id)
        );
    }
}

#[test]
fn new_chat_completion_does_not_steal_changed_context_or_match_unrelated_messages() {
    for changed in [0, 1, 2] {
        let alice = conversation("Alice", 2);
        let carol = conversation("Carol", 2);
        let mut sent = message(&carol, "expected-receipt");
        sent.from_me = true;
        let mut app = App::default();
        app.set_conversations(vec![alice.clone(), carol.clone()]);
        app.new_conversation = true;
        app.pending_address = Some("mailto:carol@example.invalid".into());
        app.draft = "first".into();
        let (attempt, target) = submit_pending(&mut app);
        app.finish_send_receipt(attempt + 100, target.clone(), Ok(sent.id().clone()));
        assert_eq!(app.pending_send_attempt, Some(attempt));
        app.apply_event(EventKind::Message {
            conversation_id: carol.id.clone(),
            message: message(&carol, "unrelated"),
            push_sequence: None,
        });
        assert_eq!(app.selected, Some(alice.id.clone()));
        match changed {
            0 => app.draft = "newer draft".into(),
            1 => app.select(Some(carol.id.clone())),
            2 => app.settings_open = true,
            _ => unreachable!(),
        }
        let selected = app.selected.clone();
        app.finish_send_receipt(attempt, target, Ok(sent.id().clone()));
        app.apply_event(EventKind::Message {
            conversation_id: carol.id.clone(),
            message: sent,
            push_sequence: None,
        });
        assert_eq!(app.selected, selected);
        if changed == 0 {
            assert_eq!(app.draft, "newer draft");
        }
    }
}

#[test]
fn rejected_send_retry_submits_the_body_once() {
    let mut app = App::default();
    let target = conversation("retry-once", 1);
    app.selected = Some(target.id.clone());
    app.active_draft_owner = Some(DraftOwner::Conversation(target.id.clone()));
    app.draft = "RetryBody".into();
    app.draft_cursor = app.draft.chars().count();
    let Action::SendMessage {
        attempt,
        conversation,
        body,
    } = app.handle_key(key(KeyCode::Enter))
    else {
        panic!("send expected")
    };
    assert_eq!(body, "RetryBody");
    app.begin_send(attempt, Some(conversation.clone()));
    app.finish_send(attempt, Some(conversation), Err("rejected".into()));
    assert_eq!(app.draft, "RetryBody");
    let Action::SendMessage { body, .. } = app.handle_key(key(KeyCode::Enter)) else {
        panic!("retry expected")
    };
    assert_eq!(
        body, "RetryBody",
        "retry must not duplicate the recovered body"
    );
}

#[test]
fn failed_send_recovers_draft_and_success_retires_it() {
    let mut app = App::default();
    let target = Some(conversation("recover-send", 1).id);
    app.selected = target.clone();
    app.draft = "keep this".into();
    let action = app.handle_key(key(KeyCode::Enter));
    let Action::SendMessage { attempt, .. } = action else {
        panic!("send expected")
    };
    app.begin_send(attempt, target.clone());
    app.finish_send(attempt, target.clone(), Err("rejected".into()));
    assert_eq!(app.draft, "keep this");

    app.draft.clear();
    app.handle_key(key(KeyCode::Char('x')));
    let Action::SendMessage { attempt, .. } = app.handle_key(key(KeyCode::Enter)) else {
        panic!("retry expected")
    };
    app.begin_send(attempt, target.clone());
    app.finish_send(attempt, target, Ok(()));
    assert!(app.recoverable_send.is_none());
}

#[test]
fn pending_send_parks_one_durable_copy_and_failure_clears_it() {
    let mut app = App::default();
    let target = conversation("parked-send", 1);
    let draft_key = DraftKey::Conversation {
        owner: target.id.clone(),
    };
    app.selected = Some(target.id.clone());
    app.active_draft_owner = Some(DraftOwner::Conversation(target.id.clone()));
    app.draft = "parked body".into();
    app.draft_cursor = app.draft.chars().count();
    let Action::SendMessage {
        attempt,
        conversation,
        ..
    } = app.handle_key(key(KeyCode::Enter))
    else {
        panic!("send expected")
    };
    app.begin_send(attempt, Some(conversation.clone()));
    assert!(app.draft.is_empty(), "the composer retires after submit");
    let desired = app.desired_drafts();
    assert_eq!(
        desired.get(&draft_key).expect("parked copy").body,
        "parked body"
    );
    assert_eq!(
        desired.len(),
        1,
        "the parked body is not duplicated in the composer"
    );

    app.finish_send(attempt, Some(conversation), Err("rejected".into()));
    assert!(app.parked_send.is_none(), "failure clears the parked copy");
    assert_eq!(app.draft, "parked body", "recovery keeps the text editable");
    assert_eq!(
        app.desired_drafts()
            .get(&draft_key)
            .expect("recovered copy")
            .body,
        "parked body"
    );
}

#[test]
fn failed_start_recovers_recipient_and_body_even_with_selected_chat() {
    let mut app = App {
        selected: Some(conversation("existing", 1).id),
        new_conversation: true,
        pending_address: Some("mailto:new@example.test".into()),
        draft: "first message".into(),
        ..Default::default()
    };
    let Action::StartConversation { attempt, .. } = app.handle_key(key(KeyCode::Enter)) else {
        panic!("start expected")
    };
    app.begin_send(attempt, None);
    app.finish_send(attempt, None, Err("rejected".into()));
    assert_eq!(app.draft, "first message");
    assert_eq!(
        app.pending_address.as_deref(),
        Some("mailto:new@example.test")
    );
    assert!(app.new_conversation);
}

#[test]
fn changed_context_recovery_never_overwrites_new_text_and_ctrl_r_recovers() {
    let mut app = App::default();
    let target = Some(conversation("recover-context", 1).id);
    app.selected = target.clone();
    app.draft = "original".into();
    let Action::SendMessage { attempt, .. } = app.handle_key(key(KeyCode::Enter)) else {
        panic!("send expected")
    };
    app.begin_send(attempt, target);
    app.draft = "newer text".into();
    app.finish_send(attempt, app.selected.clone(), Err("rejected".into()));
    assert_eq!(app.draft, "newer text");
    app.draft.clear();
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.draft, "original");
}

#[test]
fn recipient_step_does_not_create_recoverable_send() {
    let mut app = App {
        new_conversation: true,
        draft: "new@example.test".into(),
        ..Default::default()
    };
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.recoverable_send.is_none());
}

#[test]
fn send_feedback_is_contextual_and_never_claims_delivery() {
    let mut app = app_with(&["Jules Lee", "Quinn Callahan"]);
    let target = app.selected.clone();
    app.begin_send(1, target.clone());
    assert_eq!(
        app.visible_send_feedback().unwrap().phase,
        SendPhase::Sending
    );

    app.select_step(1);
    assert!(app.visible_send_feedback().is_none());
    app.finish_send(1, target.clone(), Ok(()));
    assert!(app.visible_send_feedback().is_none());
    app.selected = target;
    assert_eq!(app.visible_send_feedback().unwrap().phase, SendPhase::Sent);
    app.handle_key(key(KeyCode::Char('j')));
    assert!(app.send_feedback.is_none());

    app.begin_send(2, None);
    app.finish_send(2, None, Err("transport unavailable".into()));
    assert!(app
        .error
        .as_deref()
        .unwrap()
        .contains("transport unavailable"));
    assert!(app.send_feedback.is_none());
}

#[test]
fn local_settings_are_keyboard_accessible_and_theme_can_return_to_automatic() {
    use bloom_tui::app::Theme;

    let mut app = app_with(&["Jules Lee"]);
    app.system_theme = Theme::Light;
    app.theme = Theme::Light;
    app.composing = true;
    for character in "/settings".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.settings_open);
    assert!(!app.composing);
    assert_eq!(app.draft, "");

    // Appearance cycles Automatic, Dark, and Light with Enter.
    assert_eq!(app.theme, Theme::Light);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.theme_override, Some(Theme::Dark));
    assert_eq!(app.theme, Theme::Dark);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.theme_override, Some(Theme::Light));

    // Left/Right switches categories; Up/Down selects Layout toggles.
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.settings_category, 1);
    app.handle_key(key(KeyCode::Enter));
    assert!(app.compact_sidebar);
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Char(' ')));
    assert!(!app.wrap_composer);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.settings_category, 0);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.theme_override, None);
    assert_eq!(app.theme, Theme::Light);
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.settings_open);
}

#[test]
fn direct_chat_nicknames_are_local_filterable_and_clearable() {
    let conversation = conversation("Jules Lee", 2);
    let peer = conversation
        .participants
        .iter()
        .find(|participant| !participant.is_me)
        .unwrap()
        .handle
        .clone();
    let peer_key = bloom_tui::app::nickname_key(&peer);
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    for character in "/nickname Maya Chen".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(!app.command_menu);
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(
        app.nicknames.get(&peer_key).map(String::as_str),
        Some("Maya Chen")
    );
    assert_eq!(
        app.conversation_name(app.selected_conversation().unwrap()),
        "Maya Chen"
    );
    assert!(app.notice.as_deref().unwrap().contains("Maya Chen"));

    app.filter = Some("maya".into());
    assert_eq!(app.visible_conversations().len(), 1);
    app.cancel_filter();

    app.composing = true;
    for character in "/nickname clear".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(!app.nicknames.contains_key(&peer_key));
    assert_eq!(
        app.conversation_name(app.selected_conversation().unwrap()),
        "Jules Lee"
    );

    let mut incoming = message(&conversation, "nicknamed");
    incoming.sender = peer;
    assert_eq!(
        app.speaker_label_for(&incoming, Some(&conversation)),
        "Juleslee-1"
    );
}

#[test]
fn groups_reject_direct_chat_nickname_overrides() {
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 3)]);
    app.composing = true;
    for character in "/nickname Maya".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(!app.command_menu);
    app.handle_key(key(KeyCode::Enter));
    assert!(app.nicknames.is_empty());
    assert!(app.error.as_deref().unwrap().contains("direct chats only"));
}

#[test]
fn escape_dismisses_command_menu_without_losing_draft_and_slashes_in_prose_send_normally() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    for character in "/help".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    let draft = app.draft.clone();
    let cursor = app.draft_cursor;
    assert!(app.command_menu);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
    assert!(!app.command_menu);
    assert!(app.composing);
    assert_eq!(app.draft, draft);
    assert_eq!(app.draft_cursor, cursor);

    // `/help` opens the command popup without producing an error.
    app.draft = "/help".into();
    app.draft_cursor = 5;
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.command_menu);
    assert!(app.error.is_none());

    let mut prose = app_with(&["Jules Lee"]);
    prose.composing = true;
    let text = "See https://example.test/a";
    for character in text.chars() {
        prose.handle_key(key(KeyCode::Char(character)));
    }
    assert!(!prose.command_menu);
    let expected_conversation = prose.selected.clone().unwrap();
    assert_eq!(
        prose.handle_key(key(KeyCode::Enter)),
        Action::SendMessage {
            attempt: 1,
            conversation: expected_conversation,
            body: text.to_owned(),
        }
    );
}

#[test]
fn slash_in_list_mode_still_opens_the_conversation_filter() {
    // `/` opens the command menu now; the filter has its own route. What matters
    // for this guard is that once the filter owns the keyboard, letters are query
    // text rather than bindings.
    let mut app = app_with(&["Jules Lee"]);
    assert_eq!(app.handle_key(key(KeyCode::Char('/'))), Action::None);
    assert!(app.command_menu, "/ opens the command menu");
    assert_eq!(app.filter_query(), None);

    app.cancel_filter();
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('f')));
    assert_eq!(app.filter_query(), Some(""));
    assert!(!app.composing);
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.filter_query(), Some("q"), "q is filter text");
}

#[test]
fn ctrl_c_quits_from_every_mode() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    let mut list = app_with(&["Jules Lee"]);
    assert_eq!(list.handle_key(ctrl_c), Action::Quit);

    let mut filtering = app_with(&["Jules Lee"]);
    filtering.begin_filter();
    assert_eq!(filtering.handle_key(ctrl_c), Action::Quit);

    let mut composing = app_with(&["Jules Lee"]);
    composing.composing = true;
    assert_eq!(composing.handle_key(ctrl_c), Action::Quit);
}

#[test]
fn esc_cancels_the_filter_and_leaves_a_usable_selection() {
    let mut app = app_with(&["Jules Lee", "Maya Chen"]);
    let before = app.selected.clone();

    app.begin_filter();
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
    assert_eq!(app.filter_query(), None);
    assert_eq!(
        app.selected, before,
        "cancelling must not move the selection"
    );
    assert!(
        app.selected_conversation().is_some(),
        "the selection must still point at a real conversation"
    );
}

#[test]
fn enter_with_no_matches_neither_selects_nor_sends() {
    let mut app = app_with(&["Jules Lee"]);
    app.begin_filter();
    for character in "zzzz".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(app.filter_has_no_matches());

    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.filter_query(), None);
    assert_eq!(
        app.selected, None,
        "an empty result set must not leave a stale selection"
    );
}

#[test]
fn arrows_move_only_through_conversations_matching_the_filter() {
    let mut app = app_with(&["Jules Lee", "Maya Chen", "Jules Bae"]);
    app.begin_filter();
    for character in "jules".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(
        app.visible_conversations().len(),
        2,
        "two Jules threads match"
    );

    let first = app.selected.clone();
    app.handle_key(key(KeyCode::Down));
    let second = app.selected.clone();
    assert_ne!(first, second, "the next match must be selected");

    // Every conversation reachable while filtering must itself match the query.
    for _ in 0..4 {
        app.handle_key(key(KeyCode::Down));
        let selected = app
            .selected_conversation()
            .expect("a matching conversation stays selected");
        assert!(
            selected
                .title
                .as_deref()
                .is_some_and(|t| t.contains("Jules")),
            "arrows must not reach a filtered-out conversation: {selected:?}"
        );
    }
}

#[test]
fn a_snapshot_that_removes_the_selected_conversation_recovers() {
    let mut app = app_with(&["Jules Lee", "Maya Chen"]);
    let removed = app.selected.clone().expect("something is selected");
    app.begin_filter();
    for character in "jules".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.selected, Some(removed.clone()));

    // The conversation disappears while the filter is still narrowing the list.
    app.set_conversations(vec![conversation("Maya Chen", 2)]);
    app.begin_filter();
    for character in "jules".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(
        app.selected.is_none() || app.selected_conversation().is_some(),
        "the selection must never point at a conversation that no longer exists"
    );
}

#[test]
fn narrow_resize_does_not_replace_an_open_overlay_with_chats() {
    let alice = conversation("Alice", 2);
    let mut app = App::default();
    app.set_conversations(vec![alice.clone()]);
    app.set_terminal_width(100);
    app.focus = bloom_tui::app::Focus::Sidebar;
    app.handle_key(key(KeyCode::Char('?')));

    app.set_terminal_width(44);

    assert!(app.help_open);
    assert!(
        app.chat_drawer.is_none(),
        "resizing must preserve the current overlay input owner"
    );

    app.help_open = false;
    app.set_terminal_width(100);
    app.open_context_menu(alice.id);
    app.set_terminal_width(44);
    assert!(
        app.context_menu.is_none(),
        "a sidebar-owned menu that cannot render narrowly must close"
    );
    assert!(app.chat_drawer.is_none());
}

#[test]
fn the_two_step_composer_keeps_the_draft_between_the_steps() {
    let mut app = app_with(&["Jules Lee"]);

    // Step one: a recipient is typed and confirmed.
    app.begin_new_conversation();
    for character in "friend@example.invalid".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    // The composer must stay open: the prompt now asks for a message, so the next
    // keystrokes have to reach the body. It used to close here, which silently
    // dropped the body until the user pressed Enter a second time.
    assert!(
        app.composing,
        "confirming the recipient must leave the composer open for the body"
    );

    // Step two: starting a message for that recipient must not lose the address.
    for character in "see you soon".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.draft, "see you soon");

    match app.handle_key(key(KeyCode::Enter)) {
        Action::StartConversation { address, body, .. } => {
            assert_eq!(body, "see you soon");
            assert!(
                address.contains("friend@example.invalid"),
                "the body must be sent to the address recorded in step one, got {address}"
            );
        }
        other => panic!("expected the composer to send, got {other:?}"),
    }
}

#[test]
fn enter_on_an_empty_draft_does_not_send() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, "");
}

#[test]
fn a_message_makes_the_draft_sendable_to_the_selected_thread() {
    let mut app = app_with(&["Jules Lee"]);
    let selected = app.selected.clone().expect("selected");
    app.upsert(
        selected.clone(),
        message(&conversation("Jules Lee", 2), "m1"),
    );

    app.composing = true;
    for character in "hello".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    match app.handle_key(key(KeyCode::Enter)) {
        Action::SendMessage {
            conversation, body, ..
        } => {
            assert_eq!(conversation, selected);
            assert_eq!(body, "hello");
        }
        other => panic!("expected a send action, got {other:?}"),
    }
    assert_eq!(
        app.draft, "",
        "a sent body must not be left in the composer"
    );
}

#[test]
fn an_unknown_conversation_event_asks_for_a_refresh() {
    let mut app = app_with(&["Jules Lee"]);
    let unknown = conversation("Brand New", 2);
    app.apply_event(EventKind::Message {
        conversation_id: unknown.id.clone(),
        message: message(&unknown, "m9"),
        push_sequence: None,
    });
    assert!(
        app.resync_conversations,
        "a message for a thread we have never listed must trigger a refresh"
    );
}

#[test]
fn page_keys_move_the_reading_position_and_clamp_at_the_newest() {
    let mut app = app_with(&["Jules Lee"]);
    let selected = app.selected.clone().expect("selected");

    assert!(
        !app.is_scrolled_back(),
        "a thread opens pinned to the newest"
    );
    app.handle_key(key(KeyCode::PageDown));
    assert_eq!(app.scroll, 0, "scrolling forward from the newest stays put");

    app.handle_key(key(KeyCode::PageUp));
    app.handle_key(key(KeyCode::PageUp));
    let two_pages = app.scroll;
    assert!(two_pages >= 20, "each page moves ten rows, got {two_pages}");

    app.handle_key(key(KeyCode::PageDown));
    assert_eq!(
        app.scroll,
        two_pages - 10,
        "a page forward moves one page back"
    );

    app.handle_key(ctrl(KeyCode::Home));
    assert!(app.scroll > 0, "Ctrl+Home requests the oldest content");

    app.handle_key(ctrl(KeyCode::End));
    assert!(
        !app.is_scrolled_back(),
        "Ctrl+End returns to the newest message"
    );

    // End returns to newest without moving the draft cursor; Ctrl+E moves it.
    app.draft = "a draft".into();
    app.draft_cursor = 3;
    app.handle_key(key(KeyCode::PageUp));
    app.handle_key(key(KeyCode::End));
    assert!(!app.is_scrolled_back());
    assert_eq!(app.draft_cursor, 3, "End preserves draft cursor");
    app.handle_key(ctrl(KeyCode::Char('e')));
    assert_eq!(app.draft_cursor, "a draft".chars().count());

    // A new message never cancels deliberate reading back.
    app.handle_key(key(KeyCode::PageUp));
    assert!(app.is_scrolled_back());
    app.apply_event(EventKind::Message {
        conversation_id: selected.clone(),
        message: message(&conversation("Jules Lee", 2), "m1"),
        push_sequence: None,
    });
    assert!(
        app.is_scrolled_back(),
        "a new message must not pull the reader away from older content"
    );
}

#[test]
fn scrolling_does_not_disturb_the_composer_or_the_filter() {
    let mut app = app_with(&["Jules Lee"]);

    // While the filter owns the keyboard, page keys are not scroll keys.
    app.begin_filter();
    app.handle_key(key(KeyCode::PageUp));
    assert_eq!(app.scroll, 0);

    // While composing, page keys scroll the transcript and leave the draft alone:
    // scroll-back is the default gesture in a chat.
    app.cancel_filter();
    app.draft = "half written".into();
    app.handle_key(key(KeyCode::PageUp));
    assert_eq!(
        app.scroll, 10,
        "PageUp scrolls the transcript while composing"
    );
    assert_eq!(
        app.draft, "half written",
        "scrolling must not touch the draft"
    );
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.scroll, 9, "Down scrolls the transcript by one row");
}

#[test]
fn the_recipient_step_keeps_the_composer_open_all_the_way_to_sending() {
    // `/new` starts a chat. Then recipient, Enter, body, Enter must send to the
    // typed address, while prefix+n remains available for chat navigation.
    let mut app = app_with(&["Jules Lee"]);
    for character in "/new".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert!(app.new_conversation);
    for character in "newfriend@example.invalid".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    for character in "hello there".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    match app.handle_key(key(KeyCode::Enter)) {
        Action::StartConversation { address, body, .. } => {
            assert_eq!(address, "newfriend@example.invalid");
            assert_eq!(body, "hello there");
        }
        other => panic!("the two-step flow must send, got {other:?}"),
    }
}

#[test]
fn opening_another_thread_returns_to_its_newest_message() {
    let mut app = app_with(&["Jules Lee", "Maya Chen"]);
    let first = app.selected.clone().expect("selected");

    app.handle_key(key(KeyCode::PageUp));
    app.handle_key(key(KeyCode::PageUp));
    assert!(app.is_scrolled_back());

    // Moving to another thread must not inherit the previous reading position.
    // Chats move with the leader chord; Down scrolls the transcript now.
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('l')));
    assert_ne!(app.selected, Some(first));
    assert!(
        !app.is_scrolled_back(),
        "a different thread must open at its newest message"
    );

    // Ctrl+Home followed by a thread change is the worst case: it leaves the
    // largest offset behind if the position is not reset.
    app.handle_key(ctrl(KeyCode::Home));
    assert!(app.is_scrolled_back());
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('h')));
    assert!(
        !app.is_scrolled_back(),
        "Ctrl+Home in one thread must not carry over to the next"
    );
}

#[test]
fn composer_undo_removes_a_typed_run_and_a_deletion_is_separate() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "hello".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.draft, "hello");

    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.draft, "hell");
    assert_eq!(app.undo_depths(), (2, 0));

    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "hello", "undo restores the deleted character");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "", "undo removes the whole typed run");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "", "undo is a no-op at the oldest snapshot");
}

#[test]
fn composer_undo_and_redo_restore_cursor_and_literal_paste_provenance() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "ab".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Left));
    app.handle_key(key(KeyCode::Char('X')));
    assert_eq!(app.draft, "aXb");
    assert_eq!(app.draft_cursor, 2);

    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "ab");
    assert_eq!(app.draft_cursor, 1, "undo restores the insertion cursor");
    app.handle_key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(app.draft, "aXb");
    assert_eq!(app.draft_cursor, 2, "redo reapplies the insertion cursor");

    let mut pasted = app_with(&["Jules Lee"]);
    pasted.paste(" /literal body");
    assert_eq!(pasted.draft, " /literal body");
    assert!(pasted.pasted_body);
    let cursor = pasted.draft_cursor;
    pasted.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(pasted.draft, "");
    assert!(!pasted.pasted_body);
    pasted.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(pasted.draft, " /literal body");
    assert!(pasted.pasted_body, "redo restores literal-paste provenance");
    assert_eq!(pasted.draft_cursor, cursor);
}

#[test]
fn cancelling_composition_cannot_be_undone_back_to_life() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "hello".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "", "the typed run undoes");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.draft, "");

    app.handle_key(ctrl(KeyCode::Char('z')));
    app.handle_key(ctrl(KeyCode::Char('y')));

    assert_eq!(
        app.draft, "",
        "an explicit cancellation is final; neither undo nor redo resurrects it"
    );
    assert_eq!(app.undo_depths(), (0, 0));
}

#[test]
fn undo_works_while_the_command_menu_is_open_and_an_empty_redo_clears_literal_mode() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "/he".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert!(app.command_menu);
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "", "undo is not swallowed by the command menu");

    let mut pasted = app_with(&["Jules Lee"]);
    pasted.paste("/new");
    pasted.handle_key(ctrl(KeyCode::Char('c')));
    pasted.handle_key(ctrl(KeyCode::Char('z')));
    pasted.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(pasted.draft, "");
    assert!(
        !pasted.pasted_body,
        "an empty restored draft leaves literal-paste mode"
    );
}

#[test]
fn a_rejected_deletion_neither_adds_a_step_nor_drops_redo() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "ab".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "");
    let before = app.undo_depths();

    app.handle_key(key(KeyCode::Backspace));

    assert_eq!(
        app.undo_depths(),
        (before.0, before.1),
        "a deletion at the start of the draft changes nothing"
    );
    app.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(app.draft, "ab", "the redo stack survived the no-op");
}

#[test]
fn typing_runs_split_when_the_caret_moves_or_input_is_cleared() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "ab".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Left));
    app.handle_key(key(KeyCode::Char('X')));
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "ab", "the second run undoes on its own");

    let mut cleared = app_with(&["Jules Lee"]);
    for character in "hello".chars() {
        cleared.handle_key(key(KeyCode::Char(character)));
    }
    cleared.handle_key(ctrl(KeyCode::Char('c')));
    assert_eq!(cleared.draft, "");
    cleared.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(
        cleared.draft, "hello",
        "clearing the input is itself an undoable step"
    );
}

#[test]
fn a_pasted_address_is_an_undoable_step() {
    let mut app = app_with(&["Jules Lee"]);
    app.begin_new_conversation();
    app.paste("friend@example.test");

    assert_eq!(app.draft, "friend@example.test");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "", "a pasted recipient address can be undone");
}

#[test]
fn composer_redo_is_dropped_after_a_new_edit_and_history_is_bounded() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "one".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "");
    for character in "two".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(ctrl(KeyCode::Char('y')));
    assert_eq!(app.draft, "two", "a new edit discards the redo stack");
    assert_eq!(app.undo_depths(), (1, 0));

    let mut bounded = app_with(&["Jules Lee"]);
    for _ in 0..60 {
        bounded.paste("x");
        bounded.handle_key(key(KeyCode::Left));
    }
    assert_eq!(bounded.undo_depths(), (50, 0));
    for _ in 0..50 {
        bounded.handle_key(ctrl(KeyCode::Char('z')));
    }
    assert_eq!(bounded.draft, "x".repeat(10));
    assert_eq!(bounded.undo_depths(), (0, 50));
}

#[test]
fn switching_recipient_clears_composer_history_so_undo_cannot_follow() {
    let mut app = app_with(&["Weekend crew", "Jules Lee"]);
    let first = app.selected.clone().expect("selected chat");
    for character in "secret".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.draft, "secret");

    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('n')));
    assert_ne!(app.selected, Some(first.clone()));
    assert_eq!(app.draft, "");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(
        app.draft, "",
        "undo must not resurrect the previous chat's text"
    );

    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(app.selected, Some(first));
    assert_eq!(app.draft, "secret");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(
        app.draft, "secret",
        "returning to a chat clears its old history"
    );
}

#[test]
fn composer_undo_is_ignored_while_search_or_help_owns_input() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "hello".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }

    app.help_open = true;
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "hello", "help owns the keyboard");
    assert_eq!(app.undo_depths(), (1, 0));

    app.help_open = false;
    app.search = Some(Default::default());
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(app.draft, "hello", "search owns the keyboard");
    assert_eq!(app.undo_depths(), (1, 0));
}

#[test]
fn a_successful_send_clears_composer_history() {
    let mut app = app_with(&["Jules Lee"]);
    for character in "hello".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.undo_depths(), (1, 0));
    let (attempt, target) = submit_pending(&mut app);
    app.finish_send(attempt, target, Ok(()));
    assert_eq!(app.undo_depths(), (0, 0));
    assert_eq!(app.draft, "");
    app.handle_key(ctrl(KeyCode::Char('z')));
    assert_eq!(
        app.draft, "",
        "sent text cannot be undone back into a draft"
    );
}

#[test]
fn unread_command_and_leader_chord_anchor_the_first_unread_or_report_none() {
    let chat = conversation("Weekend crew", 2);
    let rows = vec![
        message(&chat, "m1"),
        message(&chat, "m2"),
        message(&chat, "m3"),
    ];
    let mut app = App::default();
    app.set_conversations(vec![chat.clone()]);
    app.select(Some(chat.id.clone()));
    app.messages.insert(chat.id.clone(), rows.clone());
    app.read_positions
        .insert(chat.id.clone(), (rows[0].created_at, rows[0].id().clone()));

    for character in "/unread".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.reading_anchor
            .as_ref()
            .map(|anchor| anchor.message.clone()),
        Some(rows[1].id().clone()),
        "the slash command anchors on the oldest unread message"
    );
    assert!(app.scroll > 0);

    app.reading_anchor = None;
    app.scroll = 0;
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('u')));
    assert_eq!(
        app.reading_anchor
            .as_ref()
            .map(|anchor| anchor.message.clone()),
        Some(rows[1].id().clone()),
        "Ctrl+B u reaches the same first unread message"
    );

    app.mark_read(chat.id.clone(), rows[2].clone());
    app.reading_anchor = None;
    app.scroll = 0;
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('u')));
    assert!(app.reading_anchor.is_none());
    assert_eq!(app.notice.as_deref(), Some("No unread messages."));

    app.notice = None;
    for character in "/unread".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.notice.as_deref(), Some("No unread messages."));
}

#[test]
fn following_the_newest_row_marks_read_but_reading_back_does_not() {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
    let mut paint = |app: &mut App| {
        let mut view = None;
        terminal
            .draw(|frame| view = bloom_tui::ui::render(frame, app).transcript_view)
            .unwrap();
        app.record_transcript(view);
    };
    let chat = conversation("Weekend crew", 2);
    let mut first = message(&chat, "m1");
    first.parts = vec![MessagePart::Text("a long visible history\n".repeat(50))];
    let second = message(&chat, "m2");
    let mut app = App::default();
    app.set_conversations(vec![chat.clone()]);
    app.select(Some(chat.id.clone()));

    app.apply_snapshot(Snapshot {
        token: None,
        revision: 1,
        conversations: vec![chat.clone()],
        visible: Some(chat.id.clone()),
        history: vec![first.clone()],
    });
    assert!(
        app.read_positions.is_empty(),
        "receiving a snapshot is not reading it"
    );
    paint(&mut app);
    assert_eq!(
        app.read_positions.get(&chat.id),
        Some(&(first.created_at, first.id().clone())),
        "a frame following the newest row records it as read"
    );

    app.scroll = 3;
    app.apply_snapshot(Snapshot {
        token: None,
        revision: 2,
        conversations: vec![chat.clone()],
        visible: Some(chat.id.clone()),
        history: vec![first.clone(), second.clone()],
    });
    paint(&mut app);
    assert!(app.scroll > 0);
    assert_eq!(
        app.read_positions.get(&chat.id),
        Some(&(first.created_at, first.id().clone())),
        "reading back must not advance the stored position"
    );

    app.scroll = 0;
    app.help_open = true;
    app.apply_snapshot(Snapshot {
        token: None,
        revision: 3,
        conversations: vec![chat.clone()],
        visible: Some(chat.id.clone()),
        history: vec![first.clone(), second.clone()],
    });
    paint(&mut app);
    assert_eq!(
        app.read_positions.get(&chat.id),
        Some(&(first.created_at, first.id().clone())),
        "an overlay owning input must not mark the thread read"
    );
}

fn participant_picker_app() -> (App, Conversation, ParticipantFilter) {
    let mut alice = conversation("Alice", 2);
    alice.participants[1].display_name = Some("Alice".into());
    let mut bob = conversation("Bob", 2);
    bob.participants[1].display_name = Some("Bob".into());
    let filter = ParticipantFilter {
        transport: alice.id.transport().clone(),
        handle: alice.participants[1].handle.clone(),
    };
    let mut app = App::default();
    app.set_conversations(vec![alice, bob.clone()]);
    app.select(Some(bob.id.clone()));
    app.draft = "/new\nparked 👩‍💻 draft".into();
    app.draft_cursor = 4;
    app.pasted_body = true;
    app.handle_key(ctrl(KeyCode::Char('f')));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    (app, bob, filter)
}

#[test]
fn participant_candidates_keep_exact_transport_and_kind_despite_duplicate_labels() {
    let mut first = conversation("First", 2);
    first.participants[1].handle = Handle::new(HandleKind::Email, "same-value").unwrap();
    first.participants[1].display_name = Some("Duplicate".into());
    let mut other = first.clone();
    other.id = ConversationId::new(TransportId::new("other").unwrap(), "first").unwrap();
    let mut phone = first.clone();
    phone.id = ConversationId::new(transport(), "phone").unwrap();
    phone.participants[1].handle = Handle::new(HandleKind::Tel, "same-value").unwrap();
    let mut shared = first.clone();
    shared.id = ConversationId::new(transport(), "shared-group").unwrap();
    shared.kind = ConversationKind::Group;
    let expected: Vec<_> = [&first, &other, &phone]
        .into_iter()
        .map(|chat| ParticipantFilter {
            transport: chat.id.transport().clone(),
            handle: chat.participants[1].handle.clone(),
        })
        .collect();
    let mut app = App::default();
    app.set_conversations(vec![first, other, phone, shared]);
    app.open_search();
    app.open_search_participant_picker();
    let candidates = app.search_participant_candidates();
    assert_eq!(
        candidates.len(),
        4,
        "All chats plus three exact identities, without self or group duplicates"
    );
    assert!(candidates[0].is_all_chats());
    for filter in expected {
        assert!(candidates
            .iter()
            .any(|candidate| candidate.filter() == Some(&filter)));
    }
    assert!(candidates[1..]
        .iter()
        .all(|candidate| candidate.label() == "Duplicate"));
}

#[test]
fn participant_scope_is_captured_in_actions_and_invalidates_old_replies() {
    let (mut app, bob, filter) = participant_picker_app();
    let Action::Search {
        request: old,
        participant: None,
        ..
    } = app.handle_key(key(KeyCode::Enter))
    else {
        panic!("first search is unfiltered");
    };
    app.handle_key(ctrl(KeyCode::Char('p')));
    app.paste(" Alice ");
    assert_eq!(app.search_query(), Some("needle"));
    assert_eq!(app.search_participant_picker_query(), Some(" Alice "));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.selected_search_participant_candidate()
            .unwrap()
            .filter(),
        Some(&filter)
    );
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.selected,
        Some(bob.id.clone()),
        "scope selection is not recipient navigation"
    );
    assert_eq!(app.search_participant_scope(), Some(&filter));
    let Action::Search {
        request: current,
        query,
        participant,
    } = app.handle_key(key(KeyCode::Enter))
    else {
        panic!("a scoped search must be captured");
    };
    assert_eq!(query, "needle");
    assert_eq!(participant, Some(filter.clone()));
    assert_ne!(old, current);
    app.fail_search(old, "stale scope failure".into());
    app.finish_search(
        old,
        vec![MessageSearchHit {
            conversation_id: bob.id.clone(),
            message_id: MessageId::new(transport(), "wrong-scope").unwrap(),
            created_at: 1,
            snippet: "needle from the old scope".into(),
        }],
    );
    let state = app.search.as_ref().unwrap();
    assert!(state.loading);
    assert!(state.hits.is_empty());
    assert!(state.error.is_none());
    assert_eq!(state.request, Some(current));
    app.finish_search(
        current,
        vec![MessageSearchHit {
            conversation_id: app
                .conversations
                .iter()
                .find(|chat| chat.id != bob.id)
                .unwrap()
                .id
                .clone(),
            message_id: MessageId::new(transport(), "right-scope").unwrap(),
            created_at: 2,
            snippet: "needle from the selected scope".into(),
        }],
    );
    assert!(app.search_results_visible());
    app.handle_key(ctrl(KeyCode::Char('p')));
    app.handle_key(key(KeyCode::Home));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.search_participant_scope(), None);
    assert_eq!(app.search_query(), Some("needle"));
    assert!(!app.search_results_visible());
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Action::Search {
            participant: None,
            ..
        }
    ));
}

#[test]
fn cancelling_inner_picker_restores_search_and_outer_cancel_restores_its_draft() {
    let (mut app, bob, filter) = participant_picker_app();
    assert!(app.commit_search_participant(Some(filter.clone())));
    app.handle_key(ctrl(KeyCode::Char('p')));
    app.paste("different\nquery");
    assert_eq!(
        app.search_participant_picker_query(),
        Some("different query")
    );
    assert_eq!(app.search_query(), Some("needle"));
    assert!(!app.search_results_visible());
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.search_participant_picker_open());
    assert!(app.search.is_some());
    assert_eq!(app.search_participant_scope(), Some(&filter));
    app.handle_key(ctrl(KeyCode::Char('p')));
    app.handle_key(ctrl(KeyCode::Char('p')));
    assert!(!app.search_participant_picker_open());
    assert_eq!(app.search_query(), Some("needle"));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.search.is_none());
    assert_eq!(app.selected, Some(bob.id));
    assert_eq!(app.draft, "/new\nparked 👩‍💻 draft");
    assert_eq!(app.draft_cursor, 4);
    assert!(app.pasted_body);
}

#[test]
fn editing_picker_query_cannot_commit_a_hidden_old_candidate() {
    let (mut app, bob, _) = participant_picker_app();
    let old = ParticipantFilter {
        transport: bob.id.transport().clone(),
        handle: bob.participants[1].handle.clone(),
    };
    assert!(app.commit_search_participant(Some(old.clone())));
    app.open_search_participant_picker();
    assert_eq!(
        app.selected_search_participant_candidate()
            .unwrap()
            .filter(),
        Some(&old)
    );
    app.paste("Alice");
    assert!(app
        .selected_search_participant_candidate()
        .unwrap()
        .is_all_chats());
    assert_eq!(
        app.search_participant_scope(),
        Some(&old),
        "typing has not committed a new scope"
    );
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.search_participant_scope(),
        None,
        "Enter commits the visible All chats row, not hidden Bob"
    );
    assert_eq!(app.search_query(), Some("needle"));
}

#[test]
fn metadata_changes_cannot_commit_a_picker_candidate_that_became_hidden() {
    let (mut app, _, filter) = participant_picker_app();
    let mut chats = app.conversations.clone();
    let alice = chats
        .iter_mut()
        .find(|chat| chat.participants[1].handle == filter.handle)
        .unwrap();
    alice.participants[1].display_name = Some("School friend".into());
    app.set_conversations(chats);
    app.open_search_participant_picker();
    app.search_paste("School");
    app.step_search_participant(1);
    assert_eq!(
        app.selected_search_participant_candidate()
            .unwrap()
            .filter(),
        Some(&filter)
    );
    let mut chats = app.conversations.clone();
    let alice = chats
        .iter_mut()
        .find(|chat| chat.participants[1].handle == filter.handle)
        .unwrap();
    alice.participants[1].display_name = Some("Renamed".into());
    app.set_conversations(chats);
    assert!(app.selected_search_participant_candidate().is_none());
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.search_participant_scope(), None);
    assert!(app.search_participant_picker_open());
}

#[test]
fn committing_a_removed_participant_never_falls_back_to_all_chats() {
    let (mut app, bob, filter) = participant_picker_app();
    app.open_search_participant_picker();
    app.set_conversations(vec![bob.clone()]);
    assert!(!app.commit_search_participant(Some(filter)));
    assert!(app.search_participant_picker_open());
    assert_eq!(app.search_query(), Some("needle"));
    assert_eq!(app.selected, Some(bob.id));
}

#[test]
fn unfiltered_search_actions_keep_query_text_literal() {
    let (mut app, _, _) = participant_picker_app();
    app.search_clear();
    app.search_paste("from:alice");
    assert!(matches!(app.handle_key(key(KeyCode::Enter)),
        Action::Search { query, participant: None, .. } if query == "from:alice"));
}

#[test]
fn persisted_read_position_survives_apply_read_positions_round_trip() {
    let chat = conversation("Weekend crew", 2);
    let rows = vec![
        message(&chat, "m1"),
        message(&chat, "m2"),
        message(&chat, "m3"),
    ];
    let mut app = App::default();
    app.set_conversations(vec![chat.clone()]);
    app.messages.insert(chat.id.clone(), rows.clone());
    app.mark_read(chat.id.clone(), rows[0].clone());
    let stored = app.read_positions_for_config();

    let mut reloaded = App::default();
    reloaded.set_conversations(vec![chat.clone()]);
    reloaded.messages.insert(chat.id.clone(), rows.clone());
    reloaded.apply_read_positions(&stored);

    assert_eq!(
        reloaded.unread_count(&chat.id),
        Some(UnreadCount {
            shown: 2,
            capped: false,
        })
    );
    assert_eq!(
        reloaded.first_unread(&chat.id).map(Message::id),
        Some(rows[1].id())
    );
}
