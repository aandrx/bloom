use super::*;
use crate::app::SearchContext;
use bloom_domain::{ConversationKind, Handle, HandleKind, MessagePart, Participant, TransportId};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

fn fixture() -> App {
    let transport = TransportId::new("openbubbles").unwrap();
    let id = ConversationId::new(transport.clone(), "synthetic-cache-chat").unwrap();
    let me = Handle::new(HandleKind::Email, "me@example.invalid").unwrap();
    let sender = Handle::new(HandleKind::Email, "sender@example.invalid").unwrap();
    let conversation = Conversation {
        id: id.clone(),
        remote_id: "synthetic-cache-chat".into(),
        kind: ConversationKind::Direct,
        title: Some("Synthetic cache chat".into()),
        participants: vec![
            Participant {
                handle: me.clone(),
                display_name: None,
                is_me: true,
            },
            Participant {
                handle: sender.clone(),
                display_name: Some("Synthetic sender".into()),
                is_me: false,
            },
        ],
        created_at: 1,
        last_message_at: Some(1_700_000_000_100),
        aliases: vec![],
        preview: None,
    };
    let mut app = App::default();
    app.set_conversations(vec![conversation]);
    for index in 0..12 {
        let mine = index % 3 == 0;
        let body = if index == 10 {
            "overheight Unicode e\u{301} 界 👨‍👩‍👧‍👦 ".repeat(100)
        } else {
            format!("synthetic body {index} with words and wrapping ").repeat(3)
        };
        app.upsert(
            id.clone(),
            Message::new(
                MessageId::new(transport.clone(), format!("cache-message-{index}")).unwrap(),
                id.clone(),
                None,
                if mine { me.clone() } else { sender.clone() },
                vec![MessagePart::Text(body)],
                1_700_000_000_000 + index * 400_000,
                None,
                DeliveryState::Delivered,
                mine,
                None,
                vec![],
                None,
            )
            .unwrap(),
        );
    }
    app
}

fn draw(
    app: &App,
    width: u16,
    height: u16,
    cache: &mut RenderCache,
    mono: bool,
) -> (Buffer, HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            hits = render_cached_with_mode(frame, app, mono, cache);
        })
        .unwrap();
    (terminal.backend().buffer().clone(), hits)
}

fn equivalent(app: &App, width: u16, height: u16, cache: &mut RenderCache, mono: bool) -> HitMap {
    let (actual, hits) = draw(app, width, height, cache, mono);
    let (expected, reference) = draw(app, width, height, &mut RenderCache::default(), mono);
    assert_eq!(actual, expected);
    assert_eq!(hits.composer, reference.composer);
    assert_eq!(hits.composer_positions, reference.composer_positions);
    assert_eq!(hits.latest, reference.latest);
    assert_eq!(hits.sidebar_rows, reference.sidebar_rows);
    let geometry = |hit: &HitMap| {
        hit.transcript_view.as_ref().map(|view| {
            (
                view.conversation.clone(),
                view.rows,
                view.height,
                view.top,
                view.messages
                    .iter()
                    .map(|message| (message.id.clone(), message.start, message.end))
                    .collect::<Vec<_>>(),
            )
        })
    };
    assert_eq!(geometry(&hits), geometry(&reference));
    hits
}

#[test]
fn cache_reuses_layout_across_local_ui_changes_and_unchanged_snapshots() {
    let mut app = fixture();
    let mut cache = RenderCache::default();
    let hits = equivalent(&app, 100, 30, &mut cache, false);
    app.record_transcript(hits.transcript_view);
    assert_eq!(cache.builds, 1);
    app.scroll_back(8);
    equivalent(&app, 100, 30, &mut cache, false);
    app.draft = "unsent synthetic draft".into();
    equivalent(&app, 100, 18, &mut cache, false);
    app.settings_open = true;
    equivalent(&app, 100, 30, &mut cache, false);
    app.settings_open = false;
    app.help_open = true;
    equivalent(&app, 100, 30, &mut cache, false);
    app.help_open = false;
    app.draft = "/".into();
    app.command_menu = true;
    equivalent(&app, 100, 30, &mut cache, false);
    app.command_menu = false;
    app.frame += 1;
    let id = app.selected.clone().unwrap();
    let same_history = app.messages[&id].clone();
    app.messages.insert(id, same_history);
    equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(
        cache.builds, 1,
        "local state and equivalent snapshot must not reflow history"
    );
}

