use crate::app::{encryption_label, App};
use bloom_core::MembershipState;
use ratatui::{prelude::*, widgets::*};

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width >= 80 {
        let columns = Layout::horizontal([Constraint::Length(28), Constraint::Min(20)]).split(area);
        let rooms = app
            .rooms
            .iter()
            .map(|room| {
                let marker = if Some(room.room_id.to_string()) == app.selected {
                    "> "
                } else {
                    "  "
                };
                let member = match room.membership {
                    MembershipState::Invited => "[invite]",
                    MembershipState::Joined => "[joined]",
                    MembershipState::Left => "[left]",
                };
                Line::from(format!("{marker}{} {member}", room.display_name))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(rooms).block(Block::default().borders(Borders::ALL).title(" Rooms ")),
            columns[0],
        );
        render_detail(frame, columns[1], app);
    } else {
        render_detail(frame, area, app);
    }
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let room = app.selected_room();
    let title = room
        .map(|room| room.display_name.as_str())
        .unwrap_or("No room selected");
    let membership = room
        .map(|room| match room.membership {
            MembershipState::Invited => "Invited",
            MembershipState::Joined => "Joined",
            MembershipState::Left => "Left",
        })
        .unwrap_or("Unknown");
    let encryption = room
        .map(|room| encryption_label(&room.encryption))
        .unwrap_or("Unknown");
    let connection = if app.connection.is_empty() {
        "Unknown"
    } else {
        &app.connection
    };
    let status = app
        .error
        .as_deref()
        .map(|e| format!("Error: {e}"))
        .or_else(|| (!app.status.is_empty()).then(|| app.status.clone()))
        .unwrap_or_else(|| "↑/↓ select  j join invitation  q quit".into());
    if area.height < 11 {
        let composer = format!(
            "Composer: {}",
            if app.can_send() {
                if app.composer_open {
                    &app.draft
                } else {
                    "m to compose"
                }
            } else {
                app.send_reason()
            }
        );
        let lines = match area.height {
            0 => Vec::new(),
            1 => vec![format!("{membership} | {encryption} | {composer}")],
            2 => vec![
                format!("Room: {title}"),
                format!("{membership} | {encryption} | {composer}"),
            ],
            3 => vec![
                format!("Room: {title}"),
                format!("{membership} | {encryption}"),
                composer,
            ],
            4 => vec![
                format!("Room: {title}"),
                format!("{membership} | {encryption}"),
                format!("Connection: {connection}"),
                composer,
            ],
            5 => vec![
                format!("Room: {title}"),
                format!("Membership: {membership}"),
                format!("Encryption: {encryption}"),
                format!("Connection: {connection}"),
                composer,
            ],
            _ => vec![
                format!("Room: {title}"),
                format!("Membership: {membership}"),
                format!("Encryption: {encryption}"),
                format!("Connection: {connection}"),
                status,
                composer,
            ],
        };
        frame.render_widget(Paragraph::new(lines.join("\n")), area);
        return;
    }
    let header_height = 4;
    let composer_height = 3;
    let status_height = if area.height <= 12 { 1 } else { 3 };
    let chunks = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(composer_height),
        Constraint::Length(status_height),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Membership: {membership}\nEncryption: {encryption} | Connection: {connection}"
        ))
        .block(Block::default().borders(Borders::ALL).title(title)),
        chunks[0],
    );
    let messages = room.and_then(|room| app.messages.get(&room.room_id.to_string()));
    let lines = messages
        .into_iter()
        .flatten()
        .map(|message| {
            Line::from(format!(
                "{}  {}: {}",
                message.timestamp_ms,
                message.sender,
                message.body.as_deref().unwrap_or("[non-text message]")
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Transcript ")),
        chunks[1],
    );
    if chunks[2].height > 0 {
        let composer = if app.can_send() {
            format!(
                "{}{}",
                if app.composer_open { "> " } else { "" },
                if app.composer_open {
                    app.draft.as_str()
                } else {
                    "m to compose"
                }
            )
        } else {
            app.send_reason().to_string()
        };
        frame.render_widget(
            Paragraph::new(composer)
                .block(Block::default().borders(Borders::ALL).title(" Composer ")),
            chunks[2],
        );
    }
    if chunks[3].height > 0 {
        let status = app
            .error
            .as_deref()
            .map(|e| format!("Error: {e}"))
            .or_else(|| (!app.status.is_empty()).then(|| app.status.clone()))
            .unwrap_or_else(|| "↑/↓ select  j join invitation  q quit".into());
        let status = Paragraph::new(status);
        if status_height == 1 {
            frame.render_widget(status, chunks[3]);
        } else {
            frame.render_widget(
                status.block(Block::default().borders(Borders::ALL)),
                chunks[3],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn rendered_text(width: u16, height: u16, app: &App) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selected_room(membership: MembershipState, encryption: bloom_core::EncryptionStatus) -> App {
        let room = bloom_core::RoomSummary {
            room_id: "!room:example.org".parse().unwrap(),
            display_name: "Project Room".into(),
            membership,
            encryption,
        };
        let mut app = App::default();
        app.selected = Some(room.room_id.to_string());
        app.rooms.push(room);
        app.connection = "Connected".into();
        app.status = "Invitation ready".into();
        app
    }

    #[test]
    fn narrow_and_wide_views_show_selected_room_state_and_status() {
        for (width, height) in [(70, 12), (100, 30)] {
            let mut app = selected_room(
                MembershipState::Joined,
                bloom_core::EncryptionStatus::NotEncrypted,
            );
            app.messages.insert(
                "!room:example.org".into(),
                vec![bloom_core::Message {
                    event_id: "$event:example.org".into(),
                    sender: "@alice:example.org".into(),
                    body: Some("Visible message body".into()),
                    timestamp_ms: 1,
                    send_state: bloom_core::SendState::Sent,
                }],
            );
            app.error = Some("Sync failed".into());
            let text = rendered_text(width, height, &app);
            for expected in [
                "Project Room",
                "Membership: Joined",
                "Encryption: Not encrypted",
                "Connection: Connected",
                "Error: Sync failed",
            ] {
                assert!(
                    text.contains(expected),
                    "missing {expected:?} at {width}x{height}: {text:?}"
                );
            }
            if width == 70 && height == 12 {
                assert!(
                    text.contains("Visible message body"),
                    "transcript body clipped at 70x12: {text:?}"
                );
            }
        }
        let app = selected_room(
            MembershipState::Invited,
            bloom_core::EncryptionStatus::Unknown,
        );
        let text = rendered_text(70, 12, &app);
        for expected in [
            "Membership: Invited",
            "Encryption: Unknown",
            "Connection: Connected",
            "Invitation ready",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text:?}");
        }
    }

    #[test]
    fn wide_view_shows_joined_unencrypted_room_and_error() {
        let mut app = selected_room(
            MembershipState::Joined,
            bloom_core::EncryptionStatus::NotEncrypted,
        );
        app.error = Some("Sync failed".into());
        let text = rendered_text(100, 30, &app);
        for expected in [
            "Project Room",
            "Membership: Joined",
            "Encryption: Not encrypted",
            "Connection: Connected",
            "Error: Sync failed",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text:?}");
        }
    }

    #[test]
    fn compact_view_shows_composer_and_state_at_five_rows() {
        let joined = selected_room(
            MembershipState::Joined,
            bloom_core::EncryptionStatus::Encrypted,
        );
        let mut joined = joined;
        joined.composer_open = true;
        joined.draft = "Draft message".into();
        let joined_text = rendered_text(70, 5, &joined);
        for expected in [
            "Membership: Joined",
            "Encryption: Encrypted",
            "Connection: Connected",
            "Composer: Draft message",
        ] {
            assert!(
                joined_text.contains(expected),
                "missing {expected:?}: {joined_text:?}"
            );
        }

        let invited = selected_room(
            MembershipState::Invited,
            bloom_core::EncryptionStatus::Unknown,
        );
        let invited_text = rendered_text(70, 5, &invited);
        for expected in [
            "Membership: Invited",
            "Encryption: Unknown",
            "Connection: Connected",
            "Composer: Join the room to send",
        ] {
            assert!(
                invited_text.contains(expected),
                "missing {expected:?}: {invited_text:?}"
            );
        }
    }

    #[test]
    fn compact_short_views_keep_security_and_composer_visible() {
        let app = selected_room(
            MembershipState::Joined,
            bloom_core::EncryptionStatus::Encrypted,
        );
        for (width, height) in [(70, 4), (70, 3), (70, 2), (70, 1)] {
            let text = rendered_text(width, height, &app);
            assert!(
                text.contains("Encrypted"),
                "missing security at {width}x{height}: {text:?}"
            );
            assert!(
                text.contains("to compose"),
                "missing composer at {width}x{height}: {text:?}"
            );
        }
    }

    #[test]
    fn compact_view_preserves_send_reason_at_five_rows_and_status_at_six() {
        let mut app = selected_room(
            MembershipState::Joined,
            bloom_core::EncryptionStatus::Unknown,
        );
        app.error = Some("Sync failed".into());

        let five_rows = rendered_text(70, 5, &app);
        assert!(
            five_rows.contains("Composer: Sending requires confirmed encryption"),
            "missing full send explanation: {five_rows:?}"
        );

        let six_rows = rendered_text(70, 6, &app);
        assert!(six_rows.contains("Error: Sync failed"), "{six_rows:?}");
        assert!(
            six_rows.contains("Composer: Sending requires confirmed encryption"),
            "{six_rows:?}"
        );
    }

    #[test]
    fn compact_view_shows_status_or_error_when_rows_allow() {
        let mut app = selected_room(
            MembershipState::Invited,
            bloom_core::EncryptionStatus::Unknown,
        );
        app.error = Some("Sync failed".into());
        assert!(rendered_text(70, 6, &app).contains("Error: Sync failed"));
        assert!(rendered_text(70, 6, &app).contains("Composer:"));
    }

    #[test]
    fn tiny_dimensions_render_safely() {
        for (width, height) in [(1, 1), (2, 2), (4, 1), (1, 4)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render(frame, &App::default()))
                .unwrap();
        }
    }
}
