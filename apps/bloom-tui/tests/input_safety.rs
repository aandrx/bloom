use bloom_domain::{ConversationId, TransportId};
use bloom_tui::app::{Action, App, Focus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn chat() -> ConversationId {
    ConversationId::new(TransportId::new("test").unwrap(), "a").unwrap()
}
fn enter(app: &mut App) -> Action {
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

#[test]
fn paste_preserves_body_text_and_sanitizes_controls() {
    let mut app = App {
        selected: Some(chat()),
        ..Default::default()
    };
    app.paste(" /new\r\n\nend\x1b\t");
    assert_eq!(app.draft, " /new\n\nend    ");
    assert!(app.pasted_body);
    assert!(app.pending_send_attempt.is_none());
    assert!(
        matches!(enter(&mut app), Action::SendMessage { body, .. } if body == " /new\n\nend    ")
    );
}

#[test]
fn pasted_slash_never_runs_command_and_typing_after_clear_still_does() {
    let mut app = App {
        selected: Some(chat()),
        ..Default::default()
    };
    app.paste("/new");
    assert!(!app.command_menu);
    assert!(matches!(enter(&mut app), Action::SendMessage { body, .. } if body == "/new"));
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(app.command_menu);
}

#[test]
fn recipient_paste_separates_lines_instead_of_concatenating_addresses() {
    let mut app = App {
        new_conversation: true,
        ..Default::default()
    };
    app.paste("first@example.invalid\r\nsecond@example.invalid");
    assert_eq!(app.draft, "first@example.invalid second@example.invalid");
    assert!(!app.pasted_body);
}

#[test]
fn newline_keys_are_edits_not_submission_or_popup_activation() {
    for key in [
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ] {
        let mut app = App {
            selected: Some(chat()),
            draft: "first".into(),
            draft_cursor: 5,
            ..Default::default()
        };
        assert_eq!(app.handle_key(key), Action::None);
        assert_eq!(app.draft, "first\n");
        assert!(app.pending_send_attempt.is_none());
        app.paste("second");
        assert!(matches!(enter(&mut app), Action::SendMessage {body,..} if body=="first\nsecond"));
        let mut recipient = App {
            new_conversation: true,
            draft: "alice@example.invalid".into(),
            ..Default::default()
        };
        assert_eq!(recipient.handle_key(key), Action::None);
        assert!(recipient.pending_address.is_none());
    }
}

#[test]
fn paste_at_combining_grapheme_boundary_keeps_valid_cursor() {
    let mut app = App {
        draft: "ab".into(),
        draft_cursor: 1,
        ..Default::default()
    };
    app.paste("\u{301}");
    assert_eq!(app.draft, "a\u{301}b");
    assert_eq!(app.draft_cursor, 1);
    app.paste("界");
    assert_eq!(app.draft, "a\u{301}界b");
}

#[test]
fn overlays_ignore_paste_but_search_fields_accept_it() {
    let mut app = App {
        settings_open: true,
        draft: "safe".into(),
        ..Default::default()
    };
    app.paste("/new\nanything");
    assert_eq!(app.draft, "safe");
    app.settings_open = false;
    app.contacts_open = true;
    app.paste("alice\nsmith");
    assert_eq!(app.contacts_query, "alice smith");
    assert_eq!(app.draft, "safe");
    app.contacts_open = false;
    app.filter = Some(String::new());
    app.paste("person");
    assert_eq!(app.filter.as_deref(), Some("person"));
}

#[test]
fn pending_send_does_not_consume_a_new_pasted_draft() {
    let mut app = App {
        selected: Some(chat()),
        ..Default::default()
    };
    app.begin_send(1, Some(chat()));
    app.paste("  newer\nmessage  ");
    assert_eq!(enter(&mut app), Action::None);
    assert_eq!(app.draft, "  newer\nmessage  ");
}

#[test]
fn new_chat_body_preserves_whitespace_and_literal_slash() {
    let mut app = App {
        new_conversation: true,
        pending_address: Some("alice@example.invalid".into()),
        focus: Focus::Composer,
        ..Default::default()
    };
    app.paste("  /new\nbody  ");
    assert!(matches!(enter(&mut app),Action::StartConversation{body,..} if body=="  /new\nbody  "));
}
