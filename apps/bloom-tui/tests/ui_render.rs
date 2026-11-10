//! Headless render check that the layout matches the design mockup.
//!
//! ratatui's `TestBackend` renders into an in-memory buffer, so the structural
//! elements of `design/previews/tui-preview.png` can be asserted without a
//! terminal, a daemon, or the engine.

use bloom_domain::{
    AttachmentRef, Conversation, ConversationId, ConversationKind, DeliveryState, Handle,
    HandleKind, Message, MessageId, MessagePart, Participant, Reaction, TransportId,
};
use bloom_ipc::{
    MessageSearchHit, ParticipantFilter, ReadinessError, ReadinessOutcome, ReadinessPhase,
    ReadinessSnapshot,
};
use bloom_tui::startup::StartupState;
use bloom_tui::{
    app::{
        message_text, Action, App, Focus, SendFeedback, SendPhase, Theme, OLDER_PAGE_LIMIT,
        SEARCH_RESULT_LIMIT,
    },
    daemon::{ConnectionState, HISTORY_LIMIT},
    ui,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    backend::{Backend, TestBackend},
    Terminal,
};

fn buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn transport() -> TransportId {
    TransportId::new("openbubbles").unwrap()
}

fn conversation(title: &str, participants: usize, last: i64) -> Conversation {
    let id = ConversationId::new(transport(), title).unwrap();
    Conversation {
        id,
        remote_id: format!("iMessage;-;{title}"),
        kind: ConversationKind::Direct,
        title: Some(title.to_owned()),
        participants: (0..participants)
            .map(|index| Participant {
                handle: Handle::new(HandleKind::Opaque, format!("person{index}")).unwrap(),
                display_name: None,
                is_me: index == 0,
            })
            .collect(),
        created_at: 1,
        last_message_at: Some(last),
        aliases: Vec::new(),
        preview: None,
    }
}

fn message(conversation: &Conversation, id: &str, at: i64, text: &str, mine: bool) -> Message {
    Message::new(
        MessageId::new(transport(), id).unwrap(),
        conversation.id.clone(),
        None,
        if mine {
            conversation
                .participants
                .iter()
                .find(|p| p.is_me)
                .unwrap()
                .handle
                .clone()
        } else {
            Handle::new(HandleKind::Opaque, "Maya").unwrap()
        },
        vec![MessagePart::Text(text.to_owned())],
        at,
        None,
        if mine {
            DeliveryState::Delivered
        } else {
            DeliveryState::Read
        },
        mine,
        None,
        Vec::new(),
        None,
    )
    .unwrap()
}

fn message_with_parts(
    conversation: &Conversation,
    id: &str,
    at: i64,
    parts: Vec<MessagePart>,
    mine: bool,
) -> Message {
    Message::new(
        MessageId::new(transport(), id).unwrap(),
        conversation.id.clone(),
        None,
        if mine {
            conversation
                .participants
                .iter()
                .find(|p| p.is_me)
                .unwrap()
                .handle
                .clone()
        } else {
            Handle::new(HandleKind::Opaque, "Maya").unwrap()
        },
        parts,
        at,
        None,
        if mine {
            DeliveryState::Delivered
        } else {
            DeliveryState::Read
        },
        mine,
        None,
        Vec::new(),
        None,
    )
    .unwrap()
}

/// A reported attachment name, or an unnamed attachment when `name` is `None`.
fn attachment(name: Option<&str>) -> MessagePart {
    MessagePart::Attachment(
        AttachmentRef::new("attachment-id", None, name.map(str::to_owned), None).unwrap(),
    )
}

/// Draw through the public cache entry point so a reuse regression is observable.
fn cached_frame(
    app: &App,
    width: u16,
    height: u16,
    cache: &mut ui::RenderCache,
) -> (ratatui::buffer::Buffer, ui::HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render_with_cache(frame, app, cache))
        .unwrap();
    (terminal.backend().buffer().clone(), hits)
}

/// Only the transcript rectangle, with row padding removed. This excludes the
/// sidebar preview so a stale or mismatched sidebar cannot mask the transcript.
fn transcript_flat(buffer: &ratatui::buffer::Buffer, hits: &ui::HitMap) -> String {
    let area = hits.transcript;
    (area.y..area.y + area.height)
        .map(|y| {
            (area.x..area.x + area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("")
}

fn rendered(app: &App) -> String {
    let buffer = buffer(app, 100, 24);
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// One buffer row rebuilt from its cells. Wide characters occupy a cell each, so
/// this is a display view of the row rather than a byte view of what was drawn.
fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// The display column where `needle` starts on row `y`, matched cell by cell.
fn column_of(buffer: &ratatui::buffer::Buffer, y: u16, needle: &str) -> Option<u16> {
    let span = needle.chars().count() as u16;
    (0..buffer.area.width).find(|x| {
        let mut candidate = String::new();
        for offset in 0..span {
            let index = x + offset;
            if index >= buffer.area.width {
                return false;
            }
            candidate.push_str(buffer[(index, y)].symbol());
        }
        candidate == needle
    })
}

#[test]
fn layout_matches_the_mockup_structure() {
    let selected = conversation("Weekend crew", 3, 40);
    let other = conversation("Jules Lee", 2, 20);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone(), other]);
    app.upsert(
        selected.id.clone(),
        message(
            &selected,
            "m1",
            1_700_000_000_000,
            "Saturday looks perfect for the coast.",
            false,
        ),
    );
    app.upsert(
        selected.id.clone(),
        message(&selected, "m2", 1_700_000_060_000, "Yes, 8:40 works.", true),
    );
    app.connection = "Connected".into();
    app.connection_state = ConnectionState::Connected;
    app.session = "Active".into();

    let text = rendered(&app);
    let buffer = buffer(&app, 100, 30);

    let symbol = |x, y| buffer[(x, y)].symbol();

    // Derive positions instead of hardcoding them, so a layout change cannot make
    // these assertions quietly meaningless.
    let rule_x = (2..99)
        .find(|x| symbol(*x, 10) == "│")
        .expect("the sidebar divider must be drawn");
    assert!(
        (2..29).filter(|y| symbol(rule_x, *y) == "│").count() >= 20,
        "the divider must span the body"
    );

    // The header rule belongs to the conversation column only.
    let header_rule_row = (1..8)
        .find(|y| (rule_x + 1..99).filter(|x| symbol(*x, *y) == "─").count() > 40)
        .expect("the conversation header must have its own rule");
    assert!(
        (1..rule_x).all(|x| symbol(x, header_rule_row) != "─"),
        "the header rule must not run over the sidebar"
    );

    // The window has no outer frame.
    assert!((0..100).all(|x| symbol(x, 0) != "─"));
    assert!((0..100).all(|x| symbol(x, 29) != "─"));
    assert!((0..30).all(|y| symbol(0, y) != "│"));
    assert!((0..30).all(|y| symbol(99, y) != "│"));

    // The composer has a lower boundary before its separate hint row.
    let all = rendered(&app);
    assert_eq!(all.matches("›  Write a message...").count(), 1);
    let prompt_row = (1..29)
        .find(|y| (0..100).any(|x| symbol(x, *y) == "›"))
        .expect("the composer prompt must be drawn");
    let bottom_rule: String = (0..100).map(|x| symbol(x, prompt_row + 1)).collect();
    assert!(
        !bottom_rule.contains('─'),
        "bare composer has no surrounding box: {bottom_rule:?}"
    );
    let hints_row = (prompt_row + 1..30)
        .find(|y| row_text(&buffer, *y).contains("Enter send"))
        .expect("footer hint row");
    let hints: String = (0..100).map(|x| symbol(x, hints_row)).collect();
    assert!(
        hints.contains("chats"),
        "the key hints must sit directly under the composer, found: {hints:?}"
    );

    for expected in [
        "Chats",                  // sidebar label
        "Weekend crew",           // selected conversation title
        "Weekend crew",           // selected conversation title
        "person1",                // direct-chat recipient subtitle
        "Saturday looks perfect", // transcript body
        "Yes, 8:40 works.",       // our own message
        "delivered",              // delivery marker
        "Write a message",        // composer placeholder
        "Ctrl+J newline",         // reliable multiline shortcut
        "Tab chats",              // sidebar focus shortcut
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }

    // The hint line matches the mockup and no longer carries connection state.
    let hints_row = (0..30)
        .find(|y| row_text(&buffer, *y).contains("Enter send"))
        .expect("the hint line must be drawn");
    let hint_text: String = (0..100).map(|x| symbol(x, hints_row)).collect();
    assert!(
        !hint_text.contains("Connected"),
        "connection state belongs in the header, found: {hint_text:?}"
    );

    // The header status is an outlined chip, so it has side borders on its row.
    let header = row_text(&buffer, 0);
    let controls = &header;
    assert!(controls.contains('⚙') && controls.contains('?'));
    assert!(header.contains("● Active"));

    // Pane content extends to its right edge rather than stopping at a fixed cap.

    // The old boxed layout and debug output must be gone.
    for unwanted in [
        "Conversations",
        "Thread",
        "DeliveryState",
        "Some(",
        "[attachment]",
    ] {
        assert!(
            !text.contains(unwanted),
            "unexpected {unwanted:?} in:\n{text}"
        );
    }
}

#[test]
fn empty_store_renders_a_usable_screen_rather_than_nothing() {
    let app = App {
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        ..Default::default()
    };

    let text = rendered(&app);

    assert!(
        text.contains("Chats"),
        "sidebar still present when empty:\n{text}"
    );
    assert!(
        text.contains("Bloom"),
        "header still present when empty:\n{text}"
    );
    assert!(
        text.contains("Write a message"),
        "composer still present when empty:\n{text}"
    );
    assert!(
        text.contains("/new start"),
        "empty-store action is visible:\n{text}"
    );
    assert!(
        !text.contains("Enter open"),
        "no invalid open hint:\n{text}"
    );
    assert!(
        text.contains("No conversations yet"),
        "empty-state guidance:\n{text}"
    );
    assert!(
        text.contains("start a conversation"),
        "empty-state action:\n{text}"
    );
}

#[test]
fn command_menu_renders_suggestions_and_compact_hints() {
    let mut app = App {
        composing: true,
        draft: "/".into(),
        draft_cursor: 1,
        command_menu: true,
        ..Default::default()
    };

    let desktop = buffer(&app, 100, 30);
    let desktop_text: String = (0..desktop.area.height)
        .map(|y| row_text(&desktop, y))
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "/help",
        "Show command help",
        "/theme",
        "/settings",
        "Open local display settings",
        "/nickname",
        "Name this direct chat locally",
        "/new",
        "Start a new direct chat",
        "/filter",
        "Filter your conversations",
        "/compact",
        "Toggle sidebar density",
        "/unhide",
        "Restore chats hidden with Close",
        "↑↓ select",
        "Enter run",
    ] {
        assert!(
            desktop_text.contains(expected),
            "missing {expected:?}:\n{desktop_text}"
        );
    }

    let compact = buffer(&app, 40, 8);
    let compact_text: String = (0..compact.area.height)
        .map(|y| row_text(&compact, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        compact_text.contains("/help"),
        "compact popup remains visible:\n{compact_text}"
    );
    assert!(
        compact_text.contains("run"),
        "compact menu hint remains visible:\n{compact_text}"
    );

    app.draft = "/not-a-command".into();
    app.draft_cursor = app.draft.chars().count();
    let unmatched = rendered(&app);
    assert!(
        unmatched.contains("No matching commands"),
        "unknown command has visible menu feedback:\n{unmatched}"
    );
}

#[test]
fn session_status_dot_changes_color_with_status_tone() {
    let mut colors = Vec::new();
    for (session, label) in [
        ("Active", "Active"),
        ("RenewalDue", "Renewal due"),
        ("Blocked", "Blocked"),
    ] {
        let app = App {
            connection: "Connected".into(),
            connection_state: ConnectionState::Connected,
            session: session.into(),
            ..Default::default()
        };
        let grid = buffer(&app, 100, 30);
        let x = column_of(&grid, 0, "●").expect("session status dot in header");
        colors.push(grid[(x, 0)].fg);
        assert!(row_text(&grid, 0).contains(label));
    }
    assert_ne!(colors[0], colors[1], "healthy and attention colors differ");
    assert_ne!(colors[1], colors[2], "attention and failure colors differ");
}

#[test]
fn leader_hint_explains_herdr_style_navigation() {
    let app = App {
        leader_pending: true,
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(screen.contains("n next"), "leader keys:\n{screen}");
    assert!(screen.contains("Ctrl+P previous"), "leader keys:\n{screen}");
    assert!(screen.contains("j/k scroll"), "leader keys:\n{screen}");
    assert!(screen.contains("Esc cancel"), "leader keys:\n{screen}");
}

#[test]
fn shortcut_sheet_documents_new_navigation_and_clear_before_quit() {
    let app = App {
        help_open: true,
        ..Default::default()
    };
    let screen = rendered(&app);
    for hint in [
        "/new",
        "/sync",
        "Ctrl+B n/p",
        "Ctrl+P",
        "Ctrl+C",
        "cursor, chats, or scroll",
        "toggle composer / chat list",
    ] {
        assert!(
            screen.contains(hint),
            "shortcut {hint:?} is missing:\n{screen}"
        );
    }
    assert!(
        screen.contains("clear input; quit when empty"),
        "clear-before-quit behavior is explicit:\n{screen}"
    );
}

#[test]
fn sync_feedback_is_visible_in_the_composer_footer() {
    let mut app = App::default();
    app.finish_sync(Ok((bloom_ipc::ConversationSyncStatus::Complete, 42)));
    assert!(rendered(&app).contains("Sync complete: 42 conversations listed."));

    app.finish_sync(Ok((bloom_ipc::ConversationSyncStatus::Limited, 12)));
    assert!(rendered(&app).contains("older chats may be missing."));

    app.finish_sync(Err("Conversation-list sync failed.".into()));
    assert!(rendered(&app).contains("Conversation-list sync failed."));
}

#[test]
fn send_feedback_is_visible_but_delivery_claims_stay_event_driven() {
    let pending = App {
        send_feedback: Some(SendFeedback {
            conversation: None,
            phase: SendPhase::Sending,
        }),
        ..Default::default()
    };
    assert!(rendered(&pending).contains("Sending…"));

    let accepted = App {
        send_feedback: Some(SendFeedback {
            conversation: None,
            phase: SendPhase::Sent,
        }),
        ..Default::default()
    };
    let screen = rendered(&accepted);
    assert!(screen.contains("New-chat message sent."));
    assert!(!screen.contains("delivered"));

    let chat = conversation("Alice", 2, 1);
    let scoped = App {
        selected: Some(chat.id.clone()),
        send_feedback: Some(SendFeedback {
            conversation: Some(chat.id),
            phase: SendPhase::Sent,
        }),
        ..Default::default()
    };
    assert!(rendered(&scoped).contains("Message sent."));
}

#[test]
fn new_message_has_its_own_identity_and_never_shows_the_previous_transcript() {
    for (width, height) in [(100, 30), (66, 18), (44, 12)] {
        let old = conversation("Previous private chat", 2, 1);
        let mut app = App {
            new_conversation: true,
            pending_address: Some("carol@example.invalid".into()),
            ..Default::default()
        };
        app.set_conversations(vec![old.clone()]);
        app.messages.insert(
            old.id.clone(),
            vec![message(
                &old,
                "old",
                1,
                "UNRELATED PRIVATE TRANSCRIPT",
                false,
            )],
        );
        let screen = buffer(&app, width, height);
        let header = row_text(&screen, 0);
        assert!(header.contains("New message"), "{width}: {header}");
        assert!(
            row_text(&screen, 1).contains("carol@example.invalid"),
            "Recipient identity must not be clipped into an ambiguous composer prefix"
        );
        let text = (0..height)
            .map(|y| row_text(&screen, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("UNRELATED PRIVATE TRANSCRIPT"));
    }
}

#[test]
fn empty_history_explains_the_local_store_and_never_claims_a_first_message() {
    let chat = conversation("Existing remote chat", 2, 1);
    let mut app = App {
        connection_state: ConnectionState::Connected,
        ..Default::default()
    };
    app.set_conversations(vec![chat]);
    let screen = rendered(&app);
    assert!(screen.contains("No stored messages"));
    assert!(screen.contains("Older history is not imported"));
    assert!(!screen.contains("first message"));
}

#[test]
fn empty_offline_view_prioritizes_connection_recovery() {
    let app = App {
        connection_state: ConnectionState::Disconnected,
        connection: "Disconnected, retrying".into(),
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(screen.contains("Daemon unavailable"));
    assert!(screen.contains("Reconnecting"));
    assert!(!screen.contains("start a conversation"));
}

#[test]
fn inbound_catch_up_says_updated_without_a_sync_spinner() {
    let selected = conversation("Weekend crew", 3, 40);
    for (width, height) in [(100u16, 30u16), (60, 18)] {
        let mut app = App {
            selected: Some(selected.id.clone()),
            connection: "Connected".into(),
            connection_state: ConnectionState::Connected,
            session: "Active".into(),
            syncing: true,
            ..Default::default()
        };
        app.set_conversations(vec![selected.clone()]);
        let grid = buffer(&app, width, height);
        let header = format!("{}\n{}", row_text(&grid, 0), row_text(&grid, 1));
        assert!(header.contains("Updated"), "{width}: {header:?}");
        assert!(
            !header.contains("Syncing"),
            "persisted inbound events must not claim a remote sync: {header:?}"
        );
        assert!(
            !header
                .chars()
                .any(|c| ('\u{2800}'..'\u{2900}').contains(&c)),
            "Updated must not paint a sync spinner: {header:?}"
        );
    }
}

#[test]
fn manual_sync_keeps_its_own_spinner_and_word() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        session: "Active".into(),
        sync_in_flight: true,
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let grid = buffer(&app, 100, 30);
    let header = row_text(&grid, 0);
    assert!(header.contains("Syncing"), "{header:?}");
    assert!(
        header
            .chars()
            .any(|c| ('\u{2800}'..'\u{2900}').contains(&c)),
        "manual sync keeps a spinner: {header:?}"
    );
}

#[test]
fn long_disconnected_header_keeps_identity_and_status_at_narrow_widths() {
    let selected = conversation("Long recipient name with 👨‍👩‍👧‍👦 Unicode and address", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        connection: "Disconnected, retrying after network failure".into(),
        connection_state: ConnectionState::Disconnected,
        ..Default::default()
    };
    app.set_conversations(vec![selected]);
    for width in [44, 60, 66] {
        let grid = buffer(&app, width, 18);
        let rows = (0..grid.area.height)
            .map(|y| row_text(&grid, y))
            .collect::<Vec<_>>();
        assert!(
            rows[0].contains("Long recipient name"),
            "identity at {width}: {:?}",
            rows[0]
        );
        assert!(
            rows.iter().take(2).any(|row| row.contains("Offline")),
            "failure status at {width}: {rows:?}"
        );
        if width == 44 {
            assert!(
                !rows[0].contains("Offline") && rows[1].contains("Offline"),
                "short header must move failure status to its own row: {rows:?}"
            );
        }
    }
}

#[test]
fn short_narrow_view_keeps_the_start_of_the_latest_message_visible() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    app.upsert(
        selected.id.clone(),
        message(
            &selected,
            "m1",
            1_700_000_000_000,
            "I'll check the tide times tonight and send the link here.",
            false,
        ),
    );
    let grid = buffer(&app, 40, 12);
    let screen = (0..grid.area.height)
        .map(|row| row_text(&grid, row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("I'll check the tide"),
        "latest message is readable:\n{screen}"
    );
}

#[test]
fn sidebar_identity_does_not_disguise_participant_counts_as_unread_badges() {
    let mut app = App::default();
    app.set_conversations(vec![
        conversation("Weekend crew", 3, 40),
        conversation("Jules Lee", 2, 20),
    ]);
    let grid = buffer(&app, 100, 30);
    let sidebar_row = |title: &str| {
        (0..grid.area.height)
            .map(|y| {
                row_text(&grid, y)
                    .split('│')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .find(|sidebar| sidebar.contains(title))
            .expect("sidebar conversation row")
    };
    let weekend = sidebar_row("Weekend crew");
    let jules = sidebar_row("Jules Lee");
    assert!(weekend.contains("Weekend crew"));
    assert!(weekend.trim_end().ends_with("Weekend crew"));
    assert!(jules.trim_end().ends_with("Jules Lee"));
}

#[test]
fn settings_panel_shows_selected_local_preferences() {
    let app = App {
        settings_open: true,
        settings_category: 1,
        settings_selection: 1,
        compact_sidebar: true,
        wrap_composer: false,
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(
        screen.lines().any(|line| line.contains("│  Settings")),
        "the picker owns the Settings title:\n{screen}"
    );
    assert!(
        !screen
            .lines()
            .next()
            .unwrap_or_default()
            .contains("Settings"),
        "the app header does not duplicate the picker title:\n{screen}"
    );
    assert!(screen.contains("Layout"), "layout tab:\n{screen}");
    assert!(
        screen.contains("Sidebar density"),
        "sidebar setting:\n{screen}"
    );
    assert!(screen.contains("Compact"), "compact value:\n{screen}");
    assert!(screen.contains("Wrap composer"), "wrap setting:\n{screen}");
    assert!(screen.contains("Off"), "wrap value:\n{screen}");
    assert!(
        screen.contains("stored locally"),
        "settings note:\n{screen}"
    );
    assert!(screen.contains("Esc close"), "settings help:\n{screen}");
}

#[test]
fn settings_picker_uses_measured_geometry_at_100x30() {
    let selected = conversation("Weekend crew", 2, 10);
    let mut app = App {
        settings_open: true,
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    app.selected = Some(selected.id.clone());
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();

    let popup = hits.popup.expect("settings panel");
    assert_eq!(
        popup.height, 8,
        "panel measures title, tabs, option, copy and padding"
    );
    assert_eq!(
        popup.y + popup.height,
        hits.composer.y - 1,
        "panel meets composer band"
    );
    assert!(
        popup.height < 13,
        "one-option picker must not keep a tall fixed slab"
    );
    let (row, index) = hits.settings_rows[0];
    assert_eq!(index, 0);
    assert_eq!(row.x, popup.x, "selection hitbox fills the picker width");
    assert_eq!(row.width, popup.width);
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::SettingsRow(0)));

    let grid = terminal.backend().buffer();
    let selected_bg = grid.cell((row.x, row.y)).unwrap().bg;
    assert_eq!(
        grid.cell((row.x + 2, row.y)).unwrap().symbol(),
        "›",
        "option text has the shared two-cell inset"
    );
    let tab = hits.settings_tabs[0].0;
    assert_ne!(
        grid.cell((tab.x + 1, tab.y)).unwrap().bg,
        selected_bg,
        "category tab is neutral while the option owns the accent"
    );
    let text = rendered(&app);
    let header = text.lines().next().unwrap_or_default();
    assert!(
        header.contains("Weekend crew"),
        "settings preserves chat context:\n{text}"
    );
    assert!(
        !header.contains("│Settings"),
        "header does not duplicate panel title:\n{text}"
    );
    assert!(
        text.contains("Cycles Automatic"),
        "description remains visible:\n{text}"
    );
    assert!(
        text.contains("stored locally"),
        "local note remains visible:\n{text}"
    );

    let footer_y = (0..grid.area.height)
        .find(|&y| row_text(grid, y).contains("Esc close"))
        .expect("footer row");
    let composer_bg = grid.cell((hits.composer.x, hits.composer.y)).unwrap().bg;
    assert_ne!(grid.cell((0, footer_y)).unwrap().bg, composer_bg);
    assert!(
        footer_y >= hits.composer.y,
        "footer remains below the composer band"
    );
}

#[test]
fn settings_picker_preserves_controls_at_66x18() {
    let app = App {
        settings_open: true,
        settings_category: 1,
        ..Default::default()
    };
    let backend = TestBackend::new(66, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();

    let popup = hits.popup.expect("settings panel");
    assert!(
        popup.height < 13,
        "responsive panel is bounded by its content/viewport"
    );
    assert_eq!(
        hits.settings_rows.len(),
        2,
        "both layout controls remain selectable"
    );
    assert!(popup.x + popup.width <= 66 && popup.y + popup.height <= 18);
    let text = rendered(&app);
    assert!(
        text.contains("Sidebar density"),
        "first control remains visible:\n{text}"
    );
    assert!(
        text.contains("Wrap composer"),
        "second control remains visible:\n{text}"
    );
    for (index, (row, selected)) in hits.settings_rows.iter().enumerate() {
        assert_eq!(*selected, index);
        assert_eq!(row.x, popup.x);
        assert_eq!(row.width, popup.width);
        assert!(row.y >= popup.y && row.y < popup.y + popup.height);
        assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::SettingsRow(index)));
    }
    assert_eq!(hits.settings_tabs.len(), 2);
    assert!(hits
        .settings_tabs
        .iter()
        .all(|(tab, _)| { tab.x >= popup.x && tab.x + tab.width <= popup.x + popup.width }));
}

#[test]
fn settings_tabs_switch_by_mouse_and_keep_rows_consistent() {
    let mut app = App {
        settings_open: true,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let layout_tab = hits
        .settings_tabs
        .iter()
        .find(|(_, index)| *index == 1)
        .map(|(area, _)| *area)
        .expect("Layout tab hitbox");
    assert_eq!(
        hits.at(layout_tab.x, layout_tab.y),
        Some(ui::Hit::SettingsTab(1))
    );
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        layout_tab.x,
        layout_tab.y,
        &hits,
    );
    assert_eq!(app.settings_category, 1);
    let screen = rendered(&app);
    assert!(
        screen.contains("Sidebar density"),
        "layout options:\n{screen}"
    );
    assert!(
        screen.contains("Wrap composer"),
        "layout options:\n{screen}"
    );
}

#[test]
fn appearance_category_shows_cycleable_theme() {
    let app = App {
        settings_open: true,
        settings_category: 0,
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(screen.contains("Appearance"), "appearance tab:\n{screen}");
    assert!(
        screen.contains("Automatic (Dark)"),
        "theme value:\n{screen}"
    );
    assert!(
        !screen.contains("Sidebar density"),
        "layout options stay separate:\n{screen}"
    );
}

#[test]
fn popup_surfaces_match_outgoing_chat_line_background() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        settings_open: true,
        settings_category: 1,
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    app.upsert(
        selected.id.clone(),
        message(
            &selected,
            "surface-match",
            1_700_000_000_000,
            "popup chat surface sample",
            true,
        ),
    );
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let grid = terminal.backend().buffer();
    let (chat_row, chat_text) = (0..grid.area.height)
        .map(|row| (row, row_text(grid, row)))
        .find(|(_, text)| text.contains("popup chat surface sample"))
        .expect("outgoing chat line");
    let chat_column = chat_text.find("popup chat surface sample").unwrap() as u16;
    let chat_bg = grid.cell((chat_column, chat_row)).unwrap().bg;
    let popup_row = hits
        .settings_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .map(|(area, _)| *area)
        .expect("unselected popup row");
    assert_eq!(grid.cell((popup_row.x, popup_row.y)).unwrap().bg, chat_bg);

    let mut command_app = App {
        composing: true,
        ..Default::default()
    };
    command_app.set_conversations(vec![selected]);
    command_app.draft = "/".into();
    command_app.command_menu = true;
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut command_hits = ui::HitMap::default();
    terminal
        .draw(|frame| command_hits = ui::render(frame, &command_app))
        .unwrap();
    let grid = terminal.backend().buffer();
    let menu_row = command_hits
        .menu_rows
        .iter()
        .find(|(_, index)| *index != command_app.command_selection)
        .map(|(area, _)| *area)
        .expect("unselected command row");
    assert_eq!(grid.cell((menu_row.x, menu_row.y)).unwrap().bg, chat_bg);
    let command_panel = command_hits.menu.expect("command panel hitbox");
    let bridge_y = command_panel.y + command_panel.height;
    assert!(bridge_y < grid.area.height, "popup bridge remains visible");
    assert_eq!(
        grid.cell((command_panel.x, bridge_y)).unwrap().bg,
        chat_bg,
        "the row touching the composer must share the outgoing-message surface"
    );
}

#[test]
fn settings_contacts_and_theme_panels_overlay_the_transcript() {
    let selected = conversation("Weekend crew", 3, 40);
    let make_app = |settings_open, contacts_open, theme_panel_open| {
        let mut app = App {
            settings_open,
            contacts_open,
            theme_panel_open,
            selected: Some(selected.id.clone()),
            ..Default::default()
        };
        app.set_conversations(vec![selected.clone()]);
        app.upsert(
            selected.id.clone(),
            message(
                &selected,
                "overlay-proof",
                1_700_000_000_000,
                "transcript remains visible behind the popup",
                false,
            ),
        );
        app
    };

    for (panel, title) in [
        (make_app(true, false, false), "Settings"),
        (make_app(false, true, false), "Contacts"),
        (make_app(false, false, true), "Theme"),
    ] {
        let text = rendered(&panel);
        assert!(text.contains(title), "missing {title} panel:\n{text}");
        assert!(
            text.contains("transcript remains visible"),
            "{title} replaced the transcript:\n{text}"
        );
    }

    let panel = make_app(false, false, true);
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &panel))
        .unwrap();
    let (row, index) = hits.theme_rows.first().copied().expect("theme row hitbox");
    assert_eq!(index, 0);
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::ThemeRow(0)));

    let tiny = buffer(&panel, 20, 5);
    assert_eq!((tiny.area.width, tiny.area.height), (20, 5));
}

#[test]
fn a_tall_picker_clears_orphan_transcript_text_not_just_its_background() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App::default();
    app.set_conversations(vec![selected.clone()]);
    app.upsert(
        selected.id.clone(),
        message(
            &selected,
            "last",
            1_700_000_000_000,
            "Orphan transcript fragment",
            false,
        ),
    );
    app.open_settings();
    app.settings_category = 1;
    let mut terminal = Terminal::new(TestBackend::new(66, 18)).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.unwrap();
    let excerpt_rows = popup.y.saturating_sub(hits.transcript.y);
    assert!(
        excerpt_rows > 0 && excerpt_rows < 3,
        "fixture reproduces a short excerpt"
    );
    for y in hits.transcript.y..popup.y {
        let row = &row_text(terminal.backend().buffer(), y);
        assert!(
            row.split('│').nth(1).unwrap_or(row).trim().is_empty(),
            "orphan text must be cleared: {row:?}"
        );
    }
}