#[test]
fn cache_invalidates_for_render_dependencies_and_reuses_prior_width() {
    let mut app = fixture();
    let id = app.selected.clone().unwrap();
    let mut cache = RenderCache::default();
    equivalent(&app, 100, 30, &mut cache, false);
    equivalent(&app, 90, 30, &mut cache, false);
    equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.builds, 2);
    let mut miss = |app: &App, mono| {
        let before = cache.builds;
        equivalent(app, 100, 30, &mut cache, mono);
        assert_eq!(cache.builds, before + 1);
    };
    app.messages.get_mut(&id).unwrap()[0].parts = vec![MessagePart::Text("edited body".into())];
    miss(&app, false);
    app.messages.get_mut(&id).unwrap()[0].delivery = DeliveryState::Read;
    miss(&app, false);
    app.messages.get_mut(&id).unwrap().remove(1);
    miss(&app, false);
    let handle = app.conversations[0].participants[1].handle.clone();
    app.nicknames
        .insert(crate::app::nickname_key(&handle), "Changed nickname".into());
    miss(&app, false);
    app.conversations[0].participants[1].display_name = Some("Changed participant".into());
    miss(&app, false);
    app.conversations[0]
        .aliases
        .push(ConversationId::new(id.transport().clone(), "retired-cache-alias").unwrap());
    miss(&app, false);
    app.theme = Theme::Light;
    miss(&app, false);
    miss(&app, true);
    let before = cache.builds;
    app.theme = Theme::Dark;
    equivalent(&app, 100, 30, &mut cache, true);
    assert_eq!(cache.builds, before, "monochrome palettes are identical");
    app.new_conversation = true;
    equivalent(&app, 100, 30, &mut cache, true);
    assert_eq!(cache.builds, before + 1);
}

#[test]
fn cache_is_bounded_lru_and_keeps_recent_chat_layouts() {
    let mut app = fixture();
    let mut cache = RenderCache::default();
    for width in [80, 90, 100, 110] {
        draw(&app, width, 30, &mut cache, false);
    }
    assert_eq!(cache.transcripts.len(), TRANSCRIPT_CACHE_CAPACITY);
    draw(&app, 80, 30, &mut cache, false);
    draw(&app, 120, 30, &mut cache, false);
    assert_eq!(cache.builds, 5);
    draw(&app, 80, 30, &mut cache, false);
    assert_eq!(cache.builds, 5);
    draw(&app, 90, 30, &mut cache, false);
    assert_eq!(cache.builds, 6);
    assert_eq!(cache.transcripts.len(), TRANSCRIPT_CACHE_CAPACITY);
    let first = app.selected.clone().unwrap();
    let mut second = app.conversations[0].clone();
    second.id = ConversationId::new(first.transport().clone(), "second-cache-chat").unwrap();
    app.set_conversations(vec![app.conversations[0].clone(), second.clone()]);
    draw(&app, 90, 30, &mut cache, false);
    app.select(Some(second.id));
    draw(&app, 90, 30, &mut cache, false);
    let after_second = cache.builds;
    app.select(Some(first));
    equivalent(&app, 90, 30, &mut cache, false);
    assert_eq!(cache.builds, after_second);
}

#[test]
fn edited_or_removed_history_drops_obsolete_width_variants() {
    let mut app = fixture();
    let id = app.selected.clone().unwrap();
    let mut cache = RenderCache::default();
    draw(&app, 90, 30, &mut cache, false);
    draw(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.transcripts.len(), 2);
    app.messages.get_mut(&id).unwrap()[0].parts = vec![MessagePart::Text("replacement".into())];
    equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.transcripts.len(), 1);
    assert_eq!(cache.transcripts[0].key.messages, app.messages[&id]);
    app.messages.get_mut(&id).unwrap().clear();
    equivalent(&app, 100, 30, &mut cache, false);
    assert!(cache
        .transcripts
        .iter()
        .all(|entry| entry.key.messages.is_empty()));
}

