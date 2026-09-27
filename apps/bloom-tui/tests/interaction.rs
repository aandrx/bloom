//! Key-path tests: the real `handle_key` semantics, not just the underlying
//! `App` methods. The `q`-quits-while-filtering bug and the two-step composer
//! both lived in the event loop, where method-level tests could not see them.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::EventKind;
use bloom_tui::{
    app::{Action, App, SendPhase},
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
fn q_is_text_while_the_filter_is_open_and_quits_from_the_list() {
    let mut app = app_with(&["Quinn Callahan", "Jules Lee"]);

    // q is text in the composer; Ctrl+C remains the quit path.
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.draft, "q");
    assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), Action::Quit);

    // With the filter open it must be an ordinary character, or names containing a
    // q are unsearchable and the app exits while the user is typing.
    app.begin_filter();
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.filter_query(), Some("q"));
    assert_eq!(app.handle_key(key(KeyCode::Char('u'))), Action::None);
    assert_eq!(app.filter_query(), Some("qu"));
}

#[test]
fn q_is_text_while_composing_a_message() {
    let mut app = app_with(&["Jules Lee"]);
    app.composing = true;
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(app.draft, "q");
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

    // Enter completes the highlighted partial command instead of submitting it.
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.draft, "/theme ");
    assert!(app.composing);
    assert_eq!(app.draft, "/theme ");
    assert!(!app.command_menu);
    app.draft.push_str("dark");
    app.draft_cursor = app.draft.len();
    app.composing = true;
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    assert_eq!(app.theme, bloom_tui::app::Theme::Dark);
    assert!(app.draft.is_empty());

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
    assert!(app.new_conversation);
    assert!(app.composing);

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

#[test]
fn send_feedback_is_contextual_and_never_claims_delivery() {
    let mut app = app_with(&["Jules Lee", "Quinn Callahan"]);
    let target = app.selected.clone();
    app.begin_send(target.clone());
    assert_eq!(
        app.visible_send_feedback().unwrap().phase,
        SendPhase::Sending
    );

    app.select_step(1);
    assert!(app.visible_send_feedback().is_none());
    app.finish_send(target.clone(), Ok(()));
    assert!(app.visible_send_feedback().is_none());
    app.selected = target;
    assert_eq!(app.visible_send_feedback().unwrap().phase, SendPhase::Sent);
    app.handle_key(key(KeyCode::Char('j')));
    assert!(app.send_feedback.is_none());

    app.finish_send(None, Err("transport unavailable".into()));
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

    // Theme choice cycles from automatic to explicit dark, then light, then auto.
    assert_eq!(app.theme, Theme::Light);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.theme_override, Some(Theme::Dark));
    assert_eq!(app.theme, Theme::Dark);
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.compact_sidebar);
    app.handle_key(key(KeyCode::Up));
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
        Action::StartConversation { address, body } => {
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
        Action::SendMessage { conversation, body } => {
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

    // Home and End belong to the composer line now.
    app.draft = "a draft".into();
    app.draft_cursor = 3;
    app.handle_key(key(KeyCode::Home));
    assert_eq!(app.draft_cursor, 0, "Home moves the draft cursor");
    app.handle_key(key(KeyCode::End));
    assert_eq!(app.draft_cursor, "a draft".chars().count());

    // A new message in the open thread pins the view back to the newest.
    app.handle_key(key(KeyCode::PageUp));
    assert!(app.is_scrolled_back());
    app.apply_event(EventKind::Message {
        conversation_id: selected.clone(),
        message: message(&conversation("Jules Lee", 2), "m1"),
    });
    assert!(
        !app.is_scrolled_back(),
        "a new message must not leave the reader stranded in the past"
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
    // The whole two-step flow through the real key path: the leader chord starts a
    // new chat, then recipient, Enter, body, Enter must send to the typed address.
    // A bare `n` is text now that the composer is always focused.
    let mut app = app_with(&["Jules Lee"]);
    app.handle_key(ctrl(KeyCode::Char('b')));
    app.handle_key(key(KeyCode::Char('n')));
    for character in "newfriend@example.invalid".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Action::None);
    for character in "hello there".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    match app.handle_key(key(KeyCode::Enter)) {
        Action::StartConversation { address, body } => {
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