#[test]
fn theme_picker_is_measured_and_responsive() {
    let app = App {
        theme_panel_open: true,
        theme_panel_selection: 1,
        ..Default::default()
    };
    for (width, height) in [(100u16, 30u16), (66, 18), (44, 12)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::render(frame, &app))
            .unwrap();
        let popup = hits.popup.expect("Theme panel");
        assert!(popup.x + popup.width <= width && popup.y + popup.height <= height);
        assert_eq!(hits.theme_rows.len(), 3, "all options at {width}x{height}");
        let grid = terminal.backend().buffer();
        let title_y = (popup.y..popup.y + popup.height)
            .find(|&y| row_text(grid, y).contains("Theme"))
            .expect("Theme title");
        assert_eq!(column_of(grid, title_y, "Theme"), Some(popup.x + 2));
        let (selected, index) = hits.theme_rows[1];
        assert_eq!(index, 1);
        assert_eq!(selected.x, popup.x);
        assert_eq!(selected.width, popup.width);
        assert_eq!(hits.at(selected.x, selected.y), Some(ui::Hit::ThemeRow(1)));
        assert_eq!(
            grid.cell((selected.x, selected.y)).unwrap().bg,
            grid.cell((selected.x + 1, selected.y)).unwrap().bg
        );
        assert_eq!(
            grid.cell((selected.x + 2, selected.y)).unwrap().symbol(),
            "›"
        );
        for (row, option) in hits.theme_rows.iter().zip(["Automatic", "Dark", "Light"]) {
            assert_eq!(column_of(grid, row.0.y, option), Some(popup.x + 4));
            assert!(row.0.y >= popup.y && row.0.y < popup.y + popup.height);
        }
        assert_eq!(popup.y + popup.height, hits.composer.y - 1);
        let footer_y = (0..height)
            .find(|&y| row_text(grid, y).contains("Esc close"))
            .expect("Theme footer");
        assert!(footer_y > hits.composer.y);
        assert!(selected.y + selected.height < hits.composer.y);
    }
}

#[test]
fn bottom_popups_use_one_inset_and_keep_controls_in_the_footer() {
    let app = App {
        settings_open: true,
        ..Default::default()
    };
    let text = rendered(&app);
    assert!(
        text.contains("Esc close"),
        "controls remain in the footer:\n{text}"
    );
    assert!(
        !text.contains("Esc close · ↑↓ move · Enter select"),
        "the title row contains no repeated controls:\n{text}"
    );
    assert!(
        !text.contains("Settings · Tab chats · ↑↓ choose · Enter change"),
        "the composer line contains no repeated controls:\n{text}"
    );

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.expect("settings popup");
    assert!(popup.height >= 6);
    assert_eq!(hits.settings_tabs.len(), 2);
    let layout_tab = hits
        .settings_tabs
        .iter()
        .find(|(_, index)| *index == 1)
        .map(|(area, _)| *area)
        .expect("Layout tab hitbox");
    assert_eq!(
        hits.at(layout_tab.x, layout_tab.y),
        Some(ui::Hit::SettingsTab(1))
    );
    let first_option = hits.settings_rows.first().expect("first settings option").0;
    assert_eq!(first_option.y, popup.y + 4, "room follows the tabs");
    let grid = terminal.backend().buffer();
    let top_padding = (popup.x..popup.x + popup.width)
        .map(|x| grid.cell((x, popup.y)).unwrap().symbol())
        .collect::<String>();
    assert!(
        top_padding.trim().is_empty(),
        "top padding is blank: {top_padding:?}"
    );
    assert!(
        row_text(grid, popup.y + 2).contains("Appearance"),
        "tabs follow the title"
    );
    let tab_gap = (popup.x..popup.x + popup.width)
        .map(|x| grid.cell((x, popup.y + 3)).unwrap().symbol())
        .collect::<String>();
    assert!(
        tab_gap.trim().is_empty(),
        "one row separates tabs and options: {tab_gap:?}"
    );

    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut compact_hits = ui::HitMap::default();
    terminal
        .draw(|frame| compact_hits = ui::render(frame, &app))
        .unwrap();
    let compact_popup = compact_hits.popup.expect("compact settings popup");
    assert!(compact_popup.height < 6);
    let first_option = compact_hits
        .settings_rows
        .first()
        .expect("compact settings option")
        .0;
    assert_eq!(
        first_option.y,
        compact_popup.y + 3,
        "compact layout retains the title and category before its option"
    );
}

#[test]
fn popup_spacing_bridge_focus_and_help_use_the_whole_layout() {
    let mut app = App {
        settings_open: true,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.unwrap();
    let option = hits.settings_rows[0].0;
    assert!(
        option.y >= popup.y + 4,
        "options need vertical breathing room"
    );
    let grid = terminal.backend().buffer();
    assert_eq!(grid.cell((option.x, option.y - 1)).unwrap().symbol(), " ");

    app.settings_open = false;
    app.contacts_open = true;
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.unwrap();
    let grid = terminal.backend().buffer();
    let surface = grid.cell((popup.x, popup.y + popup.height - 1)).unwrap().bg;
    for y in popup.y + popup.height..hits.composer.y {
        assert_eq!(
            grid.cell((popup.x, y)).unwrap().bg,
            surface,
            "contacts popup must join composer shading at row {y}"
        );
    }
    assert_eq!(
        hits.contacts_rows.len(),
        0,
        "empty contacts have no selectable rows"
    );
    assert!(
        rendered(&app).contains("No contacts yet"),
        "empty-state hint remains visible"
    );

    app.contacts_open = false;
    app.focus = Focus::Sidebar;
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let sidebar_bg = terminal
        .backend()
        .buffer()
        .cell((hits.composer.x, hits.composer.y))
        .unwrap()
        .bg;
    app.focus = Focus::Composer;
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert_eq!(
        sidebar_bg,
        terminal
            .backend()
            .buffer()
            .cell((hits.composer.x, hits.composer.y))
            .unwrap()
            .bg
    );

    app.help_open = true;
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let help = hits.help.unwrap();
    assert!(
        (i32::from(help.x) * 2 + i32::from(help.width) - 100).abs() <= 1,
        "help popup must center against the full 100-column TUI: {help:?}"
    );
    let short = buffer(&app, 60, 12);
    assert!(
        (0..short.area.height).any(|y| row_text(&short, y).contains("↑↓ scroll")),
        "short-terminal help must show how to reach hidden shortcuts"
    );
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    let scrolled = buffer(&app, 60, 12);
    assert!(
        (0..scrolled.area.height).any(|y| row_text(&scrolled, y).contains("Ctrl+C")),
        "hidden shortcuts become available by scrolling"
    );

    for (width, height, expected) in [
        (19, 24, "wheel"),
        (80, 4, "Keyboard shortcuts"),
        (80, 3, "Ctrl+C"),
    ] {
        let tiny_app = App {
            help_open: true,
            help_scroll: if height == 3 {
                bloom_tui::app::HELP_ROW_COUNT - 1
            } else {
                0
            },
            ..Default::default()
        };
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut tiny_hits = ui::HitMap::default();
        terminal
            .draw(|frame| tiny_hits = ui::render(frame, &tiny_app))
            .unwrap();
        assert!(
            tiny_hits.help.is_some(),
            "help remains visible at {width}x{height}"
        );
        let grid = terminal.backend().buffer();
        assert!(
            (0..height).any(|y| row_text(grid, y).contains(expected)),
            "help contains {expected:?} at {width}x{height}"
        );
    }
}

#[test]
fn help_modal_dismisses_outside_click_without_activating_underlying_chat() {
    let selected = conversation("Selected", 2, 10);
    let other = conversation("Other", 2, 20);
    let mut app = App {
        help_open: true,
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone(), other.clone()]);
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let outside = hits
        .sidebar_rows
        .iter()
        .find(|(_, id)| id == &other.id)
        .map(|(area, _)| *area)
        .expect("underlying other chat row");
    let help = hits.help.expect("help overlay");
    assert!(outside.x < help.x, "test click is outside the help panel");
    assert_eq!(hits.at(outside.x, outside.y), Some(ui::Hit::CloseOverlay));

    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        outside.x,
        outside.y,
        &hits,
    );
    assert!(!app.help_open, "outside click dismisses the help modal");
    assert_eq!(
        app.selected.as_ref(),
        Some(&selected.id),
        "outside click must not leak through and select the covered chat"
    );
}

#[test]
fn clicking_composer_below_slash_menu_restores_draft_editing() {
    let mut app = App {
        command_menu: true,
        draft: "/".into(),
        draft_cursor: 1,
        focus: Focus::Sidebar,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let composer = hits.composer;
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        composer.x + 4,
        composer.y,
        &hits,
    );
    assert_eq!(app.focus, Focus::Composer);
    assert_eq!(app.draft, "/");
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(app.draft.is_empty());
    assert!(!app.command_menu);
}

#[test]
fn new_and_filter_popups_show_real_options_and_hitboxes() {
    let direct = conversation("Jules Lee", 2, 20);
    let mut group = conversation("Weekend crew", 3, 30);
    group.kind = ConversationKind::Group;

    let mut new_app = App {
        new_conversation: true,
        ..Default::default()
    };
    new_app.set_conversations(vec![direct.clone(), group.clone()]);
    let new_screen = rendered(&new_app);
    assert!(new_screen.contains("New conversation"), "{new_screen}");
    assert!(new_screen.contains("Recent"), "{new_screen}");
    assert!(new_screen.contains("All"), "{new_screen}");
    assert!(new_screen.contains("Jules"), "{new_screen}");

    let mut filter_app = App {
        filter: Some(String::new()),
        filter_category: 1,
        selected: Some(direct.id.clone()),
        ..Default::default()
    };
    filter_app.set_conversations(vec![direct.clone(), group]);
    let filter_screen = rendered(&filter_app);
    assert!(
        filter_screen.contains("Filter conversations"),
        "{filter_screen}"
    );
    assert!(filter_screen.contains("All"), "{filter_screen}");
    assert!(filter_screen.contains("Direct"), "{filter_screen}");
    assert!(filter_screen.contains("Groups"), "{filter_screen}");
    assert!(filter_screen.contains("Jules Lee"), "{filter_screen}");
    assert!(!filter_screen.contains("Weekend crew"), "{filter_screen}");

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &new_app))
        .unwrap();
    let (row, index) = hits
        .new_contact_rows
        .first()
        .copied()
        .expect("new-contact recommendation hitbox");
    assert_eq!(index, 0);
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::NewContactRow(0)));
    let (tab, tab_index) = hits
        .new_contact_tabs
        .first()
        .copied()
        .expect("recommendation tab hitbox");
    assert_eq!(tab_index, 0);
    assert_eq!(hits.at(tab.x, tab.y), Some(ui::Hit::NewContactTab(0)));

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| hits = ui::render(frame, &filter_app))
        .unwrap();
    let popup = hits.popup.expect("shared Filter picker surface");
    let (tab, tab_index) = hits
        .filter_tabs
        .get(1)
        .copied()
        .expect("filter category hitbox");
    assert_eq!(tab_index, 1);
    assert_eq!(
        hits.filter_tabs[0].0.x,
        popup.x + 2,
        "Filter tabs share the two-cell inset"
    );
    assert_eq!(hits.at(tab.x, tab.y), Some(ui::Hit::FilterTab(1)));
    let (row, index) = hits
        .filter_rows
        .first()
        .copied()
        .expect("filtered row hitbox");
    assert_eq!(index, 0);
    assert_eq!(row.x, popup.x);
    assert_eq!(row.width, popup.width);
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::FilterRow(index)));
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((row.x + 2, row.y))
            .unwrap()
            .symbol(),
        "›"
    );
    assert_eq!(popup.y + popup.height, hits.composer.y - 1);
    assert_eq!(filter_screen.matches("Filter conversations").count(), 1);

    let count_row = row.y - 1;
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((popup.x + 2, count_row))
            .unwrap()
            .symbol(),
        "1",
        "count text begins at the shared two-cell inset"
    );
}

