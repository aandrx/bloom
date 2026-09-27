//! Headless render check that the layout matches the design mockup.
//!
//! ratatui's `TestBackend` renders into an in-memory buffer, so the structural
//! elements of `design/previews/tui-preview.png` can be asserted without a
//! terminal, a daemon, or the engine.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_tui::{
    app::{App, SendFeedback, SendPhase, Theme},
    daemon::ConnectionState,
    ui,
};
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
        .find(|y| (0..100).any(|x| symbol(x, *y) == "↑"))
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
        "↑↓ scroll",              // footer hints
        "Tab chats",              // sidebar focus shortcut
        "Dark",                   // current theme in the header
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }

    // The hint line matches the mockup and no longer carries connection state.
    let hints_row = (0..30)
        .find(|y| (0..100).any(|x| symbol(x, *y) == "↑"))
        .expect("the hint line must be drawn");
    let hint_text: String = (0..100).map(|x| symbol(x, hints_row)).collect();
    assert!(
        !hint_text.contains("Connected"),
        "connection state belongs in the header, found: {hint_text:?}"
    );

    // The header status is an outlined chip, so it has side borders on its row.
    let header = row_text(&buffer, 0);
    let theme_at = header.find(" Dark ").expect("framed theme chip");
    let controls = &header[theme_at + " Dark ".len()..];
    assert!(controls.contains('⚙') && controls.contains('?'));
    assert!(header.contains("● ⠋ Active"));

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
        connection: "Connecting".into(),
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
        text.contains("n start"),
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
    assert!(screen.contains("h/l chats"), "leader keys:\n{screen}");
    assert!(screen.contains("j/k scroll"), "leader keys:\n{screen}");
    assert!(screen.contains("Esc cancel"), "leader keys:\n{screen}");
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
    assert!(screen.contains("Message sent."));
    assert!(!screen.contains("delivered"));
}

