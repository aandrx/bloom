use bloom_domain::{ConversationId, TransportId};
use bloom_ipc::{ReadinessError, ReadinessOutcome, ReadinessPhase, ReadinessSnapshot};
use bloom_tui::app::{Action, App, Focus};
use bloom_tui::startup::StartupState;
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

#[test]
fn startup_loading_consumes_all_input_and_preserves_the_draft() {
    let mut app = App {
        startup: StartupState::loading(),
        selected: Some(chat()),
        draft: "hello".into(),
        draft_cursor: 5,
        ..Default::default()
    };
    assert_eq!(enter(&mut app), Action::None);
    assert_eq!(app.draft, "hello");
    assert_eq!(app.draft_cursor, 5);
    assert!(app.pending_send_attempt.is_none());
    assert!(app.pending_attachment_send.is_none());

    app.paste(" world");
    assert_eq!(app.draft, "hello");
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(!app.new_conversation);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Action::Quit
    );
}

#[test]
fn startup_preserves_new_chat_recipient_body_and_cursor() {
    let mut app = App {
        startup: StartupState::loading(),
        new_conversation: true,
        pending_address: Some("alice@example.invalid".into()),
        draft: "body text".into(),
        draft_cursor: 4,
        pasted_body: true,
        ..Default::default()
    };
    assert_eq!(enter(&mut app), Action::None);
    app.paste("more");
    assert_eq!(app.draft, "body text");
    assert_eq!(app.draft_cursor, 4);
    assert!(app.pasted_body);
    assert_eq!(
        app.pending_address.as_deref(),
        Some("alice@example.invalid")
    );
    assert!(app.new_conversation);
}

#[test]
fn startup_failure_offers_retry_and_quit_without_skipping() {
    let mut app = App {
        startup: StartupState::loading(),
        selected: Some(chat()),
        draft: "keep me".into(),
        ..Default::default()
    };
    app.observe_startup_connection(bloom_tui::daemon::ConnectionState::Connected);
    app.observe_startup_readiness(ReadinessSnapshot {
        generation: 1,
        operation: 1,
        revision: 1,
        phase: ReadinessPhase::Failed,
        outcome: Some(ReadinessOutcome::Failed),
        observed_records: 0,
        can_send: false,
        engine_ready: true,
        bounded_error: Some(ReadinessError::Timeout),
    });
    assert_eq!(enter(&mut app), Action::RetryStartup);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        Action::RetryStartup
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Action::Quit
    );
    assert_eq!(app.draft, "keep me");
}

#[test]
fn startup_confirmation_requires_all_five_checks_and_is_explicit() {
    let mut app = App {
        startup: StartupState::loading(),
        selected: Some(chat()),
        draft: "keep me".into(),
        ..Default::default()
    };
    app.observe_startup_connection(bloom_tui::daemon::ConnectionState::Connected);
    app.observe_startup_readiness(ReadinessSnapshot {
        generation: 1,
        operation: 1,
        revision: 1,
        phase: ReadinessPhase::Ready,
        outcome: Some(ReadinessOutcome::Complete),
        observed_records: 0,
        can_send: true,
        engine_ready: true,
        bounded_error: None,
    });
    app.observe_startup_snapshot();
    assert_eq!(enter(&mut app), Action::None);
    app.observe_startup_draft_ready();
    assert_eq!(enter(&mut app), Action::OpenBloom);
    assert_eq!(app.draft, "keep me");
    assert!(app.open_bloom());
    assert!(app.startup.opened());
}

#[test]
fn manual_sync_closes_all_three_outbound_intents_without_mutating_them() {
    let mut app = App {
        selected: Some(chat()),
        draft: "hello".into(),
        sync_in_flight: true,
        ..Default::default()
    };
    assert_eq!(enter(&mut app), Action::None);
    assert_eq!(app.draft, "hello");
    assert!(app.pending_send_attempt.is_none());

    app.draft = "/new".into();
    assert_eq!(enter(&mut app), Action::None);
    assert!(!app.new_conversation);
    assert!(app.pending_send_attempt.is_none());

    app.draft = "/attach /tmp/report.txt".into();
    assert_eq!(enter(&mut app), Action::None);
    assert!(app.pending_attachment_send.is_none());
    assert!(app.attachment_send_in_flight.is_none());
}