#[test]
fn filter_picker_keeps_rows_and_tabs_visible_at_narrow_sizes() {
    let direct = conversation("Jules Lee", 2, 20);
    let mut group = conversation("Weekend crew", 3, 30);
    group.kind = ConversationKind::Group;
    let selected_id = direct.id.clone();
    let mut app = App {
        filter: Some(String::new()),
        selected: Some(selected_id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![direct, group]);
    let selected_index = app
        .visible_conversations()
        .iter()
        .position(|conversation| conversation.id == selected_id)
        .expect("selected direct chat remains visible");

    for (width, height) in [(66u16, 18u16), (44, 12)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::render(frame, &app))
            .unwrap();
        let popup = hits.popup.expect("Filter picker remains visible");
        assert!(popup.x + popup.width <= width && popup.y + popup.height <= height);
        assert_eq!(hits.filter_tabs.len(), 3);
        assert_eq!(hits.filter_tabs[0].0.x, popup.x + 2);
        let (row, _) = hits
            .filter_rows
            .iter()
            .copied()
            .find(|(_, index)| *index == selected_index)
            .expect("selected filter row remains in view");
        assert_eq!(row.x, popup.x);
        assert_eq!(row.width, popup.width);
        assert_eq!(
            hits.at(row.x, row.y),
            Some(ui::Hit::FilterRow(selected_index))
        );
        assert!(row.y < popup.y + popup.height);
        assert!(hits.composer.y > popup.y);
        let grid = terminal.backend().buffer();
        let screen = (0..height)
            .map(|y| row_text(grid, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("Filter conversations"),
            "{width}x{height}:\n{screen}"
        );
        assert!(screen.contains("Jules Lee"), "{width}x{height}:\n{screen}");
        if width == 66 {
            assert!(screen.contains("Enter open"), "{width}x{height}:\n{screen}");
            assert!(screen.contains("Esc cancel"), "{width}x{height}:\n{screen}");
        } else {
            assert!(screen.contains("↵ open"), "{width}x{height}:\n{screen}");
            assert!(screen.contains("Esc"), "{width}x{height}:\n{screen}");
        }
    }
}

#[test]
fn filter_long_names_preserve_the_conversation_kind_at_narrow_width() {
    let long = conversation(
        "A very long conversation name that must be truncated",
        2,
        20,
    );
    let mut app = App {
        filter: Some(String::new()),
        ..Default::default()
    };
    app.set_conversations(vec![long]);
    let backend = TestBackend::new(44, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let grid = terminal.backend().buffer();
    let row = hits.filter_rows[0].0;
    let text = row_text(grid, row.y);
    assert!(
        text.contains("Direct"),
        "kind suffix remains visible: {text:?}"
    );
    assert!(
        text.contains("A very long conver"),
        "long label is clipped: {text:?}"
    );
}

#[test]
fn filter_empty_state_keeps_the_shared_content_inset() {
    let app = App {
        filter: Some("unmatched".into()),
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.expect("Filter picker surface");
    assert!(hits.filter_rows.is_empty());
    let buffer = terminal.backend().buffer();
    let row = (popup.y..popup.y + popup.height)
        .find(|row| column_of(buffer, *row, "No matching conversations").is_some())
        .expect("empty-state hint is visible");
    assert_eq!(
        column_of(buffer, row, "No matching conversations"),
        Some(popup.x + 2),
        "popup={popup:?}\n{}",
        rendered(&app)
    );
}

#[test]
fn emoji_suggestions_render_selectable_shortcodes() {
    let app = App {
        composing: true,
        draft: ":smil".into(),
        draft_cursor: ":smil".chars().count(),
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(screen.contains("Emoji suggestions"), "{screen}");
    assert!(screen.contains(":smile:"), "{screen}");
    assert!(screen.contains("😄"), "{screen}");

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let (row, index) = hits.emoji_rows.first().copied().expect("emoji row hitbox");
    assert_eq!(index, 0);
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::EmojiRow(0)));
}

#[test]
fn settings_remain_actionable_on_a_short_terminal() {
    let app = App {
        settings_open: true,
        settings_category: 1,
        ..Default::default()
    };
    let compact = buffer(&app, 40, 8);
    let screen = (0..compact.area.height)
        .map(|row| row_text(&compact, row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Sidebar"), "compact settings:\n{screen}");
    assert!(
        screen.contains("Wrap composer"),
        "compact settings retain both layout rows:\n{screen}"
    );
    assert!(screen.contains("Esc close"), "compact controls:\n{screen}");
    assert!(screen.contains("Settings"), "settings header:\n{screen}");
}

#[test]
fn footer_hints_follow_the_active_mode() {
    let normal = rendered(&App {
        connection_state: ConnectionState::Connected,
        ..Default::default()
    });
    assert!(
        normal.contains("/new start"),
        "empty-store action:\n{normal}"
    );
    assert!(normal.contains("? help"), "normal quit action:\n{normal}");
    assert!(
        normal.contains("Enter send · Ctrl+J newline · Tab chats · / commands · ? help"),
        "new composer hints:\n{normal}"
    );
    assert!(
        !normal.contains("Enter open"),
        "no selection to open:\n{normal}"
    );
    assert!(!normal.contains("Esc close"), "unsupported hint:\n{normal}");

    let composing = App {
        composing: true,
        draft: "hello".into(),
        draft_cursor: 5,
        ..Default::default()
    };
    let composing_text = rendered(&composing);
    assert!(
        composing_text.contains("Enter send"),
        "send hint:\n{composing_text}"
    );
    assert!(
        composing_text.contains("/ commands"),
        "command hint:\n{composing_text}"
    );
    assert!(
        composing_text.contains("Ctrl+J newline"),
        "multiline hint:\n{composing_text}"
    );
    assert!(
        !composing_text.contains("Enter open"),
        "stale nav hint:\n{composing_text}"
    );

    let wrapped = App {
        composing: true,
        draft: "x".repeat(100),
        draft_cursor: 100,
        ..Default::default()
    };
    let wrapped_text = rendered(&wrapped);
    assert!(
        wrapped_text.contains("↑↓ cursor"),
        "wrapped cursor hint:\n{wrapped_text}"
    );
    let sidebar_focus = App {
        focus: Focus::Sidebar,
        ..wrapped
    };
    let sidebar_text = rendered(&sidebar_focus);
    assert!(
        sidebar_text.contains("↑↓ chats"),
        "sidebar focus hint:\n{sidebar_text}"
    );
    assert!(
        sidebar_text.contains("Tab composer"),
        "sidebar return hint:\n{sidebar_text}"
    );
    assert!(
        sidebar_text.contains("Ctrl+P previous"),
        "previous-chat shortcut:\n{sidebar_text}"
    );

    let mut recipient = App::default();
    recipient.begin_new_conversation();
    let recipient_text = rendered(&recipient);
    assert!(
        recipient_text.contains("Enter continue"),
        "recipient hint:\n{recipient_text}"
    );

    recipient.draft = "person@example.test".into();
    let _ = recipient.submit_intent();
    let body_text = rendered(&recipient);
    assert!(
        body_text.contains("Enter start"),
        "new-chat body hint:\n{body_text}"
    );

    let mut filter = App::default();
    filter.begin_filter();
    filter.filter_input('z');
    let filter_text = rendered(&filter);
    assert!(
        filter_text.contains("↑↓ select"),
        "filter select hint:\n{filter_text}"
    );
    assert!(
        filter_text.contains("Enter open"),
        "filter open hint:\n{filter_text}"
    );
    assert!(
        filter_text.contains("Esc cancel"),
        "filter cancel hint:\n{filter_text}"
    );
    assert!(
        !filter_text.contains("/ filter"),
        "stale filter hint:\n{filter_text}"
    );
    assert_eq!(
        filter_text.matches("No matching conversations").count(),
        1,
        "overlay owns the empty-state copy:\n{filter_text}"
    );
}

#[test]
fn short_terminal_keeps_compact_footer_hints() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App::default();
    app.set_conversations(vec![selected]);
    let compact = buffer(&app, 40, 8);
    let compact_text = (0..compact.area.height)
        .map(|y| row_text(&compact, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        compact_text.contains("Write a message..."),
        "compact prompt:\n{compact_text}"
    );
    assert!(
        compact_text.contains("↵ send"),
        "compact send hint:\n{compact_text}"
    );
    assert!(
        compact_text.contains("? help"),
        "compact help hint:\n{compact_text}"
    );
    let compact_prompt = (0..compact.area.height)
        .find(|y| (0..compact.area.width).any(|x| compact[(x, *y)].symbol() == "›"))
        .unwrap();
    assert!(
        !row_text(&compact, compact_prompt + 1).contains("─"),
        "composer is bare:\n{compact_text}"
    );
    assert!(
        row_text(&compact, compact_prompt + 2).contains("↵ send"),
        "compact footer remains below the boundary:\n{compact_text}"
    );
    assert!(
        row_text(&compact, 0).contains("‹ Chats"),
        "compact layout exposes navigation"
    );
    assert!(
        !(0..compact.area.width).any(|x| compact[(x, 3)].symbol() == "│"),
        "narrow conversation owns the full width instead of a clipped rail"
    );

    let composing = App {
        composing: true,
        draft: "hello".into(),
        draft_cursor: 5,
        ..Default::default()
    };
    let compose_small = buffer(&composing, 34, 8);
    let compose_text = (0..compose_small.area.height)
        .map(|y| row_text(&compose_small, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        compose_text.contains("↵ send"),
        "compact composer hints:\n{compose_text}"
    );
    assert!(
        compose_text.contains("↵ send") && compose_text.contains("? help"),
        "small composer discovery:\n{compose_text}"
    );

    let mut filtering = App::default();
    filtering.begin_filter();
    filtering.filter_input('z');
    let filter_small = buffer(&filtering, 34, 8);
    let filter_text = (0..filter_small.area.height)
        .map(|y| row_text(&filter_small, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        filter_text.contains("↑↓") && filter_text.contains("↵"),
        "small filter selection and open hints:\n{filter_text}"
    );
    assert!(
        filter_text.contains("Esc"),
        "small filter cancel key:\n{filter_text}"
    );

    let narrow = buffer(&app, 60, 10);
    let narrow_text = (0..narrow.area.height)
        .map(|y| row_text(&narrow, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        narrow_text.contains("↵ send") || narrow_text.contains("Enter send"),
        "narrow footer:\n{narrow_text}"
    );
}

#[test]
fn long_header_title_does_not_overlap_status_or_theme() {
    let selected = conversation(
        "A conversation title that is much longer than the available header width",
        4,
        40,
    );
    let mut app = App {
        selected: Some(selected.id.clone()),
        connection: "Disconnected, retrying".into(),
        connection_state: ConnectionState::Disconnected,
        ..Default::default()
    };
    app.set_conversations(vec![selected]);

    let grid = buffer(&app, 100, 30);
    let header = row_text(&grid, 0);
    let title = column_of(&grid, 0, "A conversation").expect("chat title visible");
    let status = column_of(&grid, 0, "⠋ Offline").expect("status visible");
    let theme = column_of(&grid, 0, "Dark").expect("theme chip remains visible");
    assert!(title < status, "title/status overlap in {header:?}");
    assert!(
        header.rfind("Bloom").unwrap() > theme as usize,
        "Bloom stays at far right: {header:?}"
    );
    assert!(status < theme, "status/theme overlap in {header:?}");
    assert!(
        header.contains("A conversation title"),
        "selected title is shown: {header:?}"
    );
    assert!(
        !header.contains("much longer than the available header width"),
        "title is clipped before controls, not painted underneath them: {header:?}"
    );

    let narrow = buffer(&app, 60, 10);
    let narrow_header = row_text(&narrow, 0);
    assert!(
        narrow_header.contains("A conversation title"),
        "identity: {narrow_header:?}"
    );
    assert!(
        narrow_header.contains("Offline"),
        "short layout keeps status beside identity: {narrow_header:?}"
    );
}

#[test]
fn long_recipient_does_not_hide_new_message_draft() {
    let draft = "The message stays visible until I finish typing";
    let app = App {
        composing: true,
        pending_address: Some(format!(
            "person.{}.example.test",
            "long-address-".repeat(12)
        )),
        draft: draft.into(),
        draft_cursor: draft.chars().count(),
        ..Default::default()
    };

    let text = rendered(&app);
    assert!(
        text.contains(draft),
        "the recipient prefix must not obscure the body draft:\n{text}"
    );
}

#[test]
fn composer_and_own_messages_use_codex_style_surfaces() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        connection: "Connected".into(),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    app.upsert(
        selected.id.clone(),
        message(
            &selected,
            "m1",
            1_700_000_000_000,
            "A user message highlighted like Codex.",
            true,
        ),
    );

    let grid = buffer(&app, 100, 30);
    let input_row = (0..grid.area.height)
        .find(|y| (0..grid.area.width).any(|x| grid[(x, *y)].symbol() == "›"))
        .unwrap();
    let input_column = column_of(&grid, input_row, "›").unwrap();
    assert_eq!(
        grid[(input_column, input_row)].bg,
        ratatui::style::Color::Rgb(38, 38, 38)
    );

    let message_row = (0..grid.area.height)
        .find(|y| row_text(&grid, *y).contains("A user message highlighted"))
        .unwrap();
    let message_column = column_of(&grid, message_row, "A user message").unwrap();
    assert_eq!(
        grid[(message_column, message_row)].bg,
        ratatui::style::Color::Rgb(38, 38, 38)
    );
    assert_eq!(
        grid[(grid.area.width - 5, message_row)].bg,
        ratatui::style::Color::Rgb(38, 38, 38)
    );
}

#[test]
fn composer_cursor_aligns_with_ascii_draft() {
    let app = App {
        composing: true,
        draft: "abc".into(),
        draft_cursor: 2,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, &app);
        })
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let row = (0..grid.area.height)
        .find(|y| (0..grid.area.width).any(|x| grid[(x, *y)].symbol() == "›"))
        .unwrap();
    let prefix_column = column_of(&grid, row, "›  ").unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert_eq!(cursor.y, row);
    assert_eq!(cursor.x, prefix_column + 3 + 2);
}

#[test]
fn composer_cursor_tracks_wide_graphemes_and_long_drafts() {
    let mut app = App {
        composing: true,
        draft: "x👨‍👩‍👧‍👦y".into(),
        draft_cursor: 2,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, &app);
        })
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let row = (0..grid.area.height)
        .find(|y| (0..grid.area.width).any(|x| grid[(x, *y)].symbol() == "›"))
        .unwrap();
    let prefix_column = column_of(&grid, row, "›  ").unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let expected_prefix_width = unicode_width::UnicodeWidthStr::width("›  x👨‍👩‍👧‍👦");
    assert_eq!(
        (cursor.x, cursor.y),
        (prefix_column + expected_prefix_width as u16, row)
    );

    app.draft = "abcdefghijklmnopqrstuvwxyz".repeat(2);
    app.draft_cursor = app.draft.chars().count();
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let screen = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("abcdefghijklmnopqrstuv") && screen.contains("wxyz"),
        "the draft wraps without dropping its tail:\n{screen}"
    );
    assert!(
        hits.composer.height >= 2,
        "wrapped draft has multiple input rows"
    );
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.x < grid.area.width && cursor.y < grid.area.height);
}

#[test]
fn wrapped_draft_shows_both_overflow_indicators_and_mouse_cursor_target() {
    let draft = "abcdefghijklmnopqrstuvwxyz".repeat(40);
    let mut app = App {
        composing: true,
        draft,
        draft_cursor: 500,
        focus: Focus::Sidebar,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert_eq!(hits.composer.height, 5);
    let grid = terminal.backend().buffer();
    let composer_lines = (hits.composer.y..hits.composer.y + hits.composer.height)
        .map(|y| {
            (hits.composer.x..hits.composer.x + hits.composer.width)
                .map(|x| grid.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(
        composer_lines[0].contains("↑ "),
        "top overflow: {composer_lines:?}"
    );
    assert!(
        composer_lines[4].contains("↓ "),
        "bottom overflow: {composer_lines:?}"
    );
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.y >= hits.composer.y && cursor.y < hits.composer.y + hits.composer.height);

    let (x, y, index) = hits
        .composer_positions
        .iter()
        .copied()
        .find(|(_, _, index)| *index == 500)
        .expect("visible grapheme boundary under the cursor");
    assert_eq!(hits.at(x, y), Some(ui::Hit::Composer));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, y, &hits);
    assert_eq!(app.focus, Focus::Composer);
    assert_eq!(app.draft_cursor, index);
}

#[test]
fn short_terminal_keeps_multiline_cursor_and_overflow_cues() {
    let app = App {
        composing: true,
        draft: "abcdefghijklmnopqrstuvwxyz".repeat(40),
        draft_cursor: 500,
        ..Default::default()
    };
    let backend = TestBackend::new(100, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert!(hits.composer.height >= 3);
    let grid = terminal.backend().buffer();
    let first = row_text(grid, hits.composer.y);
    let last = row_text(grid, hits.composer.y + hits.composer.height - 1);
    let screen_width = grid.area.width;
    assert!(first.contains("↑ "), "top overflow: {first:?}");
    assert!(last.contains("↓ "), "bottom overflow: {last:?}");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.y >= hits.composer.y && cursor.y < hits.composer.y + hits.composer.height);
    assert!(cursor.x < screen_width);
}

#[test]
fn composer_width_helper_matches_the_rendered_input_width() {
    for (width, compact_sidebar) in [(40, false), (64, false), (100, false), (100, true)] {
        let app = App {
            compact_sidebar,
            ..Default::default()
        };
        let backend = TestBackend::new(width, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::render(frame, &app))
            .unwrap();
        assert_eq!(
            usize::from(hits.composer.width),
            ui::composer_columns(width, compact_sidebar),
            "width {width} with compact_sidebar={compact_sidebar}"
        );
    }
}

#[test]
fn wrapped_composer_keeps_wide_emoji_clusters_intact() {
    let family = "👨‍👩‍👧‍👦";
    let draft = format!("{}{}{}", "a".repeat(20), family, "b".repeat(20));
    let app = App {
        composing: true,
        draft,
        draft_cursor: 41,
        ..Default::default()
    };
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert!(hits.composer.height >= 2);
    let grid = terminal.backend().buffer();
    assert!(
        (hits.composer.y..hits.composer.y + hits.composer.height).any(|y| {
            (hits.composer.x..hits.composer.x + hits.composer.width)
                .any(|x| grid.cell((x, y)).unwrap().symbol() == family)
        }),
        "wide family emoji remains a single grapheme cell"
    );
}

#[test]
fn disabling_composer_wrap_restores_horizontal_scrolling() {
    let draft = "abcdefghijklmnopqrstuvwxyz";
    let app = App {
        composing: true,
        draft: draft.into(),
        draft_cursor: draft.chars().count(),
        wrap_composer: false,
        ..Default::default()
    };
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert_eq!(hits.composer.height, 1);
    let grid = terminal.backend().buffer();
    let input = (hits.composer.x..hits.composer.x + hits.composer.width)
        .map(|x| grid.cell((x, hits.composer.y)).unwrap().symbol())
        .collect::<String>();
    assert!(
        input.contains("uvwxyz"),
        "horizontal tail is visible: {input:?}"
    );
}

#[test]
fn small_composer_keeps_prompt_cursor_and_footer_in_bounds() {
    let app = App {
        composing: true,
        draft: "draft".into(),
        draft_cursor: 5,
        ..Default::default()
    };
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, &app);
        })
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.x < grid.area.width && cursor.y < grid.area.height);
    assert!(
        (0..grid.area.height).any(|y| (0..grid.area.width).any(|x| grid[(x, y)].symbol() == "›"))
    );
    assert!(grid.area.height <= 8);

    let backend = TestBackend::new(16, 5);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, &app);
        })
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.x < grid.area.width && cursor.y < grid.area.height);
    assert!(
        (0..grid.area.height).any(|y| (0..grid.area.width).any(|x| grid[(x, y)].symbol() == "›"))
    );
}

#[test]
fn rendering_at_small_sizes_does_not_panic() {
    let app = App::default();
    for (width, height) in [(100, 30), (60, 20), (20, 5)] {
        let _ = buffer(&app, width, height);
    }
}

#[test]
fn an_empty_active_composer_keeps_its_message_placeholder() {
    let app = App {
        composing: true,
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(
        screen.contains("Write a message..."),
        "composer prompt:\n{screen}"
    );
}

#[test]
fn new_conversation_mode_asks_for_a_recipient() {
    let mut app = App::default();
    app.begin_new_conversation();

    let text = rendered(&app);
    assert!(
        text.contains("To: recipient address"),
        "new message mode must ask for an address:\n{text}"
    );
}

#[test]
fn a_selection_deep_in_a_long_list_is_scrolled_into_view() {
    // The daemon lists hundreds of conversations, so a selection must never move
    // off-screen invisibly.
    let mut app = App::default();
    let conversations: Vec<_> = (0..300)
        .map(|index| conversation(&format!("chat {index:03}"), 2, 300 - index as i64))
        .collect();
    app.set_conversations(conversations.clone());
    let target = conversations[250].id.clone();
    app.selected = Some(target.clone());
    app.upsert(
        target.clone(),
        message(
            &conversations[250],
            "deep",
            1_700_000_000_000,
            "deep message",
            false,
        ),
    );

    let text = rendered(&app);
    assert!(
        text.contains("chat 250"),
        "the selected conversation must be visible:\n{text}"
    );
    assert!(
        !text.contains("chat 000"),
        "the list must have scrolled away from the top:\n{text}"
    );
}

#[test]
fn an_overheight_message_still_renders_its_tail() {
    let mut app = App::default();
    let only = conversation("Long message", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    app.upsert(
        only.id.clone(),
        message(
            &only,
            "huge",
            1_700_000_000_000,
            &"visible tail ".repeat(500),
            false,
        ),
    );
    let text = rendered(&app);
    assert!(text.contains("visible"), "message vanished:\n{text}");
}

#[test]
fn a_long_thread_shows_its_newest_messages() {
    let mut app = App::default();
    let only = conversation("Long thread", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..120 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index,
                &format!("message number {index:03}"),
                false,
            ),
        );
    }

    let text = rendered(&app);
    assert!(
        text.contains("message number 119"),
        "the newest message must be visible:\n{text}"
    );
    assert!(
        !text.contains("message number 000"),
        "the oldest message must have scrolled away:\n{text}"
    );
}