#[test]
fn session_status_remains_visible_when_the_theme_control_is_hidden() {
    let selected = conversation("Weekend crew", 3, 40);
    let mut app = App {
        selected: Some(selected.id.clone()),
        connection: "Connected".into(),
        connection_state: ConnectionState::Connected,
        session: "Active".into(),
        ..Default::default()
    };
    app.set_conversations(vec![selected]);
    let grid = buffer(&app, 40, 12);
    let header = row_text(&grid, 0);
    assert!(header.contains("● Active"), "status is visible: {header:?}");
    assert!(header.contains("Weekend"), "title is retained: {header:?}");
    assert!(
        !header.contains("Dark"),
        "theme control yields space: {header:?}"
    );
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
fn sidebar_people_counts_share_the_name_row_and_align_right() {
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
    assert!(weekend.trim_end().ends_with('3'));
    assert!(jules.trim_end().ends_with('2'));
    assert_eq!(
        weekend[..weekend.rfind('3').unwrap()].chars().count(),
        jules[..jules.rfind('2').unwrap()].chars().count(),
        "counter columns differ: {weekend:?} vs {jules:?}"
    );
}

#[test]
fn settings_panel_shows_selected_local_preferences() {
    let app = App {
        settings_open: true,
        settings_selection: 1,
        compact_sidebar: true,
        ..Default::default()
    };
    let screen = rendered(&app);
    assert!(screen.contains("│Settings"), "settings header:\n{screen}");
    assert!(
        screen.contains("Automatic (Dark)"),
        "theme value:\n{screen}"
    );
    assert!(screen.contains("Sidebar"), "sidebar setting:\n{screen}");
    assert!(screen.contains("Compact"), "compact value:\n{screen}");
    assert!(
        screen.contains("choices are stored locally"),
        "settings composer:\n{screen}"
    );
    assert!(screen.contains("Esc close"), "settings help:\n{screen}");
}

#[test]
fn settings_remain_actionable_on_a_short_terminal() {
    let app = App {
        settings_open: true,
        ..Default::default()
    };
    let compact = buffer(&app, 40, 8);
    let screen = (0..compact.area.height)
        .map(|row| row_text(&compact, row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Automatic"), "compact settings:\n{screen}");
    assert!(
        screen.contains("Sidebar"),
        "compact settings retain both setting rows:\n{screen}"
    );
    assert!(screen.contains("Esc close"), "compact controls:\n{screen}");
    assert!(screen.contains("Settings"), "settings header:\n{screen}");
}

#[test]
fn footer_hints_follow_the_active_mode() {
    let normal = rendered(&App::default());
    assert!(normal.contains("n start"), "empty-store action:\n{normal}");
    assert!(normal.contains("? help"), "normal quit action:\n{normal}");
    assert!(
        normal.contains("↑↓ scroll · Tab chats · Enter send · / commands · ? help"),
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
        composing_text.contains("↑↓ scroll"),
        "scroll hint:\n{composing_text}"
    );
    assert!(
        !composing_text.contains("Enter open"),
        "stale nav hint:\n{composing_text}"
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
    assert!(
        filter_text.contains("No matching conversations"),
        "filter empty state:\n{filter_text}"
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
        compact_text.contains("↑↓ scroll"),
        "compact navigation hint:\n{compact_text}"
    );
    assert!(
        compact_text.contains("/ cmds"),
        "compact command hint:\n{compact_text}"
    );
    let compact_prompt = (0..compact.area.height)
        .find(|y| (0..compact.area.width).any(|x| compact[(x, *y)].symbol() == "›"))
        .unwrap();
    assert!(
        !row_text(&compact, compact_prompt + 1).contains("─"),
        "composer is bare:\n{compact_text}"
    );
    assert!(
        row_text(&compact, compact_prompt + 2).contains("↑↓ scroll"),
        "compact footer remains below the boundary:\n{compact_text}"
    );
    let sidebar_divider = (1..compact.area.width - 1)
        .find(|x| compact[(*x, 3)].symbol() == "│")
        .expect("compact sidebar divider");
    assert!(
        sidebar_divider < 14,
        "compact sidebar preserves transcript width: {sidebar_divider}"
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
        compose_text.contains("Enter send") || compose_text.contains("↑↓ scroll"),
        "compact composer hints:\n{compose_text}"
    );
    assert!(
        compose_text.contains("↑↓ scroll"),
        "small composer navigation:\n{compose_text}"
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
        filter_text.contains("↑↓ select"),
        "small filter hint:\n{filter_text}"
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
        narrow_text.contains("↑↓ scroll"),
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
    let status = column_of(&grid, 0, "⠋ Disconnected").expect("status visible");
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
    let narrow_theme = column_of(&narrow, 0, "Dark").expect("narrow theme chip");
    assert!(
        narrow_header.contains("●") && narrow_header.contains("?"),
        "status and help remain visible: {narrow_header:?}"
    );
    assert!(
        narrow_theme > 0,
        "theme chip remains visible: {narrow_header:?}"
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
        ratatui::style::Color::Rgb(24, 24, 37)
    );

    let message_row = (0..grid.area.height)
        .find(|y| row_text(&grid, *y).contains("A user message highlighted"))
        .unwrap();
    let message_column = column_of(&grid, message_row, "A user message").unwrap();
    assert_eq!(
        grid[(message_column, message_row)].bg,
        ratatui::style::Color::Rgb(49, 50, 68)
    );
    assert_eq!(
        grid[(grid.area.width - 5, message_row)].bg,
        ratatui::style::Color::Rgb(49, 50, 68)
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

    app.draft = "abcdefghijklmnopqrstuvwxyz".into();
    app.draft_cursor = app.draft.chars().count();
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            ui::render(frame, &app);
        })
        .unwrap();
    let grid = terminal.backend().buffer().clone();
    let screen = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains("pqrstuvwxyz"),
        "draft tail is visible:\n{screen}"
    );
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert!(cursor.x < grid.area.width && cursor.y < grid.area.height);
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
        // The composer is bare now, so the row above it is the last transcript row.
        let rule = rows
            .iter()
            .position(|row| row.contains('›'))
            .expect("the composer prompt must be drawn");
        let above = rows[rule - 1];
        let has_clock = above
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
fn the_theme_control_stays_visible_on_a_narrow_terminal() {
    // On narrow terminals the theme chip is hidden while usable hints remain.
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 3, 40)]);
    app.selected = app.conversations.first().map(|c| c.id.clone());
    let grid = buffer(&app, 60, 20);
    let row: String = (0..grid.area.width)
        .map(|x| grid[(x, 0)].symbol())
        .collect();
    assert!(
        row.contains("Dark"),
        "theme chip and trailing controls at 60 columns:\n{row}"
    );
    let footer: String = (0..grid.area.height)
        .map(|y| row_text(&grid, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        footer.contains("↑↓ scroll"),
        "the narrow hints must still name the theme key, got:\n{footer}"
    );
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
        pane.starts_with("Weekend crew"),
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
    let title_row = (0..30)
        .find(|y| row_text(&grid, *y).contains("Commands"))
        .expect("popup title");
    let panel_x = column_of(&grid, title_row, "╭").expect("popup corner");
    let prompt_row = (0..30)
        .find(|y| row_text(&grid, *y).contains("›  /"))
        .expect("composer prompt");
    let prompt_x = column_of(&grid, prompt_row, "›").expect("prompt glyph");
    assert_eq!(panel_x, prompt_x, "popup starts at the chat's left edge");
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
