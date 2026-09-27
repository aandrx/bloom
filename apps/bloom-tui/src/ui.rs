use crate::app::{
    chat_name, message_text, participant_count, speaker_label, status_text, status_tone, timestamp,
    App, ComposerPrompt, StatusTone, Theme,
};

/// Key hints, exactly as the mockup shows them.
const HINTS: &str = "↑↓ chats · Enter open · / filter · n new · Esc close · t theme";
use bloom_domain::DeliveryState;
use ratatui::{prelude::*, widgets::*};

#[derive(Clone, Copy)]
struct Palette {
    bg: Color,
    surface: Color,
    rule: Color,
    text: Color,
    muted: Color,
    cyan: Color,
    green: Color,
    yellow: Color,
    red: Color,
}

const DARK: Palette = Palette {
    bg: Color::Rgb(30, 30, 46),
    surface: Color::Rgb(24, 24, 37),
    rule: Color::Rgb(69, 71, 90),
    text: Color::Rgb(205, 214, 244),
    muted: Color::Rgb(166, 173, 200),
    cyan: Color::Rgb(137, 220, 235),
    green: Color::Rgb(166, 227, 161),
    yellow: Color::Rgb(249, 226, 175),
    red: Color::Rgb(243, 139, 168),
};
const LIGHT: Palette = Palette {
    bg: Color::Rgb(255, 255, 255),
    surface: Color::Rgb(255, 255, 255),
    rule: Color::Rgb(199, 199, 199),
    text: Color::Rgb(0, 0, 0),
    muted: Color::Rgb(127, 127, 127),
    cyan: Color::Rgb(0, 117, 138),
    green: Color::Rgb(40, 117, 47),
    yellow: Color::Rgb(128, 78, 0),
    red: Color::Rgb(160, 30, 45),
};