#[test]
fn the_filter_reports_matches_and_an_empty_state() {
    let mut app = App::default();
    let weekend = conversation("Weekend crew", 2, 30);
    let jules = conversation("Jules Lee", 2, 20);
    app.set_conversations(vec![weekend, jules]);

    app.begin_filter();
    for character in "jules".chars() {
        app.filter_input(character);
    }
    let filtered = rendered(&app);
    assert!(
        filtered.contains("1 MATCH"),
        "match count missing:\n{filtered}"
    );
    assert!(
        filtered.contains("Jules Lee"),
        "the match must still be listed"
    );
    assert!(
        !filtered.contains("Weekend crew"),
        "non-matches must be hidden"
    );

    app.cancel_filter();
    app.begin_filter();
    for character in "zzzz".chars() {
        app.filter_input(character);
    }
    let empty = rendered(&app);
    assert!(
        empty.contains("NO MATCHES"),
        "an empty result needs an explicit state:\n{empty}"
    );
    assert!(
        empty.contains("filter: zzzz"),
        "the prompt must show the query being filtered on:\n{empty}"
    );
}

#[test]
fn speaker_names_and_timestamps_align_into_columns() {
    let mut app = App::default();
    let only = conversation("Aligned", 2, 10);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    // Wide and over-cap names must still align timestamps without splitting characters.
    for (index, sender) in [
        "界界界界界界界界界界",
        "averyveryverylongname@example.invalid",
    ]
    .into_iter()
    .enumerate()
    {
        let message = Message::new(
            MessageId::new(
                TransportId::new("openbubbles").unwrap(),
                format!("m{index}"),
            )
            .unwrap(),
            only.id.clone(),
            None,
            Handle::new(HandleKind::Email, sender).unwrap(),
            vec![MessagePart::Text(format!("body {index}"))],
            1_700_000_000_000 + index as i64,
            None,
            DeliveryState::Delivered,
            false,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        app.upsert(only.id.clone(), message);
    }

    let grid = buffer(&app, 100, 24);
    let expected = bloom_tui::app::timestamp(1_700_000_000_000);
    // Compare DISPLAY columns, not byte offsets: a wide character occupies two
    // cells, so `str::find` on a flattened row reports a misleading index.
    let columns: Vec<u16> = ["界", "Averyveryver"]
        .iter()
        .map(|label| {
            (0..grid.area.height)
                .find_map(|y| {
                    let text = row_text(&grid, y);
                    if text.contains(label) && text.contains(&expected) {
                        column_of(&grid, y, &expected)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("no header for {label} in:\n{}", rendered(&app)))
        })
        .collect();
    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "timestamps must share a display column, found {columns:?}"
    );
}

#[test]
fn a_send_failure_is_visible_rather_than_silent() {
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 2, 10)]);
    app.connection = "Connected".into();
    app.session = "Active".into();
    app.error = Some("send failed: transport unavailable".to_owned());

    let text = rendered(&app);
    assert!(
        text.contains("send failed"),
        "an error must be shown somewhere visible:\n{text}"
    );
}

#[test]
fn scrolling_back_reveals_older_messages_and_returning_shows_the_newest() {
    let mut app = App::default();
    let only = conversation("Long thread", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..120 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index,
                &format!("message number {index:03}"),
                false,
            ),
        );
    }

    // Default is pinned to the newest content.
    assert!(rendered(&app).contains("message number 119"));

    app.scroll_to_oldest();
    let oldest = rendered(&app);
    assert!(
        oldest.contains("message number 000"),
        "scrolling back must reach the oldest message:\n{oldest}"
    );
    assert!(
        !oldest.contains("message number 119"),
        "the newest message must scroll out of view:\n{oldest}"
    );

    app.scroll_to_newest();
    assert!(rendered(&app).contains("message number 119"));
}

#[test]
fn a_single_over_height_message_still_shows_its_tail_when_pinned() {
    // The transcript no longer drops an over-height message: at the default
    // position its final rows are what the viewport shows.
    let mut app = App::default();
    let only = conversation("Huge", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    app.upsert(
        only.id.clone(),
        message(
            &only,
            "huge",
            1_700_000_000_000,
            &"end of the message ".repeat(200),
            false,
        ),
    );
    let text = rendered(&app);
    assert!(
        text.contains("end of the message"),
        "an over-height message must render, not vanish:\n{text}"
    );
    assert!(
        !text.contains("message number"),
        "this conversation has only the one message"
    );
}

#[test]
fn the_scrolled_state_explains_itself_without_changing_the_default_hint_row() {
    let mut app = App::default();
    let only = conversation("Long thread", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..40 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index,
                &format!("message number {index:03}"),
                false,
            ),
        );
    }

    // The default view keeps the mockup's hint row untouched.
    let pinned = rendered(&app);
    assert!(
        pinned.contains("Enter send"),
        "the default hint row must describe the composer action:\n{pinned}"
    );
    assert!(!pinned.contains("reading back"));

    app.scroll_to_oldest();
    let scrolled = rendered(&app);
    assert!(
        scrolled.contains("reading back") && scrolled.contains("End returns"),
        "a scrolled transcript must say how to get back:\n{scrolled}"
    );
}

#[test]
fn short_thread_with_stale_scroll_does_not_claim_to_be_reading_back() {
    let mut app = App::default();
    let only = conversation("Short thread", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    app.upsert(
        only.id.clone(),
        message(
            &only,
            "only",
            1_700_000_000_000,
            "One short message.",
            false,
        ),
    );
    app.scroll = 50;

    let text = rendered(&app);
    assert!(
        text.contains("Enter send"),
        "normal hints remain visible:\n{text}"
    );
    assert!(
        !text.contains("reading back"),
        "clamped scroll must not claim an older view:\n{text}"
    );
}

#[test]
fn no_scroll_position_ends_the_transcript_on_a_bare_byline() {
    // A byline whose body is below the fold reads as a rendering fault. Walk every
    // scroll position and assert the row above the composer rule is never a byline.
    let mut app = App::default();
    let only = conversation("Long thread", 2, 1);
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..24 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index * 60_000,
                &format!("message number {index:03} with some words in it"),
                index % 2 == 0,
            ),
        );
    }

    let mut checked = 0;
    for back in 0..60 {
        app.scroll = back;
        let text = rendered(&app);
        let rows: Vec<&str> = text.lines().collect();
        // The shaded band has one top pad and the reserved transcript separator
        // sits above it, so the transcript viewport's final row is three rows up.
        let composer = rows
            .iter()
            .position(|row| row.contains('›'))
            .expect("the composer prompt must be drawn");
        let transcript_bottom = rows[composer - 3];
        let has_clock = transcript_bottom
            .split(|c: char| !c.is_ascii_digit() && c != ':')
            .any(|token| {
                let parts: Vec<&str> = token.split(':').collect();
                parts.len() == 2
                    && parts[0].len() == 2
                    && parts[1].len() == 2
                    && !parts[0].is_empty()
            });
        assert!(
            !has_clock,
            "scroll position {back} ends the transcript on a bare byline:\n{text}"
        );
        checked += 1;
    }
    assert!(checked > 40, "the test must actually walk scroll positions");
}

#[test]
fn the_theme_control_sits_one_column_inside_the_frame() {
    // The mockup's header control has padding inside the window. The chip must be
    // right-aligned with a gap, not flush against the frame edge.
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 3, 40)]);
    app.selected = app.conversations.first().map(|c| c.id.clone());
    app.connection = "Connected".into();
    app.session = "Active".into();

    for (theme, expected) in [(Theme::Dark, "Dark"), (Theme::Light, "Light")] {
        app.theme = theme;
        let text = rendered(&app);
        let header = text.lines().next().expect("a header row");
        let label = format!(" {expected} ");
        let at = header
            .find(&label)
            .unwrap_or_else(|| panic!("the {expected} control must be in the header:\n{text}"));
        let left_border = header[..at].rfind('│').expect("chip left border");
        let end = at + label.len();
        let right_border = header[end..]
            .find('│')
            .map(|offset| end + offset)
            .expect("chip right border");
        assert!(left_border < at && right_border >= end);
        assert!(header[right_border..].contains('⚙') && header[right_border..].contains('?'));
    }
}

#[test]
fn a_single_match_heading_is_not_followed_by_leftovers() {
    // The filter heading used to be drawn over the plain `Chats` label, and the
    // shorter text left the end of the old label showing, so it read `1 MATCH S`.
    let mut app = App::default();
    app.set_conversations(vec![
        conversation("Jules Lee", 2, 20),
        conversation("Maya Chen", 2, 10),
    ]);
    app.begin_filter();
    for character in "jules".chars() {
        app.filter_input(character);
    }

    let text = rendered(&app);
    let heading = text.lines().next().expect("a label row");
    assert!(
        heading.contains("1 MATCH"),
        "the singular heading must be shown:\n{text}"
    );
    assert!(
        !heading.contains("MATCH S"),
        "no tail of the plain label may survive:\n{text}"
    );
}

#[test]
fn narrow_header_prioritizes_identity_over_optional_theme() {
    // On narrow terminals the theme chip is hidden while usable hints remain.
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 3, 40)]);
    app.selected = app.conversations.first().map(|c| c.id.clone());
    let grid = buffer(&app, 60, 20);
    let row: String = (0..grid.area.width)
        .map(|x| grid[(x, 0)].symbol())
        .collect();
    assert!(
        row.contains("Weekend crew") && !row.contains("Dark"),
        "identity takes precedence at 60 columns:\n{row}"
    );
    let footer: String = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        (footer.contains("↵ send") || footer.contains("Enter send")) && footer.contains("? help"),
        "the narrow hints retain submission and discovery, got:\n{footer}"
    );
}

#[test]
fn selected_sidebar_chat_stays_highlighted_in_both_focus_states() {
    let selected = conversation("Weekend crew", 3, 40);
    let other = conversation("Jules Lee", 2, 30);
    for focus in [Focus::Composer, Focus::Sidebar] {
        let mut app = App {
            selected: Some(selected.id.clone()),
            focus,
            ..Default::default()
        };
        app.set_conversations(vec![selected.clone(), other.clone()]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::render(frame, &app))
            .unwrap();
        let selected_row = hits
            .sidebar_rows
            .iter()
            .find(|(_, id)| id == &selected.id)
            .map(|(area, _)| *area)
            .expect("selected chat row");
        let other_row = hits
            .sidebar_rows
            .iter()
            .find(|(_, id)| id == &other.id)
            .map(|(area, _)| *area)
            .expect("unselected chat row");
        let grid = terminal.backend().buffer();
        for offset in 0..selected_row.height {
            assert_ne!(
                grid.cell((selected_row.x, selected_row.y + offset))
                    .unwrap()
                    .bg,
                grid.cell((other_row.x, other_row.y + offset)).unwrap().bg,
                "selected row remains highlighted with focus {focus:?}"
            );
        }
        assert!(row_text(grid, selected_row.y).contains("▎"));
    }
}

#[test]
fn sidebar_entries_show_a_clipped_preview_and_highlight_both_rows() {
    let mut first = conversation("Weekend crew", 3, 40);
    first.preview = Some("I'll check the tide times tonight and send the link here.".into());
    let mut second = conversation("Jules Lee", 2, 20);
    second.preview = Some("Sending the deck".into());
    let mut app = App {
        selected: Some(first.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![first, second]);
    let grid = buffer(&app, 100, 30);
    let sidebar = |y: u16| {
        row_text(&grid, y)
            .split('│')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let name_row = (0..30)
        .find(|y| sidebar(*y).contains("Weekend crew"))
        .expect("name row");
    let preview = sidebar(name_row + 1);
    assert!(
        preview.contains("I'll check"),
        "preview under the name: {preview:?}"
    );
    assert!(
        preview.trim_end().ends_with('…'),
        "long preview is clipped: {preview:?}"
    );
    assert!(sidebar(name_row + 3).contains("Sending the deck"));

    // The selection fills both rows of the chosen chat and no other row.
    let selected_bg = grid[(4, name_row)].bg;
    assert_eq!(
        grid[(4, name_row + 1)].bg,
        selected_bg,
        "highlight spans both rows"
    );
    assert_ne!(
        grid[(4, name_row + 2)].bg,
        selected_bg,
        "other chats are not highlighted"
    );
    assert_eq!(grid[(0, name_row)].symbol(), "▎");
    assert_eq!(
        grid[(0, name_row + 1)].symbol(),
        "▎",
        "marker repeats on preview row"
    );
}

#[test]
fn sidebar_header_spacer_preserves_row_hit_testing_across_modes() {
    let selected = conversation("Weekend crew", 2, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);

    for (height, spacer, mode) in [
        (30u16, true, "normal"),
        (6, true, "short"),
        (30, true, "settings"),
        (30, true, "contacts"),
        (30, true, "help"),
        (30, true, "filter"),
    ] {
        app.settings_open = mode == "settings";
        app.contacts_open = mode == "contacts";
        app.help_open = mode == "help";
        app.filter = (mode == "filter").then(String::new);
        let grid = buffer(&app, 100, height);
        let row = |y| {
            row_text(&grid, y)
                .split('│')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let chat_y = if spacer { 2 } else { 1 };
        assert!(
            row(chat_y).contains("Weekend crew"),
            "mode={mode}: {grid:?}"
        );
        if spacer {
            assert!(row(1).trim().is_empty(), "sidebar spacer in mode={mode}");
        }

        let backend = TestBackend::new(100, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = ui::HitMap::default();
        terminal
            .draw(|frame| hits = ui::render(frame, &app))
            .unwrap();
        let expected_hit = if mode == "help" {
            Some(ui::Hit::CloseOverlay)
        } else {
            Some(ui::Hit::SidebarChat(selected.id.clone()))
        };
        assert_eq!(
            hits.at(2, chat_y),
            expected_hit,
            "chat hitbox follows shifted row in mode={mode}"
        );
        if spacer {
            assert_ne!(
                hits.at(2, 1),
                Some(ui::Hit::SidebarChat(selected.id.clone())),
                "spacer is not clickable as a chat"
            );
        }
    }

    app.settings_open = false;
    app.help_open = false;
    let tiny = buffer(&app, 100, 5);
    assert_eq!(tiny.area.height, 5, "tiny fallback remains renderable");
}

#[test]
fn contacts_view_shows_local_nicknames_and_renders_narrow_layouts() {
    let mut contact = conversation("person@example.test", 1, 10);
    contact.participants[0].is_me = false;
    contact.participants[0].handle = Handle::new(HandleKind::Email, "person@example.test").unwrap();
    let handle = contact.participants[0].handle.clone();
    let mut app = App {
        contacts_open: true,
        ..Default::default()
    };
    app.set_conversations(vec![contact]);
    app.nicknames
        .insert(bloom_tui::app::nickname_key(&handle), "Maya".into());

    for (width, height) in [(100u16, 30u16), (66, 18), (44, 12), (20, 5)] {
        let grid = buffer(&app, width, height);
        let text = (0..height)
            .map(|y| row_text(&grid, y))
            .collect::<Vec<_>>()
            .join("\n");
        if width >= 44 {
            assert!(text.contains("Contacts"));
            assert!(
                text.contains("Search by number or nickname"),
                "size={width}x{height}:\n{text}"
            );
            assert!(text.contains("Maya"), "size={width}x{height}:\n{text}");
            assert!(
                text.contains(if width >= 100 {
                    "mailto:person@example.test"
                } else {
                    "mailto:"
                }),
                "size={width}x{height}:\n{text}"
            );
        }
        assert_eq!(grid.area.width, width);
        assert_eq!(grid.area.height, height);
    }

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let popup = hits.popup.expect("Contacts picker surface");
    let (area, index) = hits
        .contacts_rows
        .first()
        .copied()
        .expect("contact row hitbox");
    assert_eq!(index, 0);
    assert_eq!(area.x, popup.x, "selected background fills the panel width");
    assert_eq!(area.width, popup.width);
    assert_eq!(hits.at(area.x, area.y), Some(ui::Hit::ContactRow(0)));
    let grid = terminal.backend().buffer();
    assert_eq!(grid.cell((area.x + 2, area.y)).unwrap().symbol(), "›");
    let tab_line = row_text(grid, hits.contacts_tabs[0].0.y);
    assert_eq!(
        tab_line.matches("All").count(),
        1,
        "tabs render once: {tab_line:?}"
    );
    assert_eq!(
        hits.contacts_tabs[0].0.x,
        popup.x + 2,
        "tabs share Settings inset"
    );
    assert_eq!(popup.y + popup.height, hits.composer.y - 1);
    let (tab, tab_index) = hits
        .contacts_tabs
        .get(1)
        .copied()
        .expect("nickname tab hitbox");
    assert_eq!(tab_index, 1);
    assert_eq!(hits.at(tab.x, tab.y), Some(ui::Hit::ContactTab(1)));
}

#[test]
fn contacts_search_renders_query_no_results_and_narrow_layouts() {
    let mut app = App {
        contacts_open: true,
        contacts_query: "no-such-contact".into(),
        ..Default::default()
    };
    app.set_conversations(vec![conversation("Known contact", 2, 1)]);

    let text = rendered(&app);
    assert!(
        text.contains("Search: no-such-contact"),
        "search field:\n{text}"
    );
    assert!(text.contains("0 matches"), "match count:\n{text}");
    assert!(text.contains("No contacts match"), "empty state:\n{text}");

    for (width, height) in [(44u16, 12u16), (20, 5)] {
        let grid = buffer(&app, width, height);
        assert_eq!((grid.area.width, grid.area.height), (width, height));
    }
}

#[test]
fn consecutive_contacts_keep_last_selection_visible_and_clickable() {
    let conversations = (0..20)
        .map(|index| {
            let mut chat = conversation(&format!("person{index}@example.test"), 1, index);
            chat.participants[0].is_me = false;
            chat.participants[0].handle =
                Handle::new(HandleKind::Email, format!("person{index}@example.test")).unwrap();
            chat
        })
        .collect();
    let mut app = App {
        contacts_open: true,
        ..Default::default()
    };
    app.set_conversations(conversations);
    app.contacts_selection = 19;
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let (row, _) = hits
        .contacts_rows
        .iter()
        .find(|(_, index)| *index == 19)
        .expect("last contact scrolls into view");
    assert_eq!(row.height, 1, "contact rows have no blank gap");
    assert_eq!(hits.at(row.x, row.y), Some(ui::Hit::ContactRow(19)));
    assert!(
        hits.contacts_rows
            .windows(2)
            .all(|pair| { pair[1].0.y == pair[0].0.y + 1 && pair[1].1 == pair[0].1 + 1 }),
        "visible contacts are consecutive: {:?}",
        hits.contacts_rows
    );
    assert!(row_text(terminal.backend().buffer(), row.y)
        .contains(app.visible_contact_handles()[19].value()));
}

#[test]
fn composer_band_shades_one_row_above_and_below_the_input() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let (width, height) = (100u16, 30u16);
    let grid = buffer(&app, width, height);
    let prompt_y = (0..height)
        .find(|y| row_text(&grid, *y).contains("›"))
        .expect("composer prompt row");
    let surface = grid[(width - 1, prompt_y)].bg;
    for offset in [-1i32, 0, 1] {
        let y = (prompt_y as i32 + offset) as u16;
        assert_eq!(
            grid[(width - 1, y)].bg,
            surface,
            "band row at offset {offset}"
        );
    }
    let divider = column_of(&grid, prompt_y, "│").expect("sidebar divider");
    for offset in [-1i32, 0, 1] {
        let y = (prompt_y as i32 + offset) as u16;
        assert_eq!(
            grid[(divider + 1, y)].bg,
            surface,
            "band reaches pane edge at offset {offset}"
        );
    }
    let footer_y = prompt_y + 3;
    assert_ne!(
        grid[(width - 1, footer_y)].bg,
        surface,
        "footer row sits outside the band"
    );
}

#[test]
fn tiny_composer_surface_only_shades_the_input_row() {
    for theme in [Theme::Dark, Theme::Light] {
        let app = App {
            theme,
            ..Default::default()
        };
        for (width, height) in [(19, 8), (40, 4)] {
            let grid = buffer(&app, width, height);
            let input_bg = grid[(0, 0)].bg;
            for x in 0..width {
                assert_eq!(grid[(x, 0)].bg, input_bg);
                for y in 1..height {
                    assert_ne!(
                        grid[(x, y)].bg,
                        input_bg,
                        "only input row shaded at {width}x{height}, ({x},{y})"
                    );
                }
            }
        }
    }
}

#[test]
fn composer_surface_fills_chat_width_and_transcript_keeps_a_blank_separator() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    app.upsert(
        selected.id.clone(),
        message(&selected, "m1", 1_700_000_000_000, "latest", false),
    );
    for (width, height) in [(100, 30), (44, 12)] {
        for theme in [Theme::Dark, Theme::Light] {
            app.theme = theme;
            let grid = buffer(&app, width, height);
            let mut composer_y = None;
            let mut transcript_y = None;
            for y in 0..height {
                let text = row_text(&grid, y);
                if text.contains("›") {
                    composer_y = Some(y);
                }
                if text.contains("latest") {
                    transcript_y = Some(y);
                }
            }
            let composer_y = composer_y.expect("composer row");
            let transcript_y = transcript_y.expect("transcript row");
            assert!(
                composer_y > transcript_y + 1,
                "separator row exists, width={width}, height={height}"
            );
            let bg = grid[(width - 3, composer_y)].bg;
            assert_ne!(
                bg,
                grid[(width - 3, transcript_y)].bg,
                "composer surface distinguishes theme {theme:?}"
            );
            assert_eq!(
                grid[(width - 1, composer_y)].bg,
                bg,
                "input reaches pane edge"
            );
            let footer_y = composer_y + 2;
            if footer_y < height {
                assert_ne!(
                    grid[(width - 3, footer_y)].bg,
                    bg,
                    "footer outside input surface"
                );
            }
            app.scroll = 1;
            let scrolled = buffer(&app, width, height);
            assert_eq!(
                scrolled[(width - 3, composer_y)].bg,
                bg,
                "scroll does not move composer surface"
            );
            app.scroll = 0;
        }
    }
}

#[test]
fn the_header_leads_with_the_chat_and_ends_with_bloom() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected]);
    let grid = buffer(&app, 100, 30);
    let header = row_text(&grid, 0);
    let pane = header.split('│').nth(1).expect("pane header");
    assert!(
        pane.trim_start().starts_with("Weekend crew"),
        "chat name leads: {pane:?}"
    );
    assert!(
        header.trim_end().ends_with("Bloom"),
        "Bloom sits at the right: {header:?}"
    );
    assert!(!header.contains("Bloom /"));
}

