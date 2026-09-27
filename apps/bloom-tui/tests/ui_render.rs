//! Headless render check that the layout matches the design mockup.
//!
//! ratatui's `TestBackend` renders into an in-memory buffer, so the structural
//! elements of `design/previews/tui-preview.png` can be asserted without a
//! terminal, a daemon, or the engine.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_tui::{app::App, ui};
use ratatui::{backend::TestBackend, Terminal};

fn buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
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
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
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

    // The outer frame is drawn on all four edges.
    assert!((0..100).filter(|x| symbol(*x, 0) == "─").count() > 80);
    assert!((0..100).filter(|x| symbol(*x, 29) == "─").count() > 80);
    assert!((1..29).filter(|y| symbol(0, *y) == "│").count() >= 25);
    assert!((1..29).filter(|y| symbol(99, *y) == "│").count() >= 25);

    // Exactly one composer line, and the hints sit directly beneath it.
    let all = rendered(&app);
    assert_eq!(all.matches("›  Write a message...").count(), 1);
    let prompt_row = (1..29)
        .find(|y| (0..100).any(|x| symbol(x, *y) == "›"))
        .expect("the composer prompt must be drawn");
    let hints_row = prompt_row + 1;
    let hints: String = (0..100).map(|x| symbol(x, hints_row)).collect();
    assert!(
        hints.contains("chats"),
        "the key hints must sit directly under the composer, found: {hints:?}"
    );

    for expected in [
        "C H A T S",              // sidebar label
        "Bloom /",                // header prefix
        "Weekend crew",           // selected conversation title
        "3 people",               // participant count wording
        "Saturday looks perfect", // transcript body
        "Yes, 8:40 works.",       // our own message
        "delivered",              // delivery marker
        "Write a message",        // composer placeholder
        "↑↓ chats",               // footer hints
        "Active",                 // engine session in the header
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
    let badge_row =
        (1..4).find(|y| (rule_x + 1..99).filter(|x| symbol(*x, *y) == "│").count() >= 2);
    assert!(
        badge_row.is_some(),
        "the header status should be framed, like the mockup's badge"
    );

    // The transcript is capped rather than running edge to edge, which is the
    // mockup's proportion: the columns before the frame edge stay empty.
    for y in 4..26 {
        for x in 94..99 {
            assert_eq!(
                symbol(x, y),
                " ",
                "transcript should not reach the frame edge at ({x},{y})"
            );
        }
    }

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
        text.contains("C H A T S"),
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
        text.contains("↑↓ chats"),
        "footer hints still present when empty:\n{text}"
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
