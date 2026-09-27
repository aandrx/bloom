use crate::app::{
    chat_name, message_text, participant_count, speaker_label, status_text, status_tone, timestamp,
    App, ComposerPrompt, StatusTone,
};

/// Key hints, exactly as the mockup shows them.
const HINTS: &str = "↑↓ chats · Enter open · / filter · n new · Esc close";
use bloom_domain::DeliveryState;
use ratatui::{prelude::*, widgets::*};

const BG: Color = Color::Rgb(30, 30, 46);
const SURFACE: Color = Color::Rgb(24, 24, 37);
const RULE: Color = Color::Rgb(69, 71, 90);
const TEXT: Color = Color::Rgb(205, 214, 244);
const MUTED: Color = Color::Rgb(166, 173, 200);
const CYAN: Color = Color::Rgb(137, 220, 235);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);

/// The mockup caps its transcript and composer at roughly 780px of a ~1300px panel,
/// about 60%. A terminal has no px, so cap in columns: a full-width transcript was
/// the main reason the UI read as denser than the mockup.
const MAX_CONTENT_WIDTH: u16 = 72;

/// Keep only the trailing lines that fit the available height, so the newest
/// content stays visible. Wrapped rows are counted so nothing is silently clipped.
fn tail_that_fits(lines: Vec<Line<'_>>, width: u16, height: u16) -> Vec<Line<'_>> {
    if height == 0 || lines.is_empty() {
        return Vec::new();
    }
    let width = width.max(1) as usize;
    let height = height as usize;
    let mut used = 0usize;
    let mut start = lines.len();
    for (index, line) in lines.iter().enumerate().rev() {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let rows = wrap_text(&text, width).len().max(1);
        if used + rows > height {
            // Nothing but blank spacers fits above this line, which means the newest
            // message is taller than the whole view. Rendering it whole would clip it
            // from the top and show the start of a long message, or nothing at all
            // when only the spacers after it fit, so show its final rows instead.
            if lines[index + 1..]
                .iter()
                .all(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
            {
                let mut wrapped = wrap_text(&text, width);
                let tail = wrapped.split_off(wrapped.len().saturating_sub(height));
                if !tail.is_empty() {
                    return tail.into_iter().map(Line::raw).collect();
                }
            }
            break;
        }
        used += rows;
        start = index;
    }
    lines.into_iter().skip(start).collect()
}

/// Wrap text into display rows the way the transcript paragraph does, breaking at
/// whitespace and splitting a word only when it cannot fit on a row by itself.
/// The count is what the paragraph will actually occupy, so the tail can be
/// chosen without clipping the oldest visible line.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for word in text
        .split_inclusive(char::is_whitespace)
        .filter(|word| !word.is_empty())
    {
        let word_width = unicode_width::UnicodeWidthStr::width(word);
        if used > 0 && used + word_width > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if word_width > width {
            for character in word.chars() {
                let character_width =
                    unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
                if used + character_width > width {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(character);
                used += character_width;
            }
            continue;
        }
        row.push_str(word);
        used += word_width;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}

fn display_truncate(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + char_width > width {
            break;
        }
        result.push(character);
        used += char_width;
    }
    result
}

fn display_pad(value: &str, width: usize) -> String {
    let mut result = display_truncate(value, width);
    let used = unicode_width::UnicodeWidthStr::width(result.as_str());
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default()
            .style(Style::default().bg(SURFACE).fg(TEXT))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(RULE)),
        area,
    );
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    // A terminal can be arbitrarily small. Below the size where the layout is
    // meaningful, show a compact fallback instead of asking ratatui for regions
    // outside the buffer, which panics.
    if inner.width < 20 || inner.height < 5 {
        frame.render_widget(
            Paragraph::new(format!("Bloom  {}", app.connection))
                .style(Style::default().fg(TEXT).bg(SURFACE)),
            inner,
        );
        return;
    }

    // The mockup's sidebar is roughly a sixth of the panel, so size it as a
    // proportion with sane bounds rather than a fixed 24 columns.
    let sidebar_width = ((inner.width / 6).clamp(14, 22)).min(inner.width.saturating_sub(3));
    let body = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    // The mockup letter-spaces this label. A terminal cannot set tracking, so the
    // spaces are inserted explicitly, and the label stays muted rather than bold so
    // it reads as a quiet section label.
    frame.render_widget(
        Paragraph::new("C H A T S").style(Style::default().fg(MUTED)),
        Rect::new(body[0].x, body[0].y, body[0].width, 1),
    );
    let visible = app.visible_conversations();
    let entries = visible
        .iter()
        .map(|c| {
            let selected = Some(&c.id) == app.selected.as_ref();
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(if selected { "▎" } else { " " }, Style::default().fg(CYAN)),
                    Span::styled(
                        chat_name(c),
                        Style::default()
                            .fg(if selected { CYAN } else { TEXT })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]),
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        participant_count(c.participants.len()),
                        Style::default().fg(MUTED),
                    ),
                ]),
            ])
        })
        .collect::<Vec<_>>();
    // Render with state so a selection deep in a long list scrolls into view
    // rather than moving off-screen invisibly.
    let selected_index = app.selected.as_ref().and_then(|id| {
        visible
            .iter()
            .position(|conversation| &conversation.id == id)
    });
    let mut list_state = ListState::default();
    list_state.select(selected_index);
    frame.render_stateful_widget(
        List::new(entries).style(Style::default().bg(BG)),
        Rect::new(
            body[0].x,
            body[0].y + 1,
            body[0].width,
            body[0].height.saturating_sub(1),
        ),
        &mut list_state,
    );

    // Filter feedback replaces the plain label so the state is never ambiguous.
    if let Some(query) = app.filter_query() {
        let label = if query.trim().is_empty() {
            "FILTER".to_owned()
        } else if app.filter_has_no_matches() {
            // Short enough to survive the narrow sidebar; the query is already
            // visible in the filter prompt.
            "NO MATCHES".to_owned()
        } else {
            match visible.len() {
                1 => "1 MATCH".to_owned(),
                count => format!("{count} MATCHES"),
            }
        };
        let width =
            (body[0].width as usize).min(unicode_width::UnicodeWidthStr::width(label.as_str()));
        frame.render_widget(
            Paragraph::new(display_truncate(&label, width))
                .style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            Rect::new(body[0].x, body[0].y, body[0].width, 1),
        );
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(RULE)),
        body[1],
    );
    // Shrink the header and composer blocks on short terminals so their heights can
    // never exceed the area they are given.
    let (header_height, composer_height) = if inner.height >= 10 {
        (2u16, 3u16)
    } else {
        (1u16, 1u16)
    };
    let right = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(composer_height),
    ])
    .split(body[2]);
    // The mockup gives the transcript clear separation from the sidebar divider.
    let gutter = ratatui::layout::Margin {
        horizontal: 3,
        vertical: 0,
    };
    let cap = |area: Rect| Rect {
        width: area.width.min(MAX_CONTENT_WIDTH),
        ..area
    };
    let content = cap(right[1].inner(gutter));
    let composer = cap(right[2].inner(gutter));
    let selected = app.selected_conversation();
    let mut heading = vec![Span::styled("Bloom", Style::default().fg(TEXT))];
    if let Some(c) = selected {
        heading.extend([
            Span::raw(" / "),
            Span::styled(
                chat_name(c),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                participant_count(c.participants.len()),
                Style::default().fg(MUTED),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(heading)),
        Rect::new(right[0].x, right[0].y, right[0].width, 1),
    );
    // The mockup keeps a status badge in the header rather than in the hint line.
    // On narrow terminals the identity needs the room more than the badge does.
    if inner.width >= 60 {
        if let Some(status) = status_text(app) {
            let tone = status_tone(app);
            let label = format!(" {status} ");
            let width = label.chars().count() as u16 + 2;
            frame.render_widget(
                Paragraph::new(label)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(match tone {
                        StatusTone::Healthy => MUTED,
                        StatusTone::Attention => YELLOW,
                        StatusTone::Failure => RED,
                    }))
                    .block(
                        Block::default()
                            .borders(Borders::LEFT | Borders::RIGHT)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(RULE)),
                    ),
                Rect::new(
                    right[0].x + right[0].width.saturating_sub(width + 1),
                    right[0].y,
                    width,
                    1,
                ),
            );
        }
    }
    frame.render_widget(
        Paragraph::new("─".repeat(right[0].width as usize)).style(Style::default().fg(RULE)),
        Rect::new(right[0].x, right[0].y + 1, right[0].width, 1),
    );
    let mut lines = Vec::new();
    if let Some(messages) = app.selected.as_ref().and_then(|id| app.messages.get(id)) {
        // Pad speaker names to a common column so timestamps line up, the way the
        // mockup's transcript does. Capped so one long name cannot push the times
        // off the visible line.
        const SPEAKER_COLUMN_CAP: usize = 12;
        let speaker_column = messages
            .iter()
            .map(|message| {
                let label = speaker_label(message, app.selected_conversation());
                unicode_width::UnicodeWidthStr::width(label.as_str())
            })
            .max()
            .unwrap_or(0)
            .clamp(1, SPEAKER_COLUMN_CAP);

        for message in messages {
            let speaker = speaker_label(message, app.selected_conversation());
            let padded = display_pad(&speaker, speaker_column);
            lines.push(Line::from(vec![
                Span::styled(
                    padded,
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(timestamp(message.created_at), Style::default().fg(MUTED)),
            ]));
            let mut content = vec![Span::styled(
                message_text(message),
                Style::default().fg(TEXT),
            )];
            if message.from_me {
                let state = match message.delivery {
                    DeliveryState::Queued => "sending",
                    DeliveryState::Sent => "sent",
                    DeliveryState::Delivered => "✓ delivered",
                    DeliveryState::Read => "✓ read",
                    DeliveryState::Failed => "failed",
                };
                content.extend([
                    Span::raw("  "),
                    Span::styled(
                        state,
                        Style::default().fg(if state.starts_with('✓') {
                            GREEN
                        } else {
                            MUTED
                        }),
                    ),
                ]);
            }
            lines.push(Line::from(content));
            lines.push(Line::raw(""));
            lines.push(Line::raw(""));
        }
    }
    // Show the tail of the thread: a long conversation must display its newest
    // messages, not its oldest.
    let fitted = tail_that_fits(lines, content.width, content.height);
    frame.render_widget(
        Paragraph::new(fitted)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(SURFACE)),
        content,
    );
    let prompt = match app.composer_prompt() {
        ComposerPrompt::Recipient => "›  To: recipient address...".to_owned(),
        ComposerPrompt::Body(address) => format!("›  To {address} — write a message..."),
        _ => "›  Write a message...".to_owned(),
    };
    let prompt = if app.new_conversation && app.pending_address.is_none() {
        "›  To: recipient address...".to_owned()
    } else if let Some(address) = &app.pending_address {
        format!("›  To {address} — write a message... {}", app.draft)
    } else if app.composing {
        format!("›  {}", app.draft)
    } else {
        prompt
    };
    let (prompt, prompt_text) = match app.filter_query() {
        Some(query) => (format!("›  filter: {query}"), query.to_owned()),
        None => (prompt, app.draft.clone()),
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if prompt_text.is_empty() { MUTED } else { TEXT }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(RULE)),
            ),
        Rect::new(composer.x, composer.y, composer.width, 2),
    );
    // Shorten the hints rather than truncating them mid-word on a narrow terminal.
    let hints = if composer.width >= HINTS.chars().count() as u16 {
        HINTS
    } else {
        "↑↓ · / · Esc"
    };
    // A failure has to be visible and must say what went wrong; a silent send
    // failure is worse than an ugly line. It takes the hint row while it lasts.
    let (footer, footer_style) = match app.error.as_deref() {
        Some(message) => (message.to_owned(), Style::default().fg(RED)),
        None => (hints.to_owned(), Style::default().fg(MUTED)),
    };
    frame.render_widget(
        Paragraph::new(footer).style(footer_style),
        Rect::new(composer.x, composer.y + 2, composer.width, 1),
    );
}