#[test]
fn the_command_popup_aligns_with_the_chat_not_the_center() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        draft: "/".into(),
        draft_cursor: 1,
        command_menu: true,
        ..Default::default()
    };
    app.set_conversations(vec![selected]);
    let grid = buffer(&app, 100, 30);
    let panel_row = (0..30)
        .find(|y| row_text(&grid, *y).contains("/help"))
        .expect("popup row");
    let panel_x = column_of(&grid, panel_row, "›").expect("selected marker");
    let prompt_row = (0..30)
        .find(|y| row_text(&grid, *y).contains("›  /"))
        .expect("composer prompt");
    let prompt_x = column_of(&grid, prompt_row, "›").expect("prompt glyph");
    assert_eq!(panel_x, prompt_x, "popup text retains chat padding");
    assert_eq!(
        grid[(99, panel_row)].bg,
        grid[(50, panel_row)].bg,
        "selection reaches pane edge"
    );
    let narrow = buffer(&app, 44, 12);
    assert!(rendered(&app).contains("/help"));
    assert!(
        row_text(&narrow, 9).contains("/help")
            || (0..12).any(|y| row_text(&narrow, y).contains("/help")),
        "menu survives narrow short layout"
    );
}

#[test]
fn older_page_keeps_every_visible_message_on_screen_when_the_thread_fits() {
    let only = conversation("Short thread", 2, 1);
    let mut app = App::default();
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..3 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("s{index:03}"),
                1_700_000_000_000 + index,
                &format!("short message {index:03}"),
                false,
            ),
        );
    }
    let (buffer, _) = tracked_frame(&mut app, 100, 24);
    assert_eq!(app.scroll, 0, "the whole thread fits on screen");
    assert!((0..24).any(|y| row_text(&buffer, y).contains("short message 000")));

    app.scroll_back(1);
    assert!(
        app.older_requested,
        "the fitting thread is already at its oldest row"
    );
    let (id, _, token) = app.begin_load_older().expect("a cursor");
    app.finish_load_older(
        id,
        token,
        // Enough to push the whole thread past the viewport once merged.
        (0..40)
            .map(|index| {
                message(
                    &only,
                    &format!("older{index:03}"),
                    1_699_999_999_000 + index,
                    &format!("older message {index:03}"),
                    false,
                )
            })
            .collect(),
        OLDER_PAGE_LIMIT,
    );

    let (buffer, _) = tracked_frame(&mut app, 100, 24);
    let screen = (0..24)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..3 {
        let marker = format!("short message {index:03}");
        assert!(
            (0..24).any(|y| row_text(&buffer, y).contains(&marker)),
            "every message that was on screen must stay on screen: {marker}\n{screen}"
        );
    }
}

#[test]
fn older_page_requests_fire_only_at_the_top_and_preserve_the_reading_anchor() {
    let only = conversation("Long thread", 2, 1);
    let mut app = App::default();
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..80 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index,
                &format!("message number {index:03}"),
                false,
            ),
        );
    }
    tracked_frame(&mut app, 100, 24);
    assert!(
        app.transcript_viewport
            .as_ref()
            .is_some_and(|view| view.max_scroll() > 0),
        "the fixture must be long enough to scroll"
    );

    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    tracked_frame(&mut app, 100, 24);
    assert!(
        !app.older_requested,
        "a mid-thread scroll must not request older history"
    );
    assert!(app.is_scrolled_back());

    for _ in 0..100 {
        if app.older_requested {
            break;
        }
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        tracked_frame(&mut app, 100, 24);
    }
    assert!(
        app.older_requested,
        "reaching the oldest loaded row must request older history"
    );

    // An overlay owns the transcript and must not page history underneath itself.
    tracked_frame(&mut app, 100, 24);
    app.older_requested = false;
    app.open_settings();
    app.scroll_back(10);
    assert!(
        !app.older_requested,
        "an overlay must keep the transcript from paging"
    );
    app.settings_open = false;

    tracked_frame(&mut app, 100, 24);
    assert!(
        !rendered(&app).contains("↑ older"),
        "reaching the top requests the page directly, so no separate affordance is drawn:\n{}",
        rendered(&app)
    );
    let anchor = app
        .reading_anchor
        .as_ref()
        .expect("a scrolled view records its reading anchor")
        .message
        .clone();

    let (id, _, token) = app.begin_load_older().expect("an older page cursor");
    app.finish_load_older(
        id,
        token,
        (0..10)
            .map(|index| {
                message(
                    &only,
                    &format!("old{index:03}"),
                    1_699_999_999_000 + index,
                    &format!("older message {index:03}"),
                    false,
                )
            })
            .collect(),
        OLDER_PAGE_LIMIT,
    );
    assert!(
        !app.older_requested,
        "starting and finishing a page clears the pending request"
    );

    tracked_frame(&mut app, 100, 24);
    assert_eq!(
        app.reading_anchor
            .as_ref()
            .map(|anchor| anchor.message.clone()),
        Some(anchor),
        "loading older history must keep the message being read at the top"
    );
}

fn tracked_frame(app: &mut App, width: u16, height: u16) -> (ratatui::buffer::Buffer, ui::HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, app))
        .unwrap();
    app.record_transcript(hits.transcript_view.clone());
    (terminal.backend().buffer().clone(), hits)
}

fn reading_fixture() -> (App, Conversation) {
    let only = conversation("Reading", 2, 1);
    let mut app = App::default();
    app.set_conversations(vec![only.clone()]);
    for index in 0..60 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("m{index:03}"),
                1_700_000_000_000 + index,
                &format!("reading message {index:03}"),
                false,
            ),
        );
    }
    tracked_frame(&mut app, 100, 24);
    app.scroll_back(10);
    tracked_frame(&mut app, 100, 24);
    (app, only)
}

#[test]
fn reading_anchor_survives_appends_edits_duplicates_snapshots_and_reflow() {
    use bloom_ipc::EventKind;
    let (mut app, only) = reading_fixture();
    let anchor = app.reading_anchor.clone().unwrap();
    let original_back = app.scroll;
    let arrival = message(
        &only,
        "arrival",
        1_700_000_001_000,
        &"new wrapped content ".repeat(50),
        false,
    );
    for _ in 0..2 {
        app.apply_event(EventKind::Message {
            conversation_id: only.id.clone(),
            message: arrival.clone(),
            push_sequence: None,
        });
        tracked_frame(&mut app, 100, 24);
        assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
        assert_eq!(app.reading_anchor.as_ref().unwrap().row, anchor.row);
    }
    assert!(
        app.scroll > original_back,
        "bottom-relative distance must grow, not the reading position"
    );
    app.apply_event(EventKind::Update {
        conversation_id: only.id.clone(),
        message: message(
            &only,
            "m000",
            1_700_000_000_000,
            &"edited earlier text ".repeat(50),
            false,
        ),
    });
    tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    app.apply_snapshot(bloom_tui::daemon::Snapshot {
        token: None,
        revision: 10,
        conversations: vec![only.clone()],
        visible: Some(only.id.clone()),
        history: app.messages[&only.id].clone(),
    });
    tracked_frame(&mut app, 66, 18);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_eq!(app.reading_anchor.as_ref().unwrap().row, anchor.row);
    tracked_frame(&mut app, 20, 3); // Tiny fallback must not discard the anchor.
    tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    app.scroll_to_newest();
    tracked_frame(&mut app, 100, 24);
    assert!(!app.is_scrolled_back());
    app.apply_event(EventKind::Message {
        conversation_id: only.id.clone(),
        message: message(
            &only,
            "following",
            1_700_000_002_000,
            "follow this new arrival",
            false,
        ),
        push_sequence: None,
    });
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert!((0..24).any(|y| row_text(&buffer, y).contains("follow this new arrival")));
    assert!(hits.latest.is_none());
}

#[test]
fn latest_has_a_measured_target_keeps_errors_and_preserves_literal_composition() {
    let (mut app, _) = reading_fixture();
    app.draft = "/new literal draft".into();
    app.pasted_body = true;
    app.draft_cursor = 4;
    app.error = Some("A visible send error".into());
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    let latest = hits
        .latest
        .expect("visible return control even with an error");
    assert_eq!(hits.at(latest.x, latest.y), Some(ui::Hit::Latest));
    assert!((0..24).any(|y| row_text(&buffer, y).contains("A visible send error")));
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        latest.x,
        latest.y,
        &hits,
    );
    assert!(!app.is_scrolled_back());
    assert_eq!(app.draft, "/new literal draft");
    assert_eq!(app.draft_cursor, 4);
    assert!(app.pasted_body);
}

#[test]
fn painted_overlays_own_wheel_and_uncovered_transcript_still_scrolls() {
    for panel in 0..5 {
        let (mut app, _) = reading_fixture();
        app.scroll_to_newest();
        match panel {
            0 => app.open_settings(),
            1 => app.theme_panel_open = true,
            2 => {
                app.draft = "/".into();
                app.command_menu = true;
            }
            3 => app.begin_contacts(),
            _ => {
                let id = app.selected.clone().unwrap();
                app.open_context_menu(id);
            }
        }
        let (_, hits) = tracked_frame(&mut app, 100, 30);
        let area = hits
            .menu
            .or(hits.context_menu)
            .or(hits.popup)
            .expect("painted panel");
        assert!(hits.latest.is_none());
        app.handle_mouse(MouseEventKind::ScrollUp, area.x + 1, area.y + 1, &hits);
        assert_eq!(app.scroll, 0, "panel {panel} must consume its own wheel");
        let point = (hits.transcript.x, hits.transcript.y);
        if !hits.overlay_contains(point.0, point.1) {
            app.handle_mouse(MouseEventKind::ScrollUp, point.0, point.1, &hits);
            assert!(
                app.is_scrolled_back(),
                "uncovered transcript remains scrollable"
            );
        }
    }
}

#[test]
fn alias_reconciliation_is_the_same_reading_context_even_before_history_arrives() {
    let (mut app, original) = reading_fixture();
    let anchor = app.reading_anchor.clone().unwrap();
    let mut canonical = conversation("Canonical Reading", 2, 1);
    canonical.aliases.push(original.id.clone());
    app.set_conversations(vec![canonical.clone()]);
    tracked_frame(&mut app, 100, 24); // The new identity has no history response yet.
    assert!(
        app.is_scrolled_back(),
        "a metadata alias must not reset the reading context"
    );
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    let history = app.messages[&original.id]
        .iter()
        .map(|old| {
            message(
                &canonical,
                old.id().value(),
                old.created_at,
                &format!(
                    "reading message {}",
                    old.id().value().trim_start_matches('m')
                ),
                false,
            )
        })
        .collect();
    app.apply_snapshot(bloom_tui::daemon::Snapshot {
        token: None,
        revision: 20,
        visible: Some(canonical.id.clone()),
        conversations: vec![canonical.clone()],
        history,
    });
    tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_eq!(
        app.reading_anchor.as_ref().unwrap().conversation,
        canonical.id
    );
    assert_eq!(app.reading_anchor.as_ref().unwrap().row, anchor.row);
}

#[test]
fn narrow_drawer_owns_input_without_retargeting_until_explicit_open() {
    let (mut app, alice) = reading_fixture();
    let bob = conversation("Bob with a useful full identity", 2, 0);
    app.set_conversations(vec![alice.clone(), bob.clone()]);
    app.draft = "/new literal private Alice body".into();
    app.pasted_body = true;
    app.draft_cursor = 7;
    let anchor = app.reading_anchor.clone().unwrap();
    app.set_terminal_width(44);
    let (_, narrow) = tracked_frame(&mut app, 44, 12);
    assert_eq!(
        narrow.composer.width as usize,
        ui::composer_columns(44, false)
    );
    assert!(narrow.sidebar_rows.is_empty());
    let chats = narrow.header_chats.expect("visible Chats navigation");
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        chats.x,
        chats.y,
        &narrow,
    );
    app.paste("Bob");
    let (buffer, hits) = tracked_frame(&mut app, 44, 12);
    assert!((0..12).any(|y| row_text(&buffer, y).contains("Bob with a useful full identity")));
    assert_eq!(
        app.selected.as_ref(),
        Some(&alice.id),
        "highlight/search must not select a recipient"
    );
    assert_eq!(app.draft, "/new literal private Alice body");
    assert_eq!(app.draft_cursor, 7);
    assert!(app.pasted_body);
    assert_eq!(hits.at(chats.x, chats.y), Some(ui::Hit::Drawer));
    assert!(hits.latest.is_none());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_eq!(app.selected.as_ref(), Some(&alice.id));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.paste("Bob");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.selected.as_ref(), Some(&bob.id));
    assert!(
        app.draft.is_empty(),
        "Alice's literal body must not appear in Bob"
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.paste("Reading");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.selected.as_ref(), Some(&alice.id));
    assert_eq!(app.draft, "/new literal private Alice body");
    assert_eq!(app.draft_cursor, 7);
    assert!(app.pasted_body);
}

#[test]
fn stale_drawer_click_cannot_open_a_different_current_chat() {
    let (mut app, alice) = reading_fixture();
    let bob = conversation("Bob", 2, 0);
    let charlie = conversation("Charlie", 2, -1);
    app.set_conversations(vec![alice.clone(), bob.clone(), charlie.clone()]);
    app.set_terminal_width(44);
    app.open_chat_drawer();
    let (_, hits) = tracked_frame(&mut app, 44, 12);
    let bob_row = hits
        .drawer_rows
        .iter()
        .find(|(_, id)| id == &bob.id)
        .unwrap()
        .0;

    let mut promoted_charlie = charlie;
    promoted_charlie.last_message_at = Some(100);
    app.set_conversations(vec![alice.clone(), promoted_charlie]);
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        bob_row.x,
        bob_row.y,
        &hits,
    );

    assert_eq!(app.selected, Some(alice.id));
    assert!(
        app.chat_drawer.is_some(),
        "stale hit must leave navigation open"
    );
}

#[test]
fn drawer_search_trims_paste_and_matches_participant_display_names() {
    let (mut app, alice) = reading_fixture();
    let mut project = conversation("Project room", 3, 0);
    project.participants[1].display_name = Some("Robert Jones".into());
    project.participants[1].handle =
        Handle::new(HandleKind::Email, "robert@example.invalid").unwrap();
    app.set_conversations(vec![alice, project.clone()]);
    app.set_terminal_width(44);
    app.open_chat_drawer();

    app.paste("robert@example.invalid\n");
    assert_eq!(app.drawer_selection(), Some(project.id.clone()));
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.paste("Robert Jones\n");
    assert_eq!(app.drawer_selection(), Some(project.id.clone()));
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.titles.insert(
        bloom_tui::app::conversation_key(&project.id),
        "Zebra hideout".into(),
    );
    app.paste("Zebra hideout\n");
    assert_eq!(app.drawer_selection(), Some(project.id));
}

#[test]
fn opening_and_closing_drawer_clears_pending_leader_input() {
    let (mut app, _) = reading_fixture();
    app.set_terminal_width(44);
    let (_, hits) = tracked_frame(&mut app, 44, 12);
    let chats = hits.header_chats.unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(app.leader_pending);
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        chats.x,
        chats.y,
        &hits,
    );
    assert!(app.chat_drawer.is_some());
    assert!(!app.leader_pending);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(app.draft, "x");
}

#[test]
fn drawer_list_scrolls_to_a_deep_candidate_and_renders_at_tiny_sizes() {
    let mut app = App::default();
    app.set_conversations(
        (0..60)
            .map(|index| conversation(&format!("Chat {index:02}"), 2, index))
            .collect(),
    );
    app.set_terminal_width(44);
    app.open_chat_drawer();
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    let chosen = app.drawer_selection().unwrap();
    for (width, height) in [(66, 18), (44, 12), (24, 6), (20, 4), (8, 2), (1, 1)] {
        let (_, hits) = tracked_frame(&mut app, width, height);
        assert!(hits.drawer.is_some());
        if height >= 6 {
            assert!(
                hits.drawer_rows.iter().any(|(_, id)| id == &chosen),
                "deep candidate must stay visible at {width}x{height}"
            );
        }
        assert!(
            hits.composer_positions.is_empty(),
            "drawer must not leave composer cursor targets behind"
        );
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        app.chat_drawer.is_none(),
        "Ctrl+C cancels navigation instead of clearing or sending the draft"
    );
}

#[test]
fn drawer_cancel_preserves_suspended_new_chat_and_empty_search_cannot_open() {
    let (mut app, _) = reading_fixture();
    app.begin_new_conversation();
    app.paste("carol@example.invalid");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.paste("/new literal Carol body");
    app.open_settings();
    app.set_terminal_width(44);
    app.open_chat_drawer();
    app.paste("no matching conversation");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.chat_drawer.is_some(),
        "empty search must remain correctable"
    );
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.settings_open,
        "cancel must return to the exact underlying panel"
    );
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.pending_address.as_deref(),
        Some("carol@example.invalid")
    );
    assert_eq!(app.draft, "/new literal Carol body");
    assert!(app.pasted_body);
    app.open_chat_drawer();
    app.paste("Reading");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.new_conversation);
    assert!(
        app.draft.is_empty(),
        "new-chat body cannot leak into an existing chat"
    );
    app.begin_new_conversation();
    app.paste("carol@example.invalid");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.draft, "/new literal Carol body",
        "navigation parks rather than discards the new-chat body"
    );
    assert!(app.pasted_body);
}

#[test]
fn drawer_candidate_survives_background_identity_alias_reconciliation() {
    let (mut app, alice) = reading_fixture();
    let bob = conversation("Bob original identity", 2, 0);
    app.set_conversations(vec![alice.clone(), bob.clone()]);
    app.set_terminal_width(44);
    app.open_chat_drawer();
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    tracked_frame(&mut app, 44, 12);
    assert_eq!(app.drawer_selection(), Some(bob.id.clone()));
    let mut canonical = conversation("Bob canonical identity", 2, 0);
    canonical.aliases.push(bob.id);
    app.set_conversations(vec![alice.clone(), canonical.clone()]);
    let (buffer, _) = tracked_frame(&mut app, 44, 12);
    assert!(
        (0..12).any(|y| row_text(&buffer, y).contains("› Bob canonical identity")),
        "background identity metadata must not change the highlighted logical chat"
    );
    let mut final_identity = conversation("Bob final identity", 2, 0);
    final_identity.aliases.push(canonical.id);
    app.set_conversations(vec![alice, final_identity.clone()]);
    let (buffer, _) = tracked_frame(&mut app, 44, 12);
    assert!(
        (0..12).any(|y| row_text(&buffer, y).contains("› Bob final identity")),
        "successive alias transitions must retain the same logical candidate"
    );
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.selected, Some(final_identity.id));
}

#[test]
fn delivery_icon_and_label_wrap_as_one_visual_unit() {
    let (mut app, selected) = reading_fixture();
    let mut sent = message(
        &selected,
        "delivered-wrap",
        1_800_000_000_000,
        "Yes, morning works best for me.",
        true,
    );
    sent.delivery = DeliveryState::Delivered;
    app.upsert(selected.id, sent);
    app.scroll_to_newest();
    for width in [24, 44, 66] {
        let grid = buffer(&app, width, 30);
        let delivered = (0..30)
            .map(|y| row_text(&grid, y))
            .find(|row| row.contains("delivered"))
            .expect("delivery status must remain visible");
        assert!(
            delivered.contains('✓'),
            "delivery icon must not be stranded on another row at {width}: {delivered:?}"
        );
    }
}

#[test]
fn consecutive_messages_from_one_sender_share_a_byline_under_a_day_divider() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    for (id, at, text, mine) in [
        ("m1", 1_700_000_000_000, "first from Maya", false),
        ("m2", 1_700_000_060_000, "second from Maya", false),
        ("m3", 1_700_000_120_000, "and a reply", true),
    ] {
        app.upsert(selected.id.clone(), message(&selected, id, at, text, mine));
    }
    let text = rendered(&app);
    let maya_bylines = text
        .lines()
        .filter(|line| {
            line.split('│')
                .nth(1)
                .is_some_and(|pane| pane.trim_start().starts_with("Maya"))
        })
        .count();
    assert_eq!(
        maya_bylines, 1,
        "a run from one sender has one byline:\n{text}"
    );
    assert!(
        text.contains("You"),
        "a new sender starts a byline:\n{text}"
    );
    let (_, _, day) = bloom_tui::app::day_label(1_700_000_000_000);
    assert!(
        text.contains(&day),
        "the day divider names the day:\n{text}"
    );
}
fn search_hit(conversation: &Conversation, id: &str, snippet: &str) -> MessageSearchHit {
    MessageSearchHit {
        conversation_id: conversation.id.clone(),
        message_id: MessageId::new(transport(), id).unwrap(),
        created_at: 1_700_000_000_000,
        snippet: snippet.to_owned(),
    }
}

