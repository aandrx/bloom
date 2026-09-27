use crate::app::{message_text, App};
use ratatui::{prelude::*, widgets::*};
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let cols =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(20)]).split(frame.area());
    let items = app
        .conversations
        .iter()
        .map(|c| {
            let marker = if Some(&c.id) == app.selected.as_ref() {
                "> "
            } else {
                "  "
            };
            let name = c.title.as_deref().unwrap_or_else(|| {
                c.participants
                    .first()
                    .and_then(|p| p.display_name.as_deref())
                    .unwrap_or("Conversation")
            });
            let preview = app.preview(&c.id);
            ListItem::new(format!("{marker}{name}\n  {preview}"))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Conversations "),
        ),
        cols[0],
    );
    let title = app
        .selected_conversation()
        .and_then(|c| c.title.as_deref())
        .unwrap_or("Thread");
    let messages = app.selected.as_ref().and_then(|id| app.messages.get(id));
    let lines = messages
        .into_iter()
        .flatten()
        .map(|m| {
            Line::from(format!(
                "{} {}: {} [{:?}]",
                m.created_at,
                m.sender.value(),
                message_text(m),
                m.delivery
            ))
        })
        .collect::<Vec<_>>();
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(cols[1]);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(if app.composing {
            format!("> {}", app.draft)
        } else {
            "Enter to compose".into()
        })
        .block(Block::default().borders(Borders::ALL).title(" Message ")),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{} | Session: {}{}",
            app.connection,
            app.session,
            app.error
                .as_ref()
                .map(|e| format!(" | {e}"))
                .unwrap_or_default()
        )),
        chunks[2],
    );
}
