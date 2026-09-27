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
    let mut app = App::default();
    app.set_conversations(vec![selected.clone(), other]);
    app.selected = Some(selected.id.clone());
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

    for expected in [
        "CHATS",                  // sidebar label
        "Bloom /",                // header prefix
        "Weekend crew",           // selected conversation title
        "3 people",               // participant count wording
        "Saturday looks perfect", // transcript body
        "Yes, 8:40 works.",       // our own message
        "delivered",              // delivery marker
        "Write a message",        // composer placeholder
        "↑↓ chats",               // footer hints
        "Connected",              // connection state
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
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
    let mut app = App::default();
    app.connection = "Connecting".into();

    let text = rendered(&app);

    assert!(
        text.contains("CHATS"),
        "sidebar still present when empty:\n{text}"
    );
    assert!(
        text.contains("Bloom /"),
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
fn new_conversation_mode_asks_for_a_recipient() {
    let mut app = App::default();
    app.begin_new_conversation();

    let text = rendered(&app);
    assert!(
        text.contains("To: recipient address"),
        "new message mode must ask for an address:\n{text}"
    );
}