/// The mockup caps its transcript and composer at roughly 780px of a ~1300px panel,
/// about 60%. A terminal has no px, so cap in columns: a full-width transcript was
/// the main reason the UI read as denser than the mockup.
const MAX_CONTENT_WIDTH: u16 = 72;

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
    let palette = match app.theme {
        Theme::Dark => DARK,
        Theme::Light => LIGHT,
    };
    let area = frame.area();
    frame.render_widget(
        Block::default()
            .style(Style::default().bg(palette.surface).fg(palette.text))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.rule)),
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
                .style(Style::default().fg(palette.text).bg(palette.surface)),
            inner,
        );
        return;
    }

    // Measured from the mockup: the rail is 287px of the 1366px window, about 21%,
    // so size it as that proportion with sane bounds rather than a fixed column
    // count. The transcript keeps its own width cap, so the rail only takes room
    // that was empty in the conversation column.
    let desired = (inner.width as u32 * 21 + 50) / 100;
    // The floor keeps the widest chat label the fixtures use (` Design systems`, with
    // its selection marker) from being clipped when the proportional width rounds down.
    let sidebar_width = desired
        .clamp(16, 28)
        .min(inner.width.saturating_sub(3) as u32) as u16;
    let body = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let visible = app.visible_conversations();
    // The mockup letter-spaces this label. A terminal cannot set tracking, so the
    // spaces are inserted explicitly, and the label stays muted rather than bold so
    // it reads as a quiet section label.
    //
    // An active filter replaces the label rather than drawing over it. Both used to
    // render into this row, and the shorter filter text left the tail of the plain
    // label behind, so `1 MATCH` appeared as `1 MATCH S`.
    let (label, is_filter) = match app.filter_query() {
        Some(query) if query.trim().is_empty() => ("FILTER".to_owned(), true),
        Some(_) if app.filter_has_no_matches() => {
            // Short enough to survive the narrow sidebar; the query itself is already
            // visible in the composer prompt.
            ("NO MATCHES".to_owned(), true)
        }
        Some(_) => match visible.len() {
            1 => ("1 MATCH".to_owned(), true),
            count => (format!("{count} MATCHES"), true),
        },
        None => ("C H A T S".to_owned(), false),
    };
    let label_style = if is_filter {
        Style::default()
            .fg(palette.cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };
    let label_width =
        (body[0].width as usize).min(unicode_width::UnicodeWidthStr::width(label.as_str()));
    frame.render_widget(
        Paragraph::new(display_truncate(&label, label_width)).style(label_style),
        Rect::new(body[0].x, body[0].y, body[0].width, 1),
    );
    let entries = visible
        .iter()
        .map(|c| {
            let selected = Some(&c.id) == app.selected.as_ref();
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        if selected { "▎" } else { " " },
                        Style::default().fg(palette.cyan),
                    ),
                    Span::styled(
                        chat_name(c),
                        Style::default()
                            .fg(if selected { palette.cyan } else { palette.text })
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
                        Style::default().fg(palette.muted),
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
        List::new(entries).style(Style::default().bg(palette.bg)),
        Rect::new(
            body[0].x,
            body[0].y + 1,
            body[0].width,
            body[0].height.saturating_sub(1),
        ),
        &mut list_state,
    );

    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(palette.rule)),
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
    let mut heading = vec![Span::styled("Bloom", Style::default().fg(palette.text))];
    if let Some(c) = selected {
        heading.extend([
            Span::raw(" / "),
            Span::styled(
                chat_name(c),
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                participant_count(c.participants.len()),
                Style::default().fg(palette.muted),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(heading)),
        Rect::new(right[0].x, right[0].y, right[0].width, 1),
    );
    // Status is only surfaced when it needs attention; the theme control stays visible
    // whenever the header has room for the identity plus the control, which it does
    // from roughly 50 columns up.
    if right[0].width >= 38 {
        let mut x = right[0].x + right[0].width;
        let theme_label = match app.theme {
            Theme::Dark => " Dark ",
            Theme::Light => " Light ",
        };
        let theme_width = theme_label.chars().count() as u16 + 2;
        // One column of breathing room before the frame edge, as the status chip had
        // and as the mockup's control has inside its header.
        x = x.saturating_sub(theme_width + 1);
        frame.render_widget(
            Paragraph::new(theme_label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted))
                .block(
                    Block::default()
                        .borders(Borders::LEFT | Borders::RIGHT)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.rule)),
                ),
            Rect::new(x, right[0].y, theme_width, 1),
        );
        if status_tone(app) != StatusTone::Healthy {
            if let Some(status) = status_text(app) {
                let tone = status_tone(app);
                let label = format!(" {status} ");
                let width = label.chars().count() as u16 + 2;
                x = x.saturating_sub(width + 1);
                frame.render_widget(
                    Paragraph::new(label)
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(match tone {
                            StatusTone::Healthy => palette.muted,
                            StatusTone::Attention => palette.yellow,
                            StatusTone::Failure => palette.red,
                        }))
                        .block(
                            Block::default()
                                .borders(Borders::LEFT | Borders::RIGHT)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(palette.rule)),
                        ),
                    Rect::new(x, right[0].y, width, 1),
                );
            }
        }
    }
    frame.render_widget(
        Paragraph::new("─".repeat(right[0].width as usize))
            .style(Style::default().fg(palette.rule)),
        Rect::new(right[0].x, right[0].y + 1, right[0].width, 1),
    );
    let mut lines = Vec::new();
    let mut header_lines: Vec<usize> = Vec::new();
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
            // Remember where each byline lands, so the viewport can avoid ending on one
            // with its body below the fold.
            header_lines.push(lines.len());
            lines.push(Line::from(vec![
                Span::styled(
                    padded,
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    timestamp(message.created_at),
                    Style::default().fg(palette.muted),
                ),
            ]));
            let mut content = vec![Span::styled(
                message_text(message),
                Style::default().fg(palette.text),
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
                            palette.green
                        } else {
                            palette.muted
                        }),
                    ),
                ]);
            }
            lines.push(Line::from(content));
            lines.push(Line::raw(""));
            lines.push(Line::raw(""));
        }
    }
    // Show the newest messages by default and let the user scroll back. Rows are
    // counted with the same wrap the paragraph performs, so the offset lands on
    // the row the renderer will actually use; scroll 0 pins to the newest content
    // and an over-height message therefore shows its tail rather than vanishing.
    let width = content.width.max(1) as usize;
    let mut line_starts: Vec<usize> = Vec::with_capacity(lines.len());
    let mut rows = 0usize;
    for line in &lines {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let count = wrap_text(&text, width).len().max(1);
        line_starts.push(rows);
        rows += count;
    }
    let max_scroll = rows.saturating_sub(content.height as usize);
    // `Paragraph::scroll` offsets from the top, so a reading position of zero rows
    // back from the newest message is the bottom of the transcript.
    let mut back = app.scroll.min(max_scroll);
    // A byline on the last visible row reads as a fault: the header is shown but its
    // message is not. Step one row so the row above it ends the view; at the very top
    // of the transcript there is nothing older to show, so step towards the newer
    // side instead and reveal the body.
    if content.height > 0 {
        let last_row = max_scroll.saturating_sub(back) + content.height as usize - 1;
        let line = line_starts
            .partition_point(|start| *start <= last_row)
            .saturating_sub(1);
        if header_lines.contains(&line) && line_starts.get(line) == Some(&last_row) {
            if back < max_scroll {
                back += 1;
            } else {
                back = back.saturating_sub(1);
            }
        }
    }
    let scroll = max_scroll.saturating_sub(back).min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .style(Style::default().bg(palette.surface)),
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
            .style(Style::default().fg(if prompt_text.is_empty() {
                palette.muted
            } else {
                palette.text
            }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(palette.rule)),
            ),
        Rect::new(composer.x, composer.y, composer.width, 2),
    );
    // Shorten the hints rather than truncating them mid-word on a narrow terminal,
    // but keep the theme key: the control it toggles is the only other hint of it.
    let hints = if composer.width >= HINTS.chars().count() as u16 {
        HINTS
    } else if composer.width >= 24 {
        "↑↓ · / · t theme · Esc"
    } else {
        "↑↓ · / · Esc"
    };
    // A failure has to be visible and must say what went wrong; a silent send
    // failure is worse than an ugly line. It takes the hint row while it lasts.
    let (footer, footer_style) = match app.error.as_deref() {
        Some(message) => (message.to_owned(), Style::default().fg(palette.red)),
        // Reading back is a state the mockup does not show, so rather than change
        // its hint row this explains how to leave that state when it happens.
        None if app.is_scrolled_back() => (
            "↑ reading back · End returns to the newest".to_owned(),
            Style::default().fg(palette.cyan),
        ),
        None => (hints.to_owned(), Style::default().fg(palette.muted)),
    };
    frame.render_widget(
        Paragraph::new(footer).style(footer_style),
        Rect::new(composer.x, composer.y + 2, composer.width, 1),
    );
}