/// Opens the search overlay, types a query and returns the request id the daemon
/// reply must carry.
fn start_search(app: &mut App, query: &str) -> u64 {
    app.open_search();
    for character in query.chars() {
        app.search_input(character);
    }
    app.begin_search().expect("search starts").0
}

fn start_context_lookup(
    app: &mut App,
    conversation: &Conversation,
    target: &str,
) -> (
    u64,
    ConversationId,
    MessageId,
    bloom_tui::daemon::ActivityToken,
) {
    let request = start_search(app, "needle");
    app.finish_search(request, vec![search_hit(conversation, target, "needle")]);
    app.begin_search_jump().expect("search jump");
    app.advance_search_jump();
    app.begin_search_context_request().expect("context request")
}

fn participant_render_app(raw_handle: &str) -> (App, ParticipantFilter, ParticipantFilter) {
    let mut first = conversation("First chat", 2, 40);
    first.id = ConversationId::new(TransportId::new("alpha").unwrap(), "first").unwrap();
    first.participants[1].handle = Handle::new(HandleKind::Email, raw_handle).unwrap();
    first.participants[1].display_name = Some("Duplicate".into());
    let mut second = first.clone();
    second.id = ConversationId::new(TransportId::new("beta").unwrap(), "second").unwrap();
    second.title = Some("Second chat".into());
    let filters: Vec<_> = [&first, &second]
        .into_iter()
        .map(|chat| ParticipantFilter {
            transport: chat.id.transport().clone(),
            handle: chat.participants[1].handle.clone(),
        })
        .collect();
    let mut app = App::default();
    app.set_conversations(vec![first.clone(), second]);
    app.select(Some(first.id.clone()));
    app.draft = "/new\nparked draft".into();
    app.draft_cursor = 3;
    app.pasted_body = true;
    let request = start_search(&mut app, "needle");
    app.finish_search(
        request,
        vec![search_hit(&first, "old", "OLD_SCOPE_SNIPPET")],
    );
    app.open_search_participant_picker();
    (app, filters[0].clone(), filters[1].clone())
}

#[test]
fn participant_picker_paints_identities_not_old_result_rows() {
    let (mut app, first, second) = participant_render_app("p@example.invalid");
    let (grid, hits) = tracked_frame(&mut app, 100, 30);
    let text: String = (0..grid.area.height).map(|y| row_text(&grid, y)).collect();
    for expected in [
        "Search: needle",
        "Chats with",
        "Find:",
        "All chats",
        "alpha",
        "beta",
        "p@example.invalid",
        "Duplicate",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("OLD_SCOPE_SNIPPET"));
    assert!(hits.search_rows.is_empty());
    assert_eq!(hits.search_participant_rows.len(), 3);
    for (area, filter) in &hits.search_participant_rows {
        assert_eq!(
            hits.at(area.x + 1, area.y),
            Some(ui::Hit::SearchParticipant(filter.clone()))
        );
    }
    assert!(hits
        .search_participant_rows
        .iter()
        .any(|(_, filter)| filter.as_ref() == Some(&first)));
    assert!(hits
        .search_participant_rows
        .iter()
        .any(|(_, filter)| filter.as_ref() == Some(&second)));
    app.close_search_participant_picker();
    assert!(rendered(&app).contains("All chats · Ctrl+P"));
    assert!(app.commit_search_participant(Some(first)));
    let text = rendered(&app);
    assert!(text.contains("Chats with"));
    assert!(text.contains("alpha"));
    assert!(text.contains("Search: needle"));
}

#[test]
fn participant_picker_narrow_rows_keep_transport_and_hitboxes_stay_inside_frame() {
    let (mut app, first, second) =
        participant_render_app("a-very-long-shared-participant-address@example.invalid");
    for (index, wanted) in [(1, first), (2, second)] {
        app.select_search_participant(index);
        let (grid, hits) = tracked_frame(&mut app, 44, 12);
        let text: String = (0..grid.area.height).map(|y| row_text(&grid, y)).collect();
        assert!(text.contains("Find:"), "{text}");
        assert!(
            text.contains(wanted.transport.as_str()),
            "the selected transport must remain visible beside a long handle: {text}"
        );
        assert!(
            hits.search_participant_rows
                .iter()
                .any(|(_, filter)| filter.as_ref() == Some(&wanted)),
            "compact mode must keep the selected identity selectable"
        );
    }
    for (width, height) in [(44, 12), (30, 8), (8, 4), (1, 1)] {
        let (_, hits) = tracked_frame(&mut app, width, height);
        for (area, filter) in &hits.search_participant_rows {
            assert!(area.width > 0 && area.height > 0);
            assert!(area.right() <= width && area.bottom() <= height);
            assert_eq!(
                hits.search_participant_at(area.x, area.y),
                Some(filter.clone())
            );
        }
        assert!(hits.search_rows.is_empty());
    }
}

#[test]
fn participant_picker_wraps_shared_prefix_handles_without_losing_distinguishing_text() {
    for values in [
        [
            "long.name.one@example.invalid",
            "long.name.two@example.invalid",
        ],
        [
            "identical.long.name@one.example.invalid",
            "identical.long.name@two.example.invalid",
        ],
    ] {
        let (mut app, _, _) = participant_render_app(values[0]);
        let mut chats = app.conversations.clone();
        chats[1].id = ConversationId::new(TransportId::new("alpha").unwrap(), "second").unwrap();
        chats[1].participants[1].handle = Handle::new(HandleKind::Email, values[1]).unwrap();
        app.set_conversations(chats);
        let candidates = app.search_participant_candidates();
        for (index, candidate) in candidates.iter().enumerate().skip(1) {
            let wanted = candidate.filter().unwrap();
            for (width, height) in [(24, 12), (44, 12), (100, 30)] {
                app.select_search_participant(index);
                let (grid, hits) = tracked_frame(&mut app, width, height);
                let row = hits
                    .search_participant_rows
                    .iter()
                    .find(|(_, filter)| filter.as_ref() == Some(wanted))
                    .expect("the selected identity must be painted")
                    .0;
                let identity: String = (row.y..row.bottom())
                    .flat_map(|y| row_text(&grid, y).chars().collect::<Vec<_>>())
                    .filter(|character| !character.is_whitespace())
                    .collect();
                assert!(
                    identity.contains(wanted.handle.value()),
                    "full distinguishing handle must be inspectable at {width}: {identity}"
                );
                assert!(identity.contains(wanted.transport.as_str()));
                assert_eq!(
                    hits.at(row.x + 1, row.bottom() - 1),
                    Some(ui::Hit::SearchParticipant(Some(wanted.clone())))
                );
                if width < 70 {
                    assert_eq!(
                        hits.sidebar.width, 0,
                        "the narrow picker owns the whole frame"
                    );
                    assert_eq!(hits.composer.width, 0);
                    assert!(hits.transcript_view.is_none());
                }
            }
        }
    }
}

#[test]
fn participant_picker_mouse_commits_the_painted_identity_after_reordering() {
    let (mut app, first, second) = participant_render_app("p@example.invalid");
    let recipient = app.selected.clone();
    let (_, hits) = tracked_frame(&mut app, 100, 30);
    let second_row = hits
        .search_participant_rows
        .iter()
        .find(|(_, filter)| filter.as_ref() == Some(&second))
        .unwrap()
        .0;
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        second_row.x + 1,
        second_row.y,
        &hits,
    );
    assert_eq!(app.search_participant_scope(), Some(&second));
    assert_eq!(app.selected, recipient);
    assert_eq!(app.search_query(), Some("needle"));
    app.open_search_participant_picker();
    let (_, hits) = tracked_frame(&mut app, 100, 30);
    let all_row = hits
        .search_participant_rows
        .iter()
        .find(|(_, filter)| filter.is_none())
        .unwrap()
        .0;
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        all_row.x + 1,
        all_row.y,
        &hits,
    );
    assert_eq!(app.search_participant_scope(), None);
    assert!(!app.search_participant_picker_open());
    app.open_search_participant_picker();
    let (_, hits) = tracked_frame(&mut app, 100, 30);
    let first_row = hits
        .search_participant_rows
        .iter()
        .find(|(_, filter)| filter.as_ref() == Some(&first))
        .unwrap()
        .0;
    let mut chats = app.conversations.clone();
    for chat in &mut chats {
        chat.participants[1].display_name = Some(
            if chat.id.transport() == &first.transport {
                "Zed"
            } else {
                "Aaa"
            }
            .into(),
        );
    }
    app.set_conversations(chats);
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        first_row.x + 1,
        first_row.y,
        &hits,
    );
    assert_eq!(
        app.search_participant_scope(),
        Some(&first),
        "clicks use the painted identity, never a later row index"
    );
    app.close_search();
    assert_eq!(app.selected, recipient);
    assert_eq!(app.draft, "/new\nparked draft");
    assert_eq!(app.draft_cursor, 3);
    assert!(app.pasted_body);
}

#[test]
fn participant_picker_hidden_highlight_and_removed_painted_rows_do_not_commit() {
    let (mut app, first, second) = participant_render_app("p@example.invalid");
    assert!(app.commit_search_participant(Some(second.clone())));
    app.open_search_participant_picker();
    app.search_paste("Duplicate");
    app.step_search_participant(1);
    let mut chats = app.conversations.clone();
    let first_chat = chats
        .iter_mut()
        .find(|chat| chat.id.transport() == &first.transport)
        .unwrap();
    first_chat.participants[1].display_name = Some("Renamed".into());
    app.set_conversations(chats);
    assert!(app.selected_search_participant_candidate().is_none());
    let (grid, hits) = tracked_frame(&mut app, 100, 30);
    for (area, _) in &hits.search_participant_rows {
        assert!(
            !row_text(&grid, area.y).contains("› "),
            "no false All chats highlight"
        );
    }
    app.search_clear();
    let (_, stale_hits) = tracked_frame(&mut app, 100, 30);
    let first_row = stale_hits
        .search_participant_rows
        .iter()
        .find(|(_, filter)| filter.as_ref() == Some(&first))
        .unwrap()
        .0;
    let chats = app
        .conversations
        .iter()
        .filter(|chat| chat.id.transport() != &first.transport)
        .cloned()
        .collect();
    app.set_conversations(chats);
    let recipient = app.selected.clone();
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        first_row.x + 1,
        first_row.y,
        &stale_hits,
    );
    assert_eq!(app.search_participant_scope(), Some(&second));
    assert!(app.search_participant_picker_open());
    assert_eq!(app.selected, recipient);
}

#[test]
fn participant_picker_wheel_owns_only_its_painted_popup() {
    let (mut app, _, _) = participant_render_app("p@example.invalid");
    let (_, hits) = tracked_frame(&mut app, 100, 30);
    let selected = app.selected_search_participant_candidate();
    app.handle_mouse(MouseEventKind::ScrollDown, 0, 0, &hits);
    assert_eq!(app.selected_search_participant_candidate(), selected);
    let row = hits.search_participant_rows[0].0;
    app.handle_mouse(MouseEventKind::ScrollDown, row.x + 1, row.y, &hits);
    assert!(!app
        .selected_search_participant_candidate()
        .unwrap()
        .is_all_chats());
}

#[test]
fn search_overlay_renders_the_query_snippets_and_other_chat_names() {
    let selected = conversation("Weekend crew", 3, 40);
    let other = conversation("Jules Lee", 2, 20);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone(), other.clone()]);
    let request = start_search(&mut app, "needle");
    app.finish_search(
        request,
        vec![search_hit(&other, "hit", "found a needle in this chat")],
    );

    let text = rendered(&app);
    assert!(text.contains("Search: needle"), "{text}");
    assert!(text.contains("Jules Lee"), "{text}");
    assert!(text.contains("found a needle"), "{text}");
    assert!(!text.contains("No messages found."), "{text}");
}

#[test]
fn search_overlay_truncates_long_snippets_to_the_popup_width() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "needle");
    let tail = "UNRENDERED_TAIL";
    app.finish_search(
        request,
        vec![search_hit(
            &selected,
            "hit",
            &format!("{}{tail}", "wide snippet ".repeat(40)),
        )],
    );

    let (grid, hits) = tracked_frame(&mut app, 66, 18);
    let popup = hits.popup.expect("search popup");
    let (row, _) = hits.search_rows.first().copied().expect("search hit row");
    assert_eq!(row.x, popup.x);
    assert_eq!(row.width, popup.width);
    let line = row_text(&grid, row.y);
    assert!(
        !line.contains(tail),
        "snippet must stop at the popup: {line:?}"
    );
}

#[test]
fn search_overlay_renders_empty_and_error_states() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "missing");
    app.finish_search(request, Vec::new());
    let empty = rendered(&app);
    assert!(empty.contains("No messages found."), "{empty}");

    app.fail_search(request, "Message search failed.".into());
    let failed = rendered(&app);
    assert!(failed.contains("Message search failed."), "{failed}");
}

#[test]
fn search_overlay_shows_the_number_of_completed_hits() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "needle");
    app.finish_search(
        request,
        vec![
            search_hit(&selected, "one", "first needle"),
            search_hit(&selected, "two", "second needle"),
        ],
    );

    let text = rendered(&app);
    assert!(text.contains("2 results"), "{text}");
    assert!(!text.contains("No messages found."), "{text}");
}

#[test]
fn search_overlay_marks_a_hit_list_at_the_result_limit() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "needle");
    let hits = (0..SEARCH_RESULT_LIMIT)
        .map(|index| search_hit(&selected, &format!("hit-{index}"), "needle"))
        .collect();
    app.finish_search(request, hits);

    let text = rendered(&app);
    assert!(
        text.contains(&format!("{SEARCH_RESULT_LIMIT} results (may be more)")),
        "{text}"
    );
}

#[test]
fn search_overlay_never_shows_a_result_count_while_loading_or_after_an_error() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "needle");
    let loading = rendered(&app);
    assert!(loading.contains("Searching…"), "{loading}");
    assert!(!loading.contains(" results"), "{loading}");

    app.fail_search(request, "Message search failed.".into());
    let failed = rendered(&app);
    assert!(failed.contains("Message search failed."), "{failed}");
    assert!(!failed.contains(" results"), "{failed}");
}

#[test]
fn search_snippets_cannot_emit_terminal_escape_sequences() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone()]);
    let request = start_search(&mut app, "needle");
    app.finish_search(
        request,
        vec![search_hit(
            &selected,
            "hit",
            "before \u{1b}[31mneedle\u{1b}[0m after",
        )],
    );

    let (grid, _) = tracked_frame(&mut app, 100, 24);
    let painted: String = (0..24).map(|y| row_text(&grid, y)).collect();
    assert!(
        !painted.contains('\u{1b}'),
        "stored text must never reach the frame as an escape sequence"
    );
    assert!(painted.contains("needle"), "{painted}");

    // Formatting and bidi controls are stripped, and tabs keep words apart.
    let request = start_search(&mut app, "tabbed");
    app.finish_search(
        request,
        vec![search_hit(&selected, "tab", "one\ttwo\u{202e}three")],
    );
    let (grid, _) = tracked_frame(&mut app, 100, 24);
    let painted: String = (0..24).map(|y| row_text(&grid, y)).collect();
    assert!(painted.contains("one two"), "{painted}");
    assert!(!painted.contains('\u{202e}'), "{painted}");
}

#[test]
fn clicking_a_search_result_opens_it_and_other_clicks_do_nothing() {
    let selected = conversation("Weekend crew", 3, 40);
    let other = conversation("Jules Lee", 2, 20);
    let mut app = App {
        selected: Some(selected.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![selected.clone(), other.clone()]);
    let request = start_search(&mut app, "needle");
    app.finish_search(request, vec![search_hit(&other, "hit", "found a needle")]);

    let (_, hits) = tracked_frame(&mut app, 100, 24);
    let (row, _) = hits.search_rows.first().copied().expect("search row");
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        row.x + 1,
        row.y,
        &hits,
    );
    assert_eq!(
        app.selected,
        Some(other.id.clone()),
        "a click opens the hit's conversation"
    );
    assert!(app.search.is_none(), "opening a hit closes the overlay");

    // A click outside the popup must not reach the transcript or sidebar.
    app.open_search();
    let (_, hits) = tracked_frame(&mut app, 100, 24);
    let before = app.selected.clone();
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3, &hits);
    assert_eq!(app.selected, before);
    assert!(app.search.is_some(), "the overlay stays open");
}

#[test]
fn search_overlay_paints_over_the_transcript_and_owns_the_wheel() {
    let (mut app, selected) = reading_fixture();
    let mut hits = vec![
        search_hit(&selected, "search-0", "first needle"),
        search_hit(&selected, "search-1", "second needle"),
    ];
    hits.push(search_hit(&selected, "search-2", "third needle"));
    let request = start_search(&mut app, "needle");
    app.finish_search(request, hits);
    app.scroll_to_newest();

    let (grid, frame_hits) = tracked_frame(&mut app, 100, 24);
    let popup = frame_hits.popup.expect("search popup");
    assert!(frame_hits.latest.is_none());
    assert!(popup.y > frame_hits.transcript.y);
    let covered =
        (frame_hits.transcript.y..popup.y).any(|y| row_text(&grid, y).contains("reading message"));
    assert!(covered, "the transcript remains painted behind the popup");
    let before = app.scroll;
    app.handle_mouse(
        MouseEventKind::ScrollDown,
        popup.x + 1,
        popup.y + popup.height.saturating_sub(1),
        &frame_hits,
    );
    assert_eq!(app.scroll, before, "search must consume its own wheel");
    assert_eq!(
        app.search.as_ref().expect("search state").selected,
        1,
        "the wheel moves the hit selection"
    );
}

#[test]
fn search_context_paints_its_rows_not_latest_and_labels_loading_and_exit() {
    let chat = conversation("Context render", 2, 1);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(
        chat.id.clone(),
        message(&chat, "normal-newest", 90, "NORMAL_LATEST_ROW", false),
    );
    let normal_history = app.messages[&chat.id].clone();
    let (generation, conversation, target, token) = start_context_lookup(&mut app, &chat, "target");

    let loading = rendered(&app);
    assert!(loading.contains("Search context"), "{loading}");
    assert!(loading.contains("loading exact message"), "{loading}");
    assert!(!loading.contains("NORMAL_LATEST_ROW"), "{loading}");

    let rows = vec![
        message(&chat, "before", 10, "CONTEXT_BEFORE_ROW", false),
        message(&chat, "target", 20, "CONTEXT_TARGET_ROW", false),
        message(&chat, "after", 30, "CONTEXT_AFTER_ROW", false),
    ];
    assert!(app.finish_message_context(
        generation,
        conversation.clone(),
        target,
        token,
        rows.clone()
    ));
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    let painted: String = (0..24).map(|y| row_text(&buffer, y)).collect();
    assert!(painted.contains("CONTEXT_TARGET_ROW"), "{painted}");
    assert!(!painted.contains("NORMAL_LATEST_ROW"), "{painted}");
    assert!(painted.contains("Search context"), "{painted}");
    assert!(
        painted.contains("End/Latest returns to newest"),
        "{painted}"
    );
    let view = hits.transcript_view.as_ref().expect("context viewport");
    assert_eq!(view.conversation, chat.id);
    assert_eq!(
        view.messages
            .iter()
            .map(|row| row.id.value())
            .collect::<Vec<_>>(),
        vec!["before", "target", "after"]
    );
    assert_eq!(app.messages[&chat.id], normal_history);

    app.open_search();
    let (_, overlay_hits) = tracked_frame(&mut app, 100, 24);
    assert!(
        overlay_hits.latest.is_none(),
        "overlay must cover the context Latest control"
    );
    app.close_search();
    assert!(
        app.search_context_active(),
        "cancelling an overlay keeps ready context"
    );
}

#[test]
fn short_search_context_still_has_a_measured_latest_control() {
    let chat = conversation("Short context", 2, 1);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(
        chat.id.clone(),
        message(&chat, "normal-newest", 90, "normal newest", false),
    );
    let (generation, conversation, target, token) = start_context_lookup(&mut app, &chat, "target");
    assert!(app.finish_message_context(
        generation,
        conversation,
        target,
        token,
        vec![message(&chat, "target", 20, "short target", false)]
    ));

    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_eq!(app.scroll, 0, "short context clamps to scroll zero");
    assert!(app.is_scrolled_back());
    let latest = hits.latest.expect("short context Latest hitbox");
    assert_eq!(hits.at(latest.x, latest.y), Some(ui::Hit::Latest));
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        latest.x,
        latest.y,
        &hits,
    );
    assert!(!app.is_scrolled_back());
    assert!(!app.search_context_active());
    let painted: String = (0..24).map(|y| row_text(&buffer, y)).collect();
    assert!(painted.contains("short target"));
}

