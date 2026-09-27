use crate::app::{
    chat_name, message_text, participant_count, speaker_label, status_text, timestamp, App,
    ComposerPrompt,
};

/// Key hints, exactly as the mockup shows them.
const HINTS: &str = "↑↓ chats · Enter open · Esc close";
use bloom_domain::DeliveryState;
use ratatui::{prelude::*, widgets::*};

const BG: Color = Color::Rgb(30, 30, 46);
const SURFACE: Color = Color::Rgb(24, 24, 37);
const RULE: Color = Color::Rgb(69, 71, 90);
const TEXT: Color = Color::Rgb(205, 214, 244);
const MUTED: Color = Color::Rgb(166, 173, 200);
const CYAN: Color = Color::Rgb(137, 220, 235);
const GREEN: Color = Color::Rgb(166, 227, 161);

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

    // Keep the sidebar inside the available width and leave room for the divider
    // and at least one column of transcript.
    let sidebar_width = 24.min(inner.width.saturating_sub(2));
    let body = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new("CHATS").style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
        Rect::new(body[0].x, body[0].y, body[0].width, 1),
    );
    let entries = app
        .conversations
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
    frame.render_widget(
        List::new(entries).style(Style::default().bg(BG)),
        Rect::new(
            body[0].x,
            body[0].y + 1,
            body[0].width,
            body[0].height.saturating_sub(1),
        ),
    );
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
        horizontal: 2,
        vertical: 0,
    };
    let content = right[1].inner(gutter);
    let composer = right[2].inner(gutter);
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
    if let Some(status) = status_text(app) {
        let width = status.chars().count() as u16 + 1;
        frame.render_widget(
            Paragraph::new(status)
                .alignment(Alignment::Right)
                .style(Style::default().fg(MUTED)),
            Rect::new(
                right[0].x + right[0].width.saturating_sub(width),
                right[0].y,
                width,
                1,
            ),
        );
    }
    frame.render_widget(
        Paragraph::new("─".repeat(right[0].width as usize)).style(Style::default().fg(RULE)),
        Rect::new(right[0].x, right[0].y + 1, right[0].width, 1),
    );
    let mut lines = Vec::new();
    if let Some(messages) = app.selected.as_ref().and_then(|id| app.messages.get(id)) {
        for message in messages {
            let speaker = speaker_label(message, app.selected_conversation());
            lines.push(Line::from(vec![
                Span::styled(
                    speaker,
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
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
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
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if app.draft.is_empty() { MUTED } else { TEXT }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(RULE)),
            ),
        Rect::new(composer.x, composer.y, composer.width, 2),
    );
    frame.render_widget(
        Paragraph::new(HINTS).style(Style::default().fg(MUTED)),
        Rect::new(composer.x, composer.y + 2, composer.width, 1),
    );
}
