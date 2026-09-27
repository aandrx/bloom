//! Key-path tests: the real `handle_key` semantics, not just the underlying
//! `App` methods. The `q`-quits-while-filtering bug and the two-step composer
//! both lived in the event loop, where method-level tests could not see them.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::EventKind;
use bloom_tui::{
    app::{Action, App},
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

    // In the plain list state `q` still quits.
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);

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
    assert!(
        !app.composing,
        "recording the recipient ends the first step"
    );

    // Step two: starting a message for that recipient must not lose the address.
    app.composing = true;
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
