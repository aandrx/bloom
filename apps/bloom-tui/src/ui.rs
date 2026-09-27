use crate::app::{chat_name, message_text, participant_count, timestamp, App, ComposerPrompt};
use bloom_domain::DeliveryState;
use ratatui::{prelude::*, widgets::*};

const BG: Color = Color::Rgb(30, 30, 46);
const RULE: Color = Color::Rgb(69, 71, 90);
const TEXT: Color = Color::Rgb(205, 214, 244);
const MUTED: Color = Color::Rgb(166, 173, 200);
const CYAN: Color = Color::Rgb(137, 220, 235);
const GREEN: Color = Color::Rgb(166, 227, 161);

pub fn render(frame: &mut Frame<'_>, app: &App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        frame.area(),
    );
    let areas = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(frame.area());
    let header = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(areas[0]);
    let selected = app.selected_conversation();
    let title = selected.map(chat_name);
    let people = selected
        .map(|c| participant_count(c.participants.len()))
        .unwrap_or_default();
    let mut heading = vec![Span::styled("Bloom", Style::default().fg(TEXT))];
    if let Some(title) = title {
        heading.push(Span::raw(" / "));
        heading.push(Span::styled(
            title,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        if !people.is_empty() {
            heading.push(Span::raw("  "));
            heading.push(Span::styled(people, Style::default().fg(MUTED)));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(heading)), header[0]);
    frame.render_widget(
        Paragraph::new("─".repeat(frame.area().width as usize)).style(Style::default().fg(RULE)),
        header[1],
    );

    let body = Layout::horizontal([
        Constraint::Length(24),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(areas[1]);
    let side = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(body[0]);
    frame.render_widget(
        Paragraph::new("CHATS").style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
        side[0],
    );
    let entries = app
        .conversations
        .iter()
        .map(|c| {
            let selected = Some(&c.id) == app.selected.as_ref();
            let name = chat_name(c);
            let count = participant_count(c.participants.len());
            let marker = if selected { "▎" } else { " " };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(marker, Style::default().fg(CYAN)),
                    Span::styled(
                        name,
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
                    Span::styled(count, Style::default().fg(MUTED)),
                ]),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(entries).style(Style::default().bg(BG)), side[1]);
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(RULE)),
        body[1],
    );

    let right = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(body[2]);
    let mut transcript = Vec::new();
    if let Some(messages) = app.selected.as_ref().and_then(|id| app.messages.get(id)) {
        for message in messages {
            // `from_me` is the authoritative signal; comparing the sender against a
            // placeholder string mislabels our own messages.
            let speaker = if message.from_me {
                "You"
            } else {
                message.sender.value()
            };
            transcript.push(Line::from(vec![
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
                    DeliveryState::Delivered => "delivered",
                    DeliveryState::Read => "read",
                    DeliveryState::Failed => "failed",
                };
                content.push(Span::raw("  "));
                content.push(Span::styled(
                    state,
                    Style::default().fg(if state == "delivered" || state == "read" {
                        GREEN
                    } else {
                        MUTED
                    }),
                ));
            }
            transcript.push(Line::from(content));
            transcript.push(Line::raw(""));
        }
    }
    frame.render_widget(
        Paragraph::new(transcript).wrap(Wrap { trim: false }),
        right[0],
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
        right[1],
    );
    let error = app
        .error
        .as_deref()
        .map(|e| format!("  {e}"))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{}  ·  ↑↓ chats · Enter open · Esc close", app.connection),
                Style::default().fg(MUTED),
            ),
            Span::styled(error, Style::default().fg(GREEN)),
        ])),
        right[2],
    );
}