#[test]
fn context_loading_and_ready_switch_layouts_and_latest_geometry() {
    let mut app = fixture();
    let id = app.selected.clone().unwrap();
    let mut cache = RenderCache::default();
    equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.builds, 1);

    app.search_context = Some(SearchContext {
        generation: 1,
        conversation: id.clone(),
        target: app.messages[&id][5].id().clone(),
        loading: true,
        requested: true,
        activity: None,
        messages: Vec::new(),
    });
    let loading = equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(
        cache.builds, 2,
        "loading context reflows to an empty transcript"
    );
    assert!(
        cache.transcripts.last().unwrap().key.messages.is_empty(),
        "loading context never caches normal history as its rows"
    );

    let rows = app.messages[&id][4..7].to_vec();
    let target = rows[1].id().clone();
    app.search_context = Some(SearchContext {
        generation: 2,
        conversation: id.clone(),
        target,
        loading: false,
        requested: true,
        activity: None,
        messages: rows.clone(),
    });
    let ready = equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(
        cache.builds, 3,
        "ready context reflows to its own bounded rows"
    );
    assert_eq!(cache.transcripts.last().unwrap().key.messages, rows);
    assert!(
        ready.latest.is_some(),
        "ready context always has a Latest exit"
    );
    assert_eq!(
        ready.transcript_view.as_ref().map(|view| view
            .messages
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>()),
        Some(rows.iter().map(|row| row.id().clone()).collect())
    );
    assert!(
        loading.latest.is_some(),
        "loading context still offers a Latest exit"
    );

    let builds = cache.builds;
    equivalent(&app, 100, 30, &mut cache, false);
    assert_eq!(
        cache.builds, builds,
        "unchanged ready context reuses its layout"
    );
}

#[test]
fn windowed_paragraph_matches_full_history_reflow() {
    let app = fixture();
    for outer_width in [24, 44, 70, 100] {
        let mut cache = RenderCache::default();
        draw(&app, outer_width, 30, &mut cache, false);
        let cached = cache.transcripts.last().unwrap();
        let layout = &cached.layout;
        let width = cached.key.width;
        let mut tops = vec![0, layout.rows / 2, layout.rows.saturating_sub(1)];
        for start in layout.line_starts.iter().step_by(3) {
            tops.extend([*start, start.saturating_add(1), start.saturating_sub(1)]);
        }
        tops.sort_unstable();
        tops.dedup();
        for height in [1, 2, 7, 20] {
            for top in &tops {
                for muted in [false, true] {
                    let top = (*top).min(layout.rows.saturating_sub(usize::from(height))) as u16;
                    let (slice, relative) = transcript_window(layout, top, height, muted, DARK);
                    assert!(
                        slice.len() <= usize::from(height) + 1,
                        "painting work must be bounded by the viewport, not loaded history"
                    );
                    let mut full = layout.lines.clone();
                    if muted {
                        for line in &mut full {
                            for span in &mut line.spans {
                                span.style = Style::default().fg(DARK.muted).bg(DARK.surface);
                            }
                        }
                    }
                    let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
                    actual
                        .draw(|f| {
                            f.render_widget(
                                Paragraph::new(slice)
                                    .wrap(Wrap { trim: false })
                                    .scroll((relative, 0))
                                    .style(Style::default().bg(DARK.surface)),
                                f.area(),
                            )
                        })
                        .unwrap();
                    let mut reference = Terminal::new(TestBackend::new(width, height)).unwrap();
                    reference
                        .draw(|f| {
                            f.render_widget(
                                Paragraph::new(full)
                                    .wrap(Wrap { trim: false })
                                    .scroll((top, 0))
                                    .style(Style::default().bg(DARK.surface)),
                                f.area(),
                            )
                        })
                        .unwrap();
                    assert_eq!(
                        actual.backend().buffer(),
                        reference.backend().buffer(),
                        "width={width} height={height} top={top} muted={muted}"
                    );
                }
            }
        }
    }
}