#[test]
fn sidebar_unread_badges_show_counts_and_skip_read_chats() {
    let weekend = conversation("Weekend crew", 2, 40);
    let jules = conversation("Jules Lee", 2, 20);
    let mut app = App::default();
    app.set_conversations(vec![weekend.clone(), jules.clone()]);
    app.selected = Some(weekend.id.clone());

    let weekend_rows = vec![
        message(&weekend, "w1", 10, "older weekend", false),
        message(&weekend, "w2", 20, "newest weekend", false),
    ];
    let jules_rows = vec![
        message(&jules, "j1", 10, "older jules", false),
        message(&jules, "j2", 20, "middle jules", false),
        message(&jules, "j3", 30, "newest jules", false),
    ];
    app.messages
        .insert(weekend.id.clone(), weekend_rows.clone());
    app.messages.insert(jules.id.clone(), jules_rows.clone());
    app.read_positions
        .insert(weekend.id.clone(), (20, weekend_rows[1].id().clone()));
    app.read_positions
        .insert(jules.id.clone(), (10, jules_rows[0].id().clone()));

    let grid = buffer(&app, 100, 30);
    let sidebar_row = |title: &str| {
        (0..grid.area.height)
            .map(|y| {
                row_text(&grid, y)
                    .split('│')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .find(|sidebar| sidebar.contains(title))
            .expect("sidebar conversation row")
    };
    let weekend_row = sidebar_row("Weekend crew");
    let jules_row = sidebar_row("Jules Lee");
    assert!(weekend_row.contains("Weekend crew"));
    assert!(
        !weekend_row.contains("·"),
        "a fully read chat has no badge: {weekend_row:?}"
    );
    assert!(
        jules_row.contains("Jules Lee · 2"),
        "unread count follows the chat name: {jules_row:?}"
    );
}

#[test]
fn sidebar_renders_the_capped_unread_form() {
    let capped = conversation("Capped chat", 2, 1);
    let mut app = App::default();
    app.set_conversations(vec![capped.clone()]);
    let rows: Vec<_> = (0..HISTORY_LIMIT)
        .map(|index| {
            message(
                &capped,
                &format!("c{index:03}"),
                1_000 + i64::from(index),
                "unread",
                false,
            )
        })
        .collect();
    app.messages.insert(capped.id.clone(), rows);
    app.read_positions.insert(
        capped.id.clone(),
        (0, MessageId::new(transport(), "floor").unwrap()),
    );

    let grid = buffer(&app, 100, 30);
    let sidebar: Vec<String> = (0..grid.area.height)
        .map(|y| {
            row_text(&grid, y)
                .split('│')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert!(
        sidebar.iter().any(|row| row.contains("Capped chat · 200+")),
        "the full loaded page renders the capped form: {sidebar:?}"
    );
}

#[test]
fn sidebar_and_drawer_badges_keep_the_name_at_narrow_widths() {
    let weekend = conversation("Weekend crew", 2, 40);
    let mut app = App::default();
    app.set_conversations(vec![weekend.clone()]);
    let rows = vec![
        message(&weekend, "w1", 10, "older", false),
        message(&weekend, "w2", 20, "newer", false),
        message(&weekend, "w3", 30, "newest", false),
    ];
    app.messages.insert(weekend.id.clone(), rows.clone());
    app.read_positions
        .insert(weekend.id.clone(), (10, rows[0].id().clone()));
    app.compact_sidebar = true;

    let rail = buffer(&app, 72, 24);
    let rail_text = (0..rail.area.height)
        .map(|y| {
            row_text(&rail, y)
                .split('│')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rail_text.contains("Weekend") && rail_text.contains("· 2"),
        "a clipped narrow rail keeps both the name and badge:\n{rail_text}"
    );

    app.open_chat_drawer();
    let drawer = buffer(&app, 44, 12);
    let drawer_text = (0..drawer.area.height)
        .map(|y| row_text(&drawer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        drawer_text.contains("Weekend crew · 2"),
        "the narrow Chats drawer carries the same badge:\n{drawer_text}"
    );
}

#[test]
fn header_reports_engine_session_state_separately_from_daemon_connection() {
    let chat = conversation("Session", 2, 1);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);

    // The daemon keeps the IPC connection while the engine generation dies.
    app.connection_state = ConnectionState::Connected;
    app.session = "Disconnected".into();
    let text = rendered(&app);
    assert!(
        text.contains("Disconnected"),
        "engine death must be visible in the header: {text}"
    );

    // A lost daemon connection has its own, stronger label.
    app.connection_state = ConnectionState::Disconnected;
    let text = rendered(&app);
    assert!(
        text.contains("Offline"),
        "a lost daemon connection must read as offline: {text}"
    );
}

#[test]
fn header_drops_unread_count_before_identity_at_tight_widths() {
    let weekend = conversation("Weekend crew planning", 2, 40);
    let mut app = App::default();
    app.set_conversations(vec![weekend.clone()]);
    app.selected = Some(weekend.id.clone());
    let rows = vec![
        message(&weekend, "w1", 10, "older", false),
        message(&weekend, "w2", 20, "newer", false),
        message(&weekend, "w3", 30, "newest", false),
    ];
    app.messages.insert(weekend.id.clone(), rows.clone());
    app.read_positions
        .insert(weekend.id.clone(), (10, rows[0].id().clone()));

    let wide = buffer(&app, 120, 30);
    let wide_header = row_text(&wide, 0);
    assert!(
        wide_header.contains("Weekend crew planning") && wide_header.contains("· 2"),
        "wide header shows identity and count: {wide_header:?}"
    );

    let tight = buffer(&app, 44, 10);
    let tight_header = row_text(&tight, 0);
    assert!(
        tight_header.contains("Weekend"),
        "identity survives a tight header: {tight_header:?}"
    );
    assert!(
        !tight_header.contains("· 2"),
        "the optional unread count goes first: {tight_header:?}"
    );
}

#[test]
fn mixed_attachment_parts_keep_order_exact_whitespace_and_reported_unicode_name() {
    let chat = conversation("Parts", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "mixed",
        10,
        vec![
            MessagePart::Text("before  two spaces".into()),
            attachment(Some("résumé final.pdf")),
            MessagePart::Unsupported {
                kind: "image/heic".into(),
            },
            MessagePart::Text("after".into()),
        ],
        false,
    );

    assert_eq!(
        message_text(&with_parts),
        "before  two spaces [attachment: résumé final.pdf] [attachment] after"
    );
}

#[test]
fn attachment_projection_uses_the_name_only_when_the_transport_reported_one() {
    let unnamed = MessagePart::Attachment(
        AttachmentRef::new("invoice.pdf", Some("image/png".into()), None, Some(1_024)).unwrap(),
    );
    let empty = attachment(Some(""));
    let unsupported = MessagePart::Unsupported {
        kind: "application/pdf".into(),
    };
    let named = attachment(Some("statement.pdf"));
    let chat = conversation("Fallback", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "fallback",
        10,
        vec![
            unnamed,
            empty,
            unsupported,
            named,
            MessagePart::Text("tail".into()),
        ],
        false,
    );

    assert_eq!(
        message_text(&with_parts),
        "[attachment] [attachment] [attachment] [attachment: statement.pdf] tail",
        "a name-like id or MIME type is not display metadata"
    );
}

#[test]
fn loaded_attachment_preview_matches_the_transcript_projection() {
    let chat = conversation("Preview", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "preview",
        10,
        vec![
            MessagePart::Text("see".into()),
            attachment(Some("plan v2.pdf")),
        ],
        true,
    );
    let expected = message_text(&with_parts);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), with_parts);

    assert_eq!(app.preview(&chat.id), expected);
    assert_eq!(app.preview(&chat.id), "see [attachment: plan v2.pdf]");
}

#[test]
fn attachment_name_projection_escapes_controls_bidi_quotes_backslash_and_zwj() {
    let name = "a\u{0}b\u{7}c\u{1b}d\u{7f}e\u{9f}f\u{61c}g\u{202e}h\u{2066}i\"j\\k\u{200d}l's.pdf";
    let chat = conversation("Escapes", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "escapes",
        10,
        vec![attachment(Some(name)), MessagePart::Text("tail".into())],
        false,
    );
    let expected_name =
        r#"a\0b\u{7}c\u{1b}d\u{7f}e\u{9f}f\u{61c}g\u{202e}h\u{2066}i\"j\\k\u{200d}l\'s.pdf"#;
    let projected = message_text(&with_parts);

    assert_eq!(
        projected,
        format!("[attachment: {expected_name}] tail"),
        "the projection must use Rust's debug escaping"
    );
    for raw in [
        '\u{0}', '\u{7}', '\u{1b}', '\u{7f}', '\u{9f}', '\u{61c}', '\u{202e}', '\u{2066}',
        '\u{200d}',
    ] {
        assert!(
            !projected.contains(raw),
            "raw {raw:?} leaked into the filename projection: {projected:?}"
        );
    }
    assert!(
        projected.contains("\\\""),
        "a quote must be escaped with a backslash: {projected:?}"
    );
    assert!(
        projected.contains("\\\\"),
        "a backslash must be escaped: {projected:?}"
    );
}

#[test]
fn raw_message_attachment_parts_survive_serde_and_rendering_unchanged() {
    let name = "raw\u{202e}name\u{200d}.pdf";
    let chat = conversation("Raw", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "raw",
        10,
        vec![
            MessagePart::Text("body".into()),
            attachment(Some(name)),
            MessagePart::Unsupported {
                kind: "image/heic".into(),
            },
        ],
        false,
    );
    let original_parts = with_parts.parts.to_vec();
    let encoded = serde_json::to_string(&original_parts).unwrap();
    let decoded: Vec<MessagePart> = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, original_parts, "serde round-trip changed parts");

    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), with_parts);
    let _ = rendered(&app);
    assert_eq!(
        app.messages[&chat.id][0].parts, original_parts,
        "rendering must not rewrite stored message parts"
    );
    assert_eq!(
        app.messages[&chat.id][0].parts,
        serde_json::from_str::<Vec<MessagePart>>(&encoded).unwrap(),
        "the stored parts must still equal the serialized raw parts"
    );
}

#[test]
fn attachment_filename_renders_in_the_transcript_at_24_44_and_100_columns_both_themes() {
    let chat = conversation("Wrap", 2, 1);
    let filename = "very-long-résumé-Ünïcode-ﬁle.pdf";
    let sentinel = "ATTACHMENT_SENTINEL";
    for width in [24u16, 44, 100] {
        for theme in [Theme::Dark, Theme::Light] {
            let mut app = App {
                selected: Some(chat.id.clone()),
                theme,
                ..Default::default()
            };
            app.set_conversations(vec![chat.clone()]);
            app.upsert(
                chat.id.clone(),
                message_with_parts(
                    &chat,
                    "wrap",
                    10,
                    vec![
                        MessagePart::Text(sentinel.into()),
                        attachment(Some(filename)),
                    ],
                    false,
                ),
            );
            let (buffer, hits) = tracked_frame(&mut app, width, 20);
            let transcript = transcript_flat(&buffer, &hits);

            assert!(
                transcript.contains(sentinel),
                "body missing at {width} {theme:?}: {transcript:?}"
            );
            assert!(
                transcript.contains(filename),
                "filename missing from the transcript at {width} {theme:?}: {transcript:?}"
            );
            if width >= 70 {
                assert!(
                    hits.sidebar.width > 0 && hits.transcript.x > 0,
                    "the sidebar is visible at {width}, so only the transcript rect is asserted"
                );
            } else {
                assert_eq!(
                    hits.sidebar.width, 0,
                    "narrow layouts hide the sidebar, leaving no preview to mask a stale transcript"
                );
            }
        }
    }
}

#[test]
fn unsupported_part_renders_the_generic_placeholder_without_inferring_its_kind() {
    let chat = conversation("Unsupported", 2, 1);
    let with_parts = message_with_parts(
        &chat,
        "unsupported",
        10,
        vec![MessagePart::Unsupported {
            kind: "image/heic".into(),
        }],
        false,
    );
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), with_parts);
    let (buffer, hits) = tracked_frame(&mut app, 44, 12);
    let transcript = transcript_flat(&buffer, &hits);

    assert!(transcript.contains("[attachment]"), "{transcript:?}");
    assert!(!transcript.contains("heic"), "{transcript:?}");
    assert!(!transcript.contains("image/"), "{transcript:?}");
}

#[test]
fn same_message_id_filename_update_repaints_a_reused_transcript_cache() {
    let chat = conversation("Cache update", 2, 1);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(
        chat.id.clone(),
        message_with_parts(
            &chat,
            "m1",
            10,
            vec![attachment(Some("old-report.pdf"))],
            false,
        ),
    );
    let mut cache = ui::RenderCache::default();
    let (first, first_hits) = cached_frame(&app, 44, 12, &mut cache);
    let first_text = transcript_flat(&first, &first_hits);
    assert!(first_text.contains("old-report.pdf"), "{first_text:?}");
    assert!(!first_text.contains("new-report.pdf"), "{first_text:?}");

    app.upsert(
        chat.id.clone(),
        message_with_parts(
            &chat,
            "m1",
            10,
            vec![attachment(Some("new-report.pdf"))],
            false,
        ),
    );
    let (second, second_hits) = cached_frame(&app, 44, 12, &mut cache);
    let second_text = transcript_flat(&second, &second_hits);
    assert!(
        second_text.contains("new-report.pdf"),
        "the same message id must repaint its new filename: {second_text:?}"
    );
    assert!(
        !second_text.contains("old-report.pdf"),
        "a reused cache must not serve the stale transcript: {second_text:?}"
    );
    assert!(
        cache.transcript_layout_builds() >= 2,
        "the second frame must rebuild the layout after the message changed"
    );
}

#[test]
fn reaction_summary_aggregates_sorts_and_survives_narrow_widths() {
    let chat = conversation("Reactions", 2, 1);
    let sender = chat.participants[1].handle.clone();
    let reaction = |kind: &str, part: Option<u32>| Reaction {
        sender: sender.clone(),
        kind: kind.into(),
        remote_id: None,
        part,
        timestamp: None,
    };
    let mut with_reactions = message_with_parts(
        &chat,
        "reacted",
        10,
        vec![MessagePart::Text("body".into())],
        false,
    );
    with_reactions.reactions = vec![
        reaction("like", None),
        reaction("heart", Some(0)),
        reaction("heart", None),
    ];
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), with_reactions);

    for width in [24u16, 44, 100] {
        let (buffer, hits) = cached_frame(&app, width, 14, &mut ui::RenderCache::default());
        let transcript = transcript_flat(&buffer, &hits);
        let compact: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains("·heart2·like1"),
            "width {width} lost the sorted summary: {transcript:?}"
        );
    }

    // No reactions must add no summary line.
    let plain = message_with_parts(
        &chat,
        "plain",
        20,
        vec![MessagePart::Text("plain body".into())],
        false,
    );
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), plain);
    let (buffer, hits) = cached_frame(&app, 44, 14, &mut ui::RenderCache::default());
    let transcript = transcript_flat(&buffer, &hits);
    assert!(
        !transcript.contains('·'),
        "unreacted message gained a summary: {transcript:?}"
    );
}

#[test]
fn edited_and_unsent_notes_are_truthful_and_survive_narrow_widths() {
    let chat = conversation("Mutations", 2, 1);
    let mut edited = message(&chat, "edited", 10, "revised body", false);
    edited.edited_at = Some(20);
    let mut unsent = message(&chat, "unsent", 11, "retained body", false);
    unsent.unsent_at = Some(21);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(chat.id.clone(), edited);
    app.upsert(chat.id.clone(), unsent);

    for width in [100u16, 44, 24] {
        let (buffer, hits) = cached_frame(&app, width, 18, &mut ui::RenderCache::default());
        let transcript = transcript_flat(&buffer, &hits);
        let compact: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains("revisedbody") && compact.contains("Edited"),
            "width {width} lost the edited note: {transcript:?}"
        );
        assert!(
            compact.contains("retainedbody") && compact.contains("Unsent·retainedlocally"),
            "width {width} lost the unsent note or body: {transcript:?}"
        );
        assert!(
            !transcript.contains("deleted"),
            "width {width} implied deletion: {transcript:?}"
        );
    }

    // A plain message must gain no annotation.
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(
        chat.id.clone(),
        message(&chat, "plain", 12, "plain body", false),
    );
    let (buffer, hits) = cached_frame(&app, 44, 18, &mut ui::RenderCache::default());
    let transcript = transcript_flat(&buffer, &hits);
    assert!(!transcript.contains("Edited") && !transcript.contains("Unsent"));
}

#[test]
fn attachment_picker_shows_every_choice_and_moves_the_marker() {
    const NEW: &str = "fedcba9876543210fedcba9876543210";
    const OLD: &str = "0123456789abcdef0123456789abcdef";
    let chat = conversation("Picks", 2, 1);
    let attachment = |name: &str, token: &str| {
        MessagePart::Attachment(
            AttachmentRef::new(
                format!("local-metadata:{name}"),
                Some("image/jpeg".into()),
                Some(name.into()),
                Some(3),
            )
            .unwrap()
            .with_fetch_token(Some(token.into()))
            .unwrap(),
        )
    };
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.messages.insert(
        chat.id.clone(),
        vec![
            message_with_parts(
                &chat,
                "older",
                10,
                vec![attachment("photo.jpg", OLD)],
                false,
            ),
            message_with_parts(
                &chat,
                "newer",
                20,
                vec![attachment("report.pdf", NEW)],
                false,
            ),
        ],
    );
    app.draft = "/attachment".into();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );

    let flat = |app: &App| {
        let rendered = buffer(app, 100, 30);
        (0..rendered.area.height)
            .map(|y| row_text(&rendered, y))
            .collect::<Vec<_>>()
    };
    let rows = flat(&app);
    let joined = rows.join("\n");
    assert!(joined.contains("Attachments"), "modal title: {joined}");
    assert!(joined.contains("report.pdf"), "newest choice: {joined}");
    assert!(joined.contains("photo.jpg"), "older choice: {joined}");
    assert!(joined.contains("Enter save"), "action hint: {joined}");
    assert!(
        !joined.contains(NEW) && !joined.contains(OLD),
        "tokens are never painted"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("report.pdf") && row.contains('›')),
        "newest starts selected: {joined}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let moved = flat(&app).join("\n");
    assert!(
        moved
            .lines()
            .any(|row| row.contains("photo.jpg") && row.contains('›')),
        "the marker follows the selection: {moved}"
    );
}

#[test]
fn attachment_picker_survives_a_tiny_viewport() {
    let mut app = App {
        attachment_picker: None,
        ..Default::default()
    };
    let chat = conversation("Tiny", 2, 1);
    let attachment = MessagePart::Attachment(
        AttachmentRef::new(
            "local-metadata:tiny",
            None,
            Some("photo.jpg".into()),
            Some(3),
        )
        .unwrap()
        .with_fetch_token(Some("fedcba9876543210fedcba9876543210".into()))
        .unwrap(),
    );
    app.messages.insert(
        chat.id.clone(),
        vec![message_with_parts(
            &chat,
            "tiny",
            10,
            vec![attachment],
            false,
        )],
    );
    app.selected = Some(chat.id.clone());
    app.draft = "/attachment".into();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );

    let rendered = buffer(&app, 11, 6);
    let joined = (0..rendered.area.height)
        .map(|y| row_text(&rendered, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("photo.jpg"),
        "the selected choice stays visible in the 11-column fallback: {joined}"
    );
}

#[test]
fn sidebar_bottom_row_is_clickable_and_the_scrollbar_tracks_the_viewport() {
    let conversations: Vec<_> = (0..30)
        .map(|index| conversation(&format!("chat {index:02}"), 2, index))
        .collect();
    let mut app = App {
        selected: Some(conversations[0].id.clone()),
        ..Default::default()
    };
    app.set_conversations(conversations);
    let first_id = app.visible_ids().first().cloned().unwrap();
    let last_id = app.visible_ids().last().cloned().unwrap();
    app.sidebar_reveal_pending = false;
    app.sidebar_scroll = usize::MAX;

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();

    assert!(hits.sidebar_max_offset > 0, "the fixture must overflow");
    assert!(
        hits.sidebar_scrollbar.is_some(),
        "an overflowing sidebar shows a scrollbar"
    );
    assert!(
        !hits.sidebar_rows.iter().any(|(_, id)| id == &first_id),
        "the top chat is scrolled out at the bottom"
    );
    let row = hits
        .sidebar_rows
        .iter()
        .find(|(_, id)| id == &last_id)
        .map(|(area, _)| *area)
        .expect("the last chat is fully painted at the bottom");
    assert_eq!(
        hits.at(row.x, row.y),
        Some(ui::Hit::SidebarChat(last_id.clone()))
    );

    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y, &hits);
    assert_eq!(app.selected, Some(last_id));

    let track = hits.sidebar_scrollbar.expect("track");
    assert_eq!(hits.at(track.x, track.y), Some(ui::Hit::SidebarScrollbar));
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        track.x,
        track.y,
        &hits,
    );
    assert_eq!(app.sidebar_scroll, 0, "track top maps to the list top");
    assert!(app.sidebar_drag);
    app.handle_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        track.x,
        track.y + track.height - 1,
        &hits,
    );
    assert_eq!(app.sidebar_scroll, hits.sidebar_max_offset);
    app.handle_mouse(
        MouseEventKind::Up(MouseButton::Left),
        track.x,
        track.y,
        &hits,
    );
    assert!(!app.sidebar_drag);
}

#[test]
fn sidebar_scrollbar_is_hidden_when_every_chat_fits() {
    let chats = vec![conversation("one", 2, 2), conversation("two", 2, 1)];
    let mut app = App {
        selected: Some(chats[0].id.clone()),
        ..Default::default()
    };
    app.set_conversations(chats);
    app.sidebar_reveal_pending = false;
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    assert_eq!(hits.sidebar_max_offset, 0);
    assert!(hits.sidebar_scrollbar.is_none());
}

