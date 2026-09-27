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
    app::{App, Theme},
    ui,
};
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
        pinned.contains("Enter open"),
        "the default hint row must stay as the mockup shows it:\n{pinned}"
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
        // The composer rule is the lowest long horizontal run inside the window,
        // above the frame's own bottom border.
        let rule = rows
            .iter()
            .enumerate()
            .rev()
            .skip(1)
            .find(|(_, row)| row.matches('─').count() > 40)
            .map(|(index, _)| index)
            .expect("the composer rule must be drawn");
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
        let header = text.lines().nth(1).expect("a header row");
        let label = format!(" {expected} ");
        let at = header
            .find(&label)
            .unwrap_or_else(|| panic!("the {expected} control must be in the header:\n{text}"));
        let right_border = header[..at].rfind('│').expect("the chip's left border");
        let end = at + label.len();
        let chip_right = header[end..]
            .find('│')
            .map(|offset| end + offset)
            .expect("the chip's right border");
        let trailing = header[chip_right + '│'.len_utf8()..].chars().count();
        assert!(
            trailing >= 2,
            "the {expected} control must keep a gap inside the frame, found {trailing} trailing cells"
        );
        assert!(
            right_border < at,
            "the chip must be drawn as a bordered control"
        );
    }
}

#[test]
fn a_single_match_heading_is_not_followed_by_leftovers() {
    // The filter heading used to be drawn over the plain `C H A T S` label, and the
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
    let heading = text.lines().nth(1).expect("a label row");
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
    // At 60 columns there is room for the identity and the control; losing the control
    // left no visible way to discover the theme.
    let mut app = App::default();
    app.set_conversations(vec![conversation("Weekend crew", 3, 40)]);
    app.selected = app.conversations.first().map(|c| c.id.clone());
    let grid = buffer(&app, 60, 20);
    let row: String = (0..grid.area.width)
        .map(|x| grid[(x, 1)].symbol())
        .collect();
    assert!(
        row.contains("Dark"),
        "the theme control must survive at 60 columns, got:\n{row}"
    );
    let footer: String = (0..grid.area.width)
        .map(|x| grid[(x, 18)].symbol())
        .collect();
    assert!(
        footer.contains("t theme"),
        "the narrow hints must still name the theme key, got:\n{footer}"
    );
}