fn rail_cells(buffer: &Buffer, hits: &HitMap) -> Vec<(u16, String, ratatui::style::Color)> {
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

#[test]
fn transcript_rail_reserves_the_rightmost_column_in_the_cache_key() {
    let mut app = fixture();
    let mut cache = RenderCache::default();
    let (_, hits) = draw(&app, 100, 30, &mut cache, false);
    let message_width = hits.transcript.width.saturating_sub(TRANSCRIPT_RAIL_WIDTH);
    assert_eq!(
        cache.transcripts.last().unwrap().key.width,
        message_width,
        "the cache key must use the rail-reduced message width"
    );
    assert_eq!(cache.builds, 1);
    let (first, _) = draw(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.builds, 1, "the same reduced width must reuse");
    app.scroll_back(8);
    let (scrolled, _) = draw(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.builds, 1, "scrolling must not reflow history");
    assert_ne!(
        first, scrolled,
        "the scroll must still repaint the viewport"
    );

    let (_, wide_hits) = draw(&app, 101, 30, &mut cache, false);
    assert_eq!(cache.builds, 2, "a wider layout must rebuild");
    assert_eq!(
        cache.transcripts.last().unwrap().key.width,
        wide_hits
            .transcript
            .width
            .saturating_sub(TRANSCRIPT_RAIL_WIDTH)
    );
    draw(&app, 100, 30, &mut cache, false);
    assert_eq!(cache.builds, 2, "the parked width variant must be reused");
}

#[test]
fn transcript_rail_geometry_handles_fit_zero_range_and_overflow() {
    let conversation =
        ConversationId::new(TransportId::new("openbubbles").unwrap(), "rail").unwrap();
    let view = |rows: usize, height: usize, top: usize| crate::transcript::TranscriptViewport {
        conversation: conversation.clone(),
        messages: Vec::new(),
        rows,
        height,
        top,
    };
    let content = Rect::new(0, 0, 20, 10);
    assert!(
        transcript_rail(content, &view(10, 10, 0)).is_none(),
        "exact fit has no range"
    );
    assert!(
        transcript_rail(content, &view(0, 10, 0)).is_none(),
        "no displayed transcript has no rail"
    );
    assert!(
        transcript_rail(Rect::new(0, 0, 20, 1), &view(20, 1, 0)).is_none(),
        "a one-row track cannot hold a thumb"
    );
    assert!(
        transcript_rail(Rect::new(0, 0, 2, 10), &view(20, 10, 0)).is_none(),
        "an unusable message width hides the rail"
    );

    let rail = transcript_rail(content, &view(11, 10, 0)).expect("one row of range");
    assert_eq!(rail.track, Rect::new(19, 0, 1, 10));
    assert_eq!(rail.thumb, Rect::new(19, 0, 1, 9));

    let rail = transcript_rail(content, &view(20, 10, 10)).expect("half-way range");
    assert_eq!(rail.thumb, Rect::new(19, 5, 1, 5));

    let rail = transcript_rail(content, &view(20, 10, usize::MAX)).expect("clamped range");
    assert_eq!(rail.thumb, Rect::new(19, 5, 1, 5));

    let rail = transcript_rail(content, &view(usize::MAX, 10, usize::MAX))
        .expect("overflow-safe arithmetic");
    assert!(rail.thumb.height >= 1 && rail.thumb.height < rail.track.height);
    assert!(rail.thumb.bottom() <= rail.track.bottom());
}

#[test]
fn transcript_rail_keeps_glyphs_in_monochrome() {
    let app = fixture();
    let mut cache = RenderCache::default();
    let (buffer, hits) = draw(&app, 100, 30, &mut cache, true);
    let cells = rail_cells(&buffer, &hits);
    assert!(!cells.is_empty());
    assert!(
        cells.iter().any(|(_, symbol, _)| symbol == "█"),
        "the thumb must remain visible without colour"
    );
    assert!(
        cells.iter().any(|(_, symbol, _)| symbol == "│"),
        "the track must remain visible without colour"
    );
    assert!(
        cells
            .iter()
            .all(|(_, _, fg)| *fg == ratatui::style::Color::Reset),
        "NO_COLOR must reset every rail colour"
    );
}