#[test]
fn clicking_a_chat_during_new_chat_composition_parks_the_draft() {
    let first = conversation("first", 2, 2);
    let second = conversation("second", 2, 1);
    let owner = bloom_tui::app::DraftOwner::NewChat(Some("new@example.test".into()));
    let mut app = App {
        selected: Some(first.id.clone()),
        new_conversation: true,
        pending_address: Some("new@example.test".into()),
        active_draft_owner: Some(owner.clone()),
        draft: "park me".into(),
        ..Default::default()
    };
    app.set_conversations(vec![first, second.clone()]);

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render(frame, &app))
        .unwrap();
    let row = hits
        .sidebar_rows
        .iter()
        .find(|(_, id)| id == &second.id)
        .map(|(area, _)| *area)
        .expect("second chat row");
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y, &hits);
    assert!(!app.new_conversation);
    assert_eq!(app.selected, Some(second.id));
    assert!(
        app.drafts.contains_key(&owner),
        "the unfinished new-chat body is parked under its owner"
    );
}

#[test]
fn keyboard_selection_reveals_the_selected_chat() {
    let mut app = App::default();
    let chats: Vec<_> = (0..30)
        .map(|index| conversation(&format!("chat {index:02}"), 2, 30 - index))
        .collect();
    app.set_conversations(chats.clone());
    app.selected = Some(chats[0].id.clone());
    app.record_sidebar_view(0, 8, 22);
    app.select(Some(chats[20].id.clone()));
    assert_eq!(
        app.sidebar_scroll, 13,
        "selection below the window scrolls it into view"
    );
}

#[test]
fn startup_checklist_is_the_only_surface_until_all_five_checks_pass() {
    let mut app = App {
        startup: StartupState::loading(),
        connection_state: ConnectionState::Connecting,
        ..Default::default()
    };
    let text = rendered(&app);
    assert!(text.contains("0/5 checks"), "{text}");
    assert!(text.contains("Daemon connected"));
    assert!(text.contains("Engine ready"));
    assert!(text.contains("Metadata refresh"));
    assert!(text.contains("Persistence acknowledged"));
    assert!(text.contains("Local view and draft"));
    assert!(text.contains("not a fully synced account"));
    assert!(!text.contains("Open Bloom"));
    assert!(!text.contains('%'));

    app.observe_startup_connection(ConnectionState::Connected);
    app.observe_startup_readiness(ReadinessSnapshot {
        generation: 1,
        operation: 1,
        revision: 1,
        phase: ReadinessPhase::Ready,
        outcome: Some(ReadinessOutcome::Complete),
        observed_records: 12,
        can_send: true,
        engine_ready: true,
        bounded_error: None,
    });
    app.observe_startup_snapshot();
    app.observe_startup_draft_ready();
    let ready = rendered(&app);
    assert!(ready.contains("5/5 checks"), "{ready}");
    assert!(ready.contains("Observed metadata records: 12"));
    assert!(ready.contains("Your initial list is persisted locally."));
    assert!(ready.contains("not a fully synced account"));
    assert!(ready.contains("Open Bloom (Enter)"));

    app.observe_startup_readiness(ReadinessSnapshot {
        generation: 1,
        operation: 2,
        revision: 2,
        phase: ReadinessPhase::Failed,
        outcome: Some(ReadinessOutcome::Failed),
        observed_records: 0,
        can_send: false,
        engine_ready: true,
        bounded_error: Some(ReadinessError::Timeout),
    });
    let failed = rendered(&app);
    assert!(failed.contains("timed out"), "{failed}");
    assert!(failed.contains("Retry (r)"));
    assert!(failed.contains("Quit (q)"));
    assert!(!failed.contains("Open Bloom"));
}

#[test]
fn only_the_busy_drawer_row_shows_a_local_spinner() {
    let busy = conversation("Busy chat", 2, 20);
    let idle = conversation("Idle chat", 2, 10);
    let mut app = App {
        selected: Some(busy.id.clone()),
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        session: "Active".into(),
        ..Default::default()
    };
    app.set_conversations(vec![busy.clone(), idle.clone()]);
    app.begin_snapshot_request();
    app.open_chat_drawer();

    let grid = buffer(&app, 100, 30);
    let busy_row = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .find(|row| row.contains("Busy chat"))
        .expect("busy drawer row");
    let idle_row = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .find(|row| row.contains("Idle chat"))
        .expect("idle drawer row");
    assert!(
        busy_row
            .chars()
            .any(|c| ('\u{2800}'..'\u{2900}').contains(&c)),
        "the busy conversation owns the only spinner: {busy_row:?}"
    );
    assert!(
        !idle_row
            .chars()
            .any(|c| ('\u{2800}'..'\u{2900}').contains(&c)),
        "an idle conversation must not look busy: {idle_row:?}"
    );
}

#[test]
fn selected_header_says_loading_local_messages_for_local_activity() {
    let chat = conversation("Local loading chat", 2, 20);
    let mut app = App {
        selected: Some(chat.id.clone()),
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        session: "Active".into(),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.begin_snapshot_request();

    let grid = buffer(&app, 120, 30);
    let header = row_text(&grid, 0);
    assert!(
        header.contains("Loading local messages"),
        "the selected header describes local loading: {header:?}"
    );
    assert!(
        !header.contains("Syncing"),
        "local loading is not a remote sync: {header:?}"
    );
}

#[test]
fn global_metadata_sync_keeps_its_messaging_alongside_local_loading() {
    let chat = conversation("Metadata chat", 2, 20);
    let mut app = App {
        selected: Some(chat.id.clone()),
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        session: "Active".into(),
        sync_in_flight: true,
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);

    let grid = buffer(&app, 120, 30);
    let header = row_text(&grid, 0);
    assert!(header.contains("Syncing"), "{header:?}");
    assert!(
        !header.contains("Loading local messages"),
        "metadata refresh alone is not per-chat local loading: {header:?}"
    );

    app.begin_snapshot_request();
    let grid = buffer(&app, 120, 30);
    let header = row_text(&grid, 0);
    assert!(header.contains("Syncing"), "{header:?}");
    assert!(
        header.contains("Loading local messages"),
        "local activity is reported separately from metadata: {header:?}"
    );
}

// --- Passive transcript rail -------------------------------------------------

fn rail_cells(
    buffer: &ratatui::buffer::Buffer,
    hits: &ui::HitMap,
) -> Vec<(u16, String, ratatui::style::Color)> {
    let content = hits.transcript;
    if content.width == 0 {
        return Vec::new();
    }
    let x = content.right().saturating_sub(1);
    (content.y..content.bottom())
        .map(|y| {
            let cell = &buffer[(x, y)];
            (y, cell.symbol().to_owned(), cell.fg)
        })
        .collect()
}

fn expected_rail_thumb(
    view: &bloom_tui::transcript::TranscriptViewport,
    track_height: usize,
) -> Option<(usize, usize)> {
    if track_height < 2 {
        return None;
    }
    let height = view.height.min(track_height);
    let range = view.rows.saturating_sub(height);
    if view.rows == 0 || height == 0 || view.rows <= height || range == 0 {
        return None;
    }
    let thumb_length = ((track_height as u128 * height as u128) / view.rows as u128)
        .clamp(1, (track_height - 1) as u128) as usize;
    let thumb_offset = ((track_height - thumb_length) as u128 * view.top.min(range) as u128
        / range as u128) as usize;
    Some((thumb_offset, thumb_length))
}

fn assert_rail_matches(buffer: &ratatui::buffer::Buffer, hits: &ui::HitMap) {
    let content = hits.transcript;
    let cells = rail_cells(buffer, hits);
    let expected = if content.width < 3 {
        None
    } else {
        hits.transcript_view
            .as_ref()
            .and_then(|view| expected_rail_thumb(view, usize::from(content.height)))
    };
    match expected {
        Some((offset, length)) => {
            assert_eq!(
                cells.len(),
                usize::from(content.height),
                "the rail must span the transcript height"
            );
            for (index, (y, symbol, _)) in cells.iter().enumerate() {
                let want = if index >= offset && index < offset + length {
                    "█"
                } else {
                    "│"
                };
                assert_eq!(
                    symbol,
                    want,
                    "rail row {y} must be {want:?} for thumb {offset}..{}",
                    offset + length
                );
            }
        }
        None => {
            for (y, symbol, _) in &cells {
                assert!(
                    symbol.trim().is_empty(),
                    "a hidden rail must not paint {symbol:?} at row {y}"
                );
            }
        }
    }
}

fn rail_fixture() -> (App, Conversation) {
    let only = conversation("Rail thread", 2, 1);
    let mut app = App::default();
    app.set_conversations(vec![only.clone()]);
    app.selected = Some(only.id.clone());
    for index in 0..40 {
        app.upsert(
            only.id.clone(),
            message(
                &only,
                &format!("rail{index:03}"),
                1_700_000_000_000 + index * 60_000,
                &format!("rail message {index:03} with enough words to wrap on narrow screens"),
                index % 2 == 0,
            ),
        );
    }
    (app, only)
}

#[test]
fn passive_rail_geometry_matches_the_final_viewport_at_normal_widths() {
    for (width, height) in [(100u16, 30u16), (66, 18), (44, 12), (24, 12)] {
        let (mut app, _) = rail_fixture();
        let (buffer, hits) = tracked_frame(&mut app, width, height);
        assert!(
            hits.transcript_view
                .as_ref()
                .is_some_and(|view| view.rows > view.height),
            "{width}x{height} must overflow so the rail is eligible"
        );
        assert_rail_matches(&buffer, &hits);
        let cells = rail_cells(&buffer, &hits);
        assert!(
            cells.iter().any(|(_, symbol, _)| symbol == "█"),
            "{width}x{height} must paint a thumb"
        );
        assert!(
            cells.iter().any(|(_, symbol, _)| symbol == "│"),
            "{width}x{height} must paint a track"
        );
    }
}

#[test]
fn passive_rail_hides_when_content_fits_or_the_layout_is_tiny() {
    let chat = conversation("Fit rail", 2, 1);
    let mut app = App {
        selected: Some(chat.id.clone()),
        ..Default::default()
    };
    app.set_conversations(vec![chat.clone()]);
    app.upsert(
        chat.id.clone(),
        message(&chat, "only", 10, "short body", false),
    );
    let (buffer, hits) = tracked_frame(&mut app, 100, 30);
    assert!(
        hits.transcript_view
            .as_ref()
            .is_some_and(|view| view.rows <= view.height),
        "the fixture must fit"
    );
    assert_rail_matches(&buffer, &hits);
    assert!(
        rail_cells(&buffer, &hits)
            .iter()
            .all(|(_, symbol, _)| symbol.trim().is_empty()),
        "a fitting transcript must not paint a rail"
    );

    let (mut tiny, _) = rail_fixture();
    let (buffer, hits) = tracked_frame(&mut tiny, 20, 3);
    assert_eq!(
        hits.transcript.width, 0,
        "the tiny fallback has no transcript viewport"
    );
    assert!(rail_cells(&buffer, &hits).is_empty());
}

#[test]
fn passive_rail_reflects_wrapped_extent_not_message_count() {
    let only = conversation("Extent rail", 2, 1);
    let build = |body: &str| {
        let mut app = App {
            selected: Some(only.id.clone()),
            ..Default::default()
        };
        app.set_conversations(vec![only.clone()]);
        for index in 0..24 {
            app.upsert(
                only.id.clone(),
                message(
                    &only,
                    &format!("extent{index:03}"),
                    1_700_000_000_000 + index * 60_000,
                    body,
                    false,
                ),
            );
        }
        app
    };
    let mut short = build("hi");
    let mut long = build(&"wrapped words ".repeat(40));
    assert_eq!(
        short.messages[&only.id].len(),
        long.messages[&only.id].len(),
        "the comparison must hold message count equal"
    );
    let (short_buffer, short_hits) = tracked_frame(&mut short, 100, 30);
    let (long_buffer, long_hits) = tracked_frame(&mut long, 100, 30);
    assert_rail_matches(&short_buffer, &short_hits);
    assert_rail_matches(&long_buffer, &long_hits);
    let short_view = short_hits.transcript_view.as_ref().expect("short view");
    let long_view = long_hits.transcript_view.as_ref().expect("long view");
    assert!(
        long_view.rows > short_view.rows,
        "heavier wrapping must produce more rows"
    );
    let short_thumb = expected_rail_thumb(short_view, usize::from(short_hits.transcript.height))
        .expect("short rail")
        .1;
    let long_thumb = expected_rail_thumb(long_view, usize::from(long_hits.transcript.height))
        .expect("long rail")
        .1;
    assert!(
        long_thumb < short_thumb,
        "a larger wrapped extent must shorten the thumb: {long_thumb} vs {short_thumb}"
    );
}

#[test]
fn passive_rail_uses_the_byline_adjusted_final_top() {
    let (mut app, _) = rail_fixture();
    let (_, first) = tracked_frame(&mut app, 100, 24);
    let max_scroll = first
        .transcript_view
        .as_ref()
        .expect("viewport")
        .max_scroll();
    let mut checked = 0;
    let mut found = false;
    for back in 0..=max_scroll {
        app.scroll = back;
        app.reading_anchor = None;
        app.transcript_viewport = None;
        let (buffer, hits) = tracked_frame(&mut app, 100, 24);
        let view = hits.transcript_view.as_ref().expect("viewport");
        assert_rail_matches(&buffer, &hits);
        let raw_top = view.max_scroll().saturating_sub(back);
        if view.top == raw_top {
            continue;
        }
        found = true;
        let track = usize::from(hits.transcript.height);
        let adjusted = expected_rail_thumb(view, track).expect("adjusted rail");
        let raw = ((track - adjusted.1) as u128 * raw_top.min(view.max_scroll()) as u128
            / view.max_scroll() as u128) as usize;
        if raw != adjusted.0 {
            assert_eq!(
                rail_cells(&buffer, &hits)
                    .iter()
                    .position(|(_, symbol, _)| symbol == "█"),
                Some(adjusted.0),
                "the thumb must start at the byline-adjusted top, not the raw scroll"
            );
            checked += 1;
            break;
        }
    }
    assert!(found, "the fixture must exercise a byline adjustment");
    assert!(checked > 0, "the adjustment must move the thumb");
}

#[test]
fn passive_rail_tracks_the_bounded_search_context_extent() {
    let chat = conversation("Context rail", 2, 1);
    let build = || {
        let mut app = App {
            selected: Some(chat.id.clone()),
            ..Default::default()
        };
        app.set_conversations(vec![chat.clone()]);
        app.upsert(
            chat.id.clone(),
            message(&chat, "normal-newest", 90, "NORMAL_LATEST_ROW", false),
        );
        app
    };
    let mut short = build();
    let (generation, conversation, target, token) =
        start_context_lookup(&mut short, &chat, "target");
    assert!(short.finish_message_context(
        generation,
        conversation,
        target,
        token,
        vec![message(&chat, "target", 20, "short context", false)]
    ));
    let (buffer, hits) = tracked_frame(&mut short, 100, 24);
    assert_rail_matches(&buffer, &hits);
    assert!(
        rail_cells(&buffer, &hits)
            .iter()
            .all(|(_, symbol, _)| symbol.trim().is_empty()),
        "a short search context fits and hides the rail"
    );

    let mut long = build();
    let (generation, conversation, target, token) =
        start_context_lookup(&mut long, &chat, "target");
    let rows = (0..40)
        .map(|index| {
            let id = if index == 20 {
                "target".to_owned()
            } else {
                format!("ctx{index:03}")
            };
            message(
                &chat,
                &id,
                1_700_000_000_000 + index,
                &format!("context row {index:03} with words that wrap"),
                false,
            )
        })
        .collect::<Vec<_>>();
    assert!(long.finish_message_context(generation, conversation, target, token, rows.clone()));
    let (buffer, hits) = tracked_frame(&mut long, 100, 24);
    assert_rail_matches(&buffer, &hits);
    let view = hits.transcript_view.as_ref().expect("context viewport");
    assert_eq!(
        view.messages
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>(),
        rows.iter().map(|row| row.id().clone()).collect::<Vec<_>>()
    );
    assert!(
        rail_cells(&buffer, &hits)
            .iter()
            .any(|(_, symbol, _)| symbol == "█"),
        "the rail must reflect the loaded context buffer"
    );
}

#[test]
fn passive_rail_keeps_anchors_across_arrival_prepend_reconcile_and_resize() {
    use bloom_ipc::EventKind;
    let (mut app, only) = reading_fixture();
    let anchor = app.reading_anchor.clone().expect("reading anchor");
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_rail_matches(&buffer, &hits);

    app.apply_event(EventKind::Message {
        conversation_id: only.id.clone(),
        message: message(
            &only,
            "rail-arrival",
            1_700_000_001_000,
            &"new wrapped content ".repeat(30),
            false,
        ),
        push_sequence: None,
    });
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_rail_matches(&buffer, &hits);

    let (id, _, token) = app.begin_load_older().expect("older page cursor");
    app.finish_load_older(
        id,
        token,
        (0..12)
            .map(|index| {
                message(
                    &only,
                    &format!("rail-old{index:03}"),
                    1_699_999_999_000 + index,
                    &format!("prepended older message {index:03}"),
                    false,
                )
            })
            .collect(),
        OLDER_PAGE_LIMIT,
    );
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_rail_matches(&buffer, &hits);

    app.apply_snapshot(bloom_tui::daemon::Snapshot {
        token: None,
        revision: 30,
        conversations: vec![only.clone()],
        visible: Some(only.id.clone()),
        history: app.messages[&only.id].clone(),
    });
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_eq!(app.reading_anchor.as_ref().unwrap().message, anchor.message);
    assert_rail_matches(&buffer, &hits);

    for (width, height) in [(66u16, 18u16), (24, 12), (100, 24)] {
        let (buffer, hits) = tracked_frame(&mut app, width, height);
        assert_eq!(
            app.reading_anchor.as_ref().unwrap().message,
            anchor.message,
            "resize to {width}x{height} must keep the anchor"
        );
        assert_rail_matches(&buffer, &hits);
    }
}

#[test]
fn latest_keeps_its_behavior_with_a_visible_rail() {
    let (mut app, _) = reading_fixture();
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_rail_matches(&buffer, &hits);
    assert!(rail_cells(&buffer, &hits)
        .iter()
        .any(|(_, symbol, _)| symbol == "█"));
    let latest = hits.latest.expect("Latest control");
    assert_eq!(hits.at(latest.x, latest.y), Some(ui::Hit::Latest));
    assert_eq!(
        latest.y,
        hits.transcript.bottom(),
        "Latest stays on the reserved spacer row"
    );
    assert_eq!(
        latest.x + latest.width,
        hits.transcript.right(),
        "Latest keeps the original outer content width"
    );
    app.handle_mouse(
        MouseEventKind::Down(MouseButton::Left),
        latest.x,
        latest.y,
        &hits,
    );
    assert!(!app.is_scrolled_back());
    let (buffer, hits) = tracked_frame(&mut app, 100, 24);
    assert_rail_matches(&buffer, &hits);
    assert!(hits.latest.is_none());
}

#[test]
fn passive_rail_click_drag_and_release_do_nothing_but_wheel_still_scrolls() {
    let (mut app, _) = reading_fixture();
    let (_, hits) = tracked_frame(&mut app, 100, 24);
    let content = hits.transcript;
    let x = content.right().saturating_sub(1);
    let y = content.y + 1;
    assert_eq!(
        hits.at(x, y),
        Some(ui::Hit::Transcript),
        "the rail column is still the transcript, not a new hit target"
    );
    let before = app.scroll;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let action = app.handle_mouse(kind, x, y, &hits);
        assert_eq!(action, Action::None);
        assert_eq!(app.scroll, before, "a passive rail must not move the view");
    }
    app.handle_mouse(MouseEventKind::ScrollUp, x, y, &hits);
    assert!(
        app.scroll > before,
        "wheel over the uncovered rail still scrolls the transcript"
    );
}

#[test]
fn passive_rail_input_is_owned_by_a_covering_overlay() {
    let (mut app, _) = reading_fixture();
    app.open_settings();
    let (_, hits) = tracked_frame(&mut app, 100, 30);
    let x = hits.transcript.right().saturating_sub(1);
    let covered = (hits.transcript.y..hits.transcript.bottom())
        .find(|y| hits.overlay_contains(x, *y))
        .expect("the settings overlay must cover part of the rail column");
    let before = app.scroll;
    app.handle_mouse(MouseEventKind::ScrollUp, x, covered, &hits);
    assert_eq!(app.scroll, before, "the overlay owns the rail wheel");
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), x, covered, &hits);
    assert_eq!(app.scroll, before, "the overlay owns the rail click");
}

#[test]
fn passive_rail_uses_theme_colors_and_stays_distinct() {
    let (mut app, _) = rail_fixture();
    let mut colors = Vec::new();
    for theme in [Theme::Dark, Theme::Light] {
        app.theme = theme;
        let (buffer, hits) = tracked_frame(&mut app, 100, 30);
        assert_rail_matches(&buffer, &hits);
        let cells = rail_cells(&buffer, &hits);
        let thumb = cells
            .iter()
            .find(|(_, symbol, _)| symbol == "█")
            .map(|(_, _, fg)| *fg)
            .expect("thumb");
        let track = cells
            .iter()
            .find(|(_, symbol, _)| symbol == "│")
            .map(|(_, _, fg)| *fg)
            .expect("track");
        assert_ne!(thumb, track, "{theme:?} must separate thumb and track");
        colors.push((thumb, track));
    }
    assert_ne!(
        colors[0], colors[1],
        "dark and light themes must paint different rail colors"
    );
}
