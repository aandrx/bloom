use crate::app::{
    conversation_subtitle, message_text, status_text, status_tone, timestamp, App, ComposerPrompt,
    Focus, SearchParticipantCandidate, SearchState, StatusTone, Theme, SEARCH_RESULT_LIMIT,
};

use bloom_domain::{Conversation, ConversationId, DeliveryState, HandleKind, Message, MessageId};
use bloom_ipc::ParticipantFilter;
use ratatui::{prelude::*, widgets::*};
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
#[path = "ui_cache_tests.rs"]
mod cache_tests;

fn monochrome_from(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn monochrome() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| monochrome_from(std::env::var_os("NO_COLOR")))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Palette {
    bg: Color,
    surface: Color,
    input_surface: Color,
    rule: Color,
    text: Color,
    muted: Color,
    cyan: Color,
    green: Color,
    yellow: Color,
    red: Color,
    /// A third speaker colour, so sender names separate at a glance.
    magenta: Color,
    selection: Color,
}

const DARK: Palette = Palette {
    bg: Color::Rgb(24, 24, 24),
    surface: Color::Rgb(20, 20, 20),
    input_surface: Color::Rgb(38, 38, 38),
    rule: Color::Rgb(65, 65, 65),
    text: Color::Rgb(230, 230, 230),
    muted: Color::Rgb(163, 163, 163),
    cyan: Color::Rgb(147, 197, 253),
    green: Color::Rgb(134, 199, 151),
    yellow: Color::Rgb(226, 190, 113),
    magenta: Color::Rgb(190, 175, 218),
    red: Color::Rgb(239, 143, 143),
    selection: Color::Rgb(52, 52, 52),
};
const LIGHT: Palette = Palette {
    bg: Color::Rgb(255, 255, 255),
    surface: Color::Rgb(250, 250, 250),
    input_surface: Color::Rgb(239, 239, 239),
    rule: Color::Rgb(199, 199, 199),
    text: Color::Rgb(0, 0, 0),
    muted: Color::Rgb(104, 104, 104),
    cyan: Color::Rgb(0, 117, 138),
    green: Color::Rgb(40, 117, 47),
    yellow: Color::Rgb(128, 78, 0),
    magenta: Color::Rgb(130, 60, 170),
    red: Color::Rgb(160, 30, 45),
    selection: Color::Rgb(220, 230, 242),
};

const COMFORTABLE_RAIL_MIN_WIDTH: u16 = 70;

pub fn uses_chat_drawer(width: u16) -> bool {
    width < COMFORTABLE_RAIL_MIN_WIDTH
}

fn sidebar_width_for(width: u16, compact_sidebar: bool) -> u16 {
    if uses_chat_drawer(width) {
        return 0;
    }
    let rail_percent = if compact_sidebar { 15 } else { 25 };
    let desired = (u32::from(width) * rail_percent + 50) / 100;
    desired.clamp(16, 34).min(width.saturating_sub(3) as u32) as u16
}

/// Usable composer columns after the responsive chat rail and text gutters.
pub fn composer_columns(width: u16, compact_sidebar: bool) -> usize {
    if width < 20 {
        return usize::from(width.max(1));
    }
    usize::from(
        width
            .saturating_sub(
                sidebar_width_for(width, compact_sidebar) + u16::from(!uses_chat_drawer(width)) + 4,
            )
            .max(1),
    )
}

/// Prompt width used by the draft's vertical cursor movement.
pub fn composer_prefix_columns(app: &App, width: usize) -> usize {
    let (prefix, _, _, editable, _) = composer_content(app, width);
    let available = width.saturating_sub(if editable { width.min(10) } else { 0 });
    unicode_width::UnicodeWidthStr::width(truncate_display_graphemes(&prefix, available).as_str())
}

#[derive(Clone)]
struct ComposerLine {
    text: String,
    indicator: bool,
}

#[derive(Clone)]
struct ComposerLayout {
    input: Rect,
    lines: Vec<ComposerLine>,
    cursor: Option<(u16, u16)>,
    cursor_positions: Vec<(u16, u16, usize)>,
}

impl ComposerLayout {
    fn new(
        rect: Rect,
        prefix: &str,
        value: &str,
        cursor_byte: usize,
        editable: bool,
        wrap: bool,
    ) -> Self {
        let width = usize::from(rect.width);
        let prefix_limit = width.saturating_sub(if editable { width.min(10) } else { 0 });
        let visible_prefix = truncate_display_graphemes(prefix, prefix_limit);
        let prefix_width = unicode_width::UnicodeWidthStr::width(visible_prefix.as_str());
        let graphemes: Vec<(usize, &str)> = value.grapheme_indices(true).collect();
        let cursor_byte = cursor_byte.min(value.len());
        let cursor_index = graphemes.partition_point(|(start, _)| *start < cursor_byte);

        if wrap && editable && width > 0 {
            let wrapped = crate::composer::wrap_draft(&visible_prefix, value, width);
            let (start, end, top, bottom, combined) = composer_window(
                wrapped.lines.len(),
                wrapped.positions[cursor_index].0,
                rect.height.max(1) as usize,
            );
            let mut lines = Vec::new();
            if combined {
                lines.push(ComposerLine {
                    text: truncate_with_ellipsis(
                        &format!("↑ {start} more · ↓ {} more", wrapped.lines.len() - end),
                        width,
                    ),
                    indicator: true,
                });
            } else if top {
                lines.push(ComposerLine {
                    text: truncate_with_ellipsis(&format!("↑ {start} more"), width),
                    indicator: true,
                });
            }
            lines.extend(
                wrapped.lines[start..end]
                    .iter()
                    .cloned()
                    .map(|text| ComposerLine {
                        text,
                        indicator: false,
                    }),
            );
            if bottom && !combined {
                lines.push(ComposerLine {
                    text: truncate_with_ellipsis(
                        &format!("↓ {} more", wrapped.lines.len() - end),
                        width,
                    ),
                    indicator: true,
                });
            }

            let input = Rect::new(
                rect.x,
                rect.y,
                rect.width,
                lines.len().min(u16::MAX as usize) as u16,
            );
            let indicator_rows = usize::from(top || combined);
            let cursor_line = wrapped.positions[cursor_index].0;
            let cursor_column = wrapped.positions[cursor_index].1;
            let indent = prefix_width.min(width.saturating_sub(1));
            let cursor = (rect.width > 0
                && cursor_line >= start
                && cursor_line < end
                && cursor_line < wrapped.lines.len())
            .then(|| {
                let base = if cursor_line == 0 {
                    prefix_width
                } else {
                    indent
                };
                let line = indicator_rows + cursor_line - start;
                (
                    rect.x
                        .saturating_add(base.saturating_add(cursor_column).min(width - 1) as u16),
                    rect.y.saturating_add(line as u16),
                )
            });
            let mut cursor_positions = Vec::new();
            if rect.width > 0 {
                for (index, (line, column)) in wrapped.positions.iter().copied().enumerate() {
                    if line < start || line >= end || line >= wrapped.lines.len() {
                        continue;
                    }
                    let base = if line == 0 { prefix_width } else { indent };
                    let display_line = indicator_rows + line - start;
                    cursor_positions.push((
                        rect.x
                            .saturating_add(base.saturating_add(column).min(width - 1) as u16),
                        rect.y.saturating_add(display_line as u16),
                        index,
                    ));
                }
            }
            return Self {
                input,
                lines,
                cursor,
                cursor_positions,
            };
        }

        let input = Rect::new(rect.x, rect.y, rect.width, 1.min(rect.height));
        let available = width.saturating_sub(prefix_width);
        let mut start_index = 0;
        let mut cursor_width = graphemes[..cursor_index]
            .iter()
            .map(|(_, grapheme)| unicode_width::UnicodeWidthStr::width(*grapheme))
            .sum::<usize>();
        while available > 0 && cursor_width >= available && start_index < cursor_index {
            cursor_width = cursor_width.saturating_sub(unicode_width::UnicodeWidthStr::width(
                graphemes[start_index].1,
            ));
            start_index += 1;
        }
        let mut visible_value = String::new();
        let mut visible_count = 0usize;
        let mut used = 0usize;
        for (_, grapheme) in graphemes.iter().skip(start_index) {
            let grapheme_width = unicode_width::UnicodeWidthStr::width(*grapheme);
            if used + grapheme_width > available {
                break;
            }
            visible_value.push_str(grapheme);
            used += grapheme_width;
            visible_count += 1;
        }
        let mut prompt = visible_prefix;
        prompt.push_str(&visible_value);
        let cursor = (editable && rect.width > 0).then(|| {
            let column = prefix_width.saturating_add(cursor_width);
            let x = rect
                .x
                .saturating_add(column.min(width.saturating_sub(1)) as u16);
            (x, rect.y)
        });
        let mut cursor_positions = Vec::new();
        if editable && rect.width > 0 {
            let mut column = prefix_width;
            for (index, grapheme_entry) in graphemes
                .iter()
                .enumerate()
                .skip(start_index)
                .take(visible_count + 1)
            {
                cursor_positions.push((
                    rect.x
                        .saturating_add(column.min(width.saturating_sub(1)) as u16),
                    rect.y,
                    index,
                ));
                if index < start_index + visible_count {
                    column += unicode_width::UnicodeWidthStr::width(grapheme_entry.1);
                }
            }
        }
        Self {
            input,
            lines: vec![ComposerLine {
                text: prompt,
                indicator: false,
            }],
            cursor,
            cursor_positions,
        }
    }
}

fn render_composer_layout(
    frame: &mut Frame<'_>,
    layout: &ComposerLayout,
    muted: bool,
    palette: Palette,
    surface: Color,
) {
    let lines = layout
        .lines
        .iter()
        .map(|row| {
            let foreground = if muted || row.indicator {
                palette.muted
            } else {
                palette.text
            };
            Line::from(Span::styled(
                row.text.clone(),
                Style::default().fg(foreground).bg(surface),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(surface)),
        layout.input,
    );
    if let Some(cursor) = layout.cursor {
        frame.set_cursor_position(cursor);
    }
}

fn composer_window(total: usize, cursor: usize, budget: usize) -> (usize, usize, bool, bool, bool) {
    if total <= budget {
        return (0, total, false, false, false);
    }
    if budget <= 1 {
        let start = cursor.min(total.saturating_sub(1));
        return (start, start + 1, false, false, false);
    }
    if budget == 2 {
        let start = cursor.min(total.saturating_sub(1));
        let end = start + 1;
        let top = start > 0;
        let bottom = end < total;
        return (start, end, top, bottom, top && bottom);
    }
    let near_top_slots = budget - 1;
    if cursor < near_top_slots {
        let end = near_top_slots.min(total);
        return (0, end, false, end < total, false);
    }
    if cursor >= total.saturating_sub(near_top_slots) {
        let start = total.saturating_sub(near_top_slots);
        return (start, total, start > 0, false, false);
    }
    let text_slots = budget - 2;
    let start = cursor
        .saturating_sub(text_slots / 2)
        .min(total.saturating_sub(text_slots));
    let end = (start + text_slots).min(total);
    (start, end, start > 0, end < total, false)
}

fn composer_content(app: &App, width: usize) -> (String, String, usize, bool, bool) {
    if app.settings_open || app.theme_panel_open {
        return ("›  ".to_owned(), String::new(), 0, false, true);
    }
    if app.contacts_open {
        if app.contacts_editing.is_some() {
            return (
                "›  nickname: ".to_owned(),
                app.draft.clone(),
                app.draft_cursor_byte(),
                true,
                app.draft.is_empty(),
            );
        }
        return ("›  ".to_owned(), String::new(), 0, false, true);
    }
    if app.renaming.is_some() {
        return (
            "›  rename: ".to_owned(),
            app.draft.clone(),
            app.draft_cursor_byte(),
            true,
            false,
        );
    }
    if let Some(query) = app.filter_query() {
        return (
            "›  filter: ".to_owned(),
            query.to_owned(),
            query.len(),
            true,
            query.is_empty(),
        );
    }
    match app.composer_prompt() {
        ComposerPrompt::Recipient => (
            "›  To: ".to_owned(),
            if app.draft.is_empty() {
                if width < 24 {
                    "Recipient".to_owned()
                } else {
                    "recipient address...".to_owned()
                }
            } else {
                app.draft.clone()
            },
            app.draft_cursor_byte(),
            true,
            app.draft.is_empty(),
        ),
        ComposerPrompt::Body(_) => (
            "›  ".to_owned(),
            if app.draft.is_empty() {
                "Message...".to_owned()
            } else {
                app.draft.clone()
            },
            app.draft_cursor_byte(),
            app.composing,
            app.draft.is_empty(),
        ),
        ComposerPrompt::Message => (
            "›  ".to_owned(),
            if app.draft.is_empty() {
                if width < 21 {
                    "Message...".to_owned()
                } else {
                    "Write a message...".to_owned()
                }
            } else {
                app.draft.clone()
            },
            app.draft_cursor_byte(),
            true,
            app.draft.is_empty(),
        ),
        ComposerPrompt::Idle => (
            "›  ".to_owned(),
            if width < 21 {
                "Message...".to_owned()
            } else {
                "Write a message...".to_owned()
            },
            0,
            false,
            true,
        ),
    }
}

/// The local unread count for a conversation, from the newest loaded page. Counts
/// are navigation state, never read receipts, and an empty or zero count is absent.
fn unread_badge(app: &App, conversation: &bloom_domain::Conversation) -> Option<String> {
    let count = app.unread_count(&conversation.id)?;
    if count.shown == 0 {
        return None;
    }
    Some(if count.capped {
        format!("{}+", count.shown)
    } else {
        count.shown.to_string()
    })
}

/// Conversation identity gets the rail width first, then the unread badge. When
/// there is not enough room for both, the name remains and the badge is dropped.
fn sidebar_name_line(
    app: &App,
    conversation: &bloom_domain::Conversation,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let selected = Some(&conversation.id) == app.selected.as_ref() && !app.new_conversation;
    let mut name = app.conversation_name(conversation);
    if let Some(badge) = unread_badge(app, conversation) {
        let suffix = format!(" · {badge}");
        let suffix_width = unicode_width::UnicodeWidthStr::width(suffix.as_str());
        let name_width = width.saturating_sub(suffix_width);
        name = if name_width >= 4 {
            format!("{}{}", truncate_with_ellipsis(&name, name_width), suffix)
        } else {
            truncate_with_ellipsis(&name, width)
        };
    } else {
        name = truncate_with_ellipsis(&name, width);
    }
    Line::from(vec![Span::styled(
        name,
        Style::default()
            .fg(if selected { palette.cyan } else { palette.text })
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )])
}

/// The newest message, clipped with an ellipsis when it cannot fit.
fn sidebar_preview_line(
    conversation: &bloom_domain::Conversation,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let preview = conversation.preview.as_deref().unwrap_or("");
    let single_line = preview.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = truncate_with_ellipsis(&single_line, width);
    Line::from(Span::styled(shown, Style::default().fg(palette.muted)))
}

/// Clip to `width` display columns, ending with an ellipsis when something was cut.
fn truncate_with_ellipsis(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if unicode_width::UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    let mut result = truncate_display_graphemes(value, width.saturating_sub(1));
    result.push('…');
    result
}

fn footer_hints(width: usize, palette: Palette, app: &App, wrapped_input: bool) -> Line<'static> {
    // The Codex-shaped home hints: arrows scroll the transcript, Tab moves to the
    // chat list, and the rest is discoverable through commands and the sheet.
    const HOME_FULL: [(&str, &str); 5] = [
        ("Enter", " send"),
        ("Ctrl+J", " newline"),
        ("Tab", " chats"),
        ("/", " commands"),
        ("?", " help"),
    ];
    const HOME_MEDIUM: [(&str, &str); 4] = [
        ("Enter", " send"),
        ("Ctrl+J", " newline"),
        ("/", " commands"),
        ("?", " help"),
    ];
    const HOME_COMPACT: [(&str, &str); 3] = [("↵", " send"), ("/", " cmds"), ("?", " help")];
    const HOME_NARROW: [(&str, &str); 2] = [("↵", " send"), ("?", " help")];
    const HOME_TIGHT: [(&str, &str); 1] = [("↵", "")];
    const WRAPPED_HOME_FULL: [(&str, &str); 5] = [
        ("Enter", " send"),
        ("Ctrl+J", " newline"),
        ("↑↓", " cursor"),
        ("/", " commands"),
        ("?", " help"),
    ];
    const WRAPPED_HOME_COMPACT: [(&str, &str); 3] =
        [("↑↓", " cursor"), ("Tab", " chats"), ("/", " cmds")];
    const SIDEBAR_FOCUS_FULL: [(&str, &str); 5] = [
        ("↑↓", " chats"),
        ("Tab", " composer"),
        ("Ctrl+P", " previous"),
        ("Ctrl+B n", " next"),
        ("?", " help"),
    ];
    const SIDEBAR_FOCUS_COMPACT: [(&str, &str); 3] =
        [("↑↓", " chats"), ("Tab", " composer"), ("Ctrl+P", " prev")];
    const LEADER_FULL: [(&str, &str); 5] = [
        ("n", " next"),
        ("Ctrl+P", " previous"),
        ("j/k", " scroll"),
        ("f/s/t", " options"),
        ("Esc", " cancel"),
    ];
    const LEADER_COMPACT: [(&str, &str); 3] =
        [("n", " next"), ("Ctrl+P", " previous"), ("Esc", " cancel")];
    const FILTER_FULL: [(&str, &str); 4] = [
        ("←→", " type"),
        ("↑↓", " select"),
        ("Enter", " open"),
        ("Esc", " cancel"),
    ];
    const FILTER_COMPACT: [(&str, &str); 4] =
        [("←→", " type"), ("↑↓", ""), ("↵", " open"), ("Esc", "")];
    const FILTER_NARROW: [(&str, &str); 3] = [("↑↓", ""), ("↵", ""), ("Esc", "")];
    const FILTER_TIGHT: [(&str, &str); 2] = [("↑↓", ""), ("Esc", "")];
    const RECIPIENT_FULL: [(&str, &str); 3] =
        [("Enter", " continue"), ("Esc", " cancel"), ("←→", " edit")];
    const RECIPIENT_MEDIUM: [(&str, &str); 2] = [("Enter", " continue"), ("Esc", " cancel")];
    const RECIPIENT_COMPACT: [(&str, &str); 2] = [("↵", " next"), ("Esc", " cancel")];
    const RECIPIENT_TIGHT: [(&str, &str); 2] = [("↵", " next"), ("Esc", "")];
    const NEW_CONTACT_FULL: [(&str, &str); 4] = [
        ("↑↓", " select"),
        ("←→", " source"),
        ("Enter", " choose"),
        ("Esc", " cancel"),
    ];
    const NEW_CONTACT_COMPACT: [(&str, &str); 2] = [("↑↓", " select"), ("↵", " choose")];
    const START_FULL: [(&str, &str); 3] =
        [("Enter", " start"), ("Esc", " cancel"), ("←→", " edit")];
    const START_MEDIUM: [(&str, &str); 2] = [("Enter", " start"), ("Esc", " cancel")];
    const START_COMPACT: [(&str, &str); 2] = [("↵", " start"), ("Esc", " cancel")];
    const START_TIGHT: [(&str, &str); 2] = [("↵", " start"), ("Esc", "")];
    const ENTER_ONLY: [(&str, &str); 1] = [("↵", "")];
    const COMMAND_FULL: [(&str, &str); 3] =
        [("↑↓", " select"), ("Enter", " run"), ("Esc", " dismiss")];
    const COMMAND_COMPACT: [(&str, &str); 2] = [("↵", " run"), ("Esc", " dismiss")];
    const COMMAND_TIGHT: [(&str, &str); 1] = [("↵", " run")];
    const EMOJI_FULL: [(&str, &str); 2] = [("↑↓", " choose"), ("Enter", " insert")];
    const SETTINGS_FULL: [(&str, &str); 5] = [
        ("←→", " tabs"),
        ("↑↓", " select"),
        ("Space", " toggle"),
        ("Enter", " set"),
        ("Esc", " close"),
    ];
    const SETTINGS_MEDIUM: [(&str, &str); 3] =
        [("←→/↑↓", " move"), ("Enter", " set"), ("Esc", " close")];
    const SETTINGS_TIGHT: [(&str, &str); 2] = [("↑↓←→", " move"), ("Esc", " close")];
    const THEME_FULL: [(&str, &str); 4] = [
        ("↑↓", " choose"),
        ("←→", " cycle"),
        ("Enter", " apply"),
        ("Esc", " close"),
    ];
    const THEME_MEDIUM: [(&str, &str); 3] =
        [("↑↓", " choose"), ("Enter", " apply"), ("Esc", " close")];
    const CONTACTS_FULL: [(&str, &str); 4] = [
        ("↑↓", " select"),
        ("Enter", " name"),
        ("Tab", " chats"),
        ("Esc", " close"),
    ];
    const CONTACTS_EDIT: [(&str, &str); 2] = [("Enter", " save"), ("Esc", " cancel")];
    const CONTACTS_TIGHT: [(&str, &str); 1] = [("Esc", " close")];
    const RENAME_FULL: [(&str, &str); 2] = [("Enter", " save"), ("Esc", " cancel")];
    const RENAME_TIGHT: [(&str, &str); 1] = [("Esc", " cancel")];
    const SEARCH_FULL: [(&str, &str); 4] = [
        ("Enter", " search text/filenames"),
        ("Ctrl+P", " chats with"),
        ("↑↓", " choose"),
        ("Esc", " close"),
    ];
    const SEARCH_COMPACT: [(&str, &str); 3] = [
        ("Enter", " search"),
        ("Ctrl+P", " scope"),
        ("Esc", " close"),
    ];
    const SEARCH_TIGHT: [(&str, &str); 2] = [("Ctrl+P", " scope"), ("Esc", " close")];
    const SEARCH_PICKER_FULL: [(&str, &str); 4] = [
        ("↑↓", " choose"),
        ("Enter", " set scope"),
        ("Type", " filter"),
        ("Esc", " back"),
    ];
    const SEARCH_PICKER_COMPACT: [(&str, &str); 3] =
        [("↑↓", " choose"), ("↵", " scope"), ("Esc", " back")];
    const SEARCH_PICKER_TIGHT: [(&str, &str); 2] = [("↑↓", ""), ("Esc", " back")];

    let hints: &[(&str, &str)] = if app.leader_pending {
        if width >= 64 {
            &LEADER_FULL
        } else if width >= 27 {
            &LEADER_COMPACT
        } else {
            &LEADER_COMPACT[..1]
        }
    } else if app.search.is_some() {
        if app.search_participant_picker_open() {
            if width >= 45 {
                &SEARCH_PICKER_FULL
            } else if width >= 27 {
                &SEARCH_PICKER_COMPACT
            } else {
                &SEARCH_PICKER_TIGHT
            }
        } else if width >= 52 {
            &SEARCH_FULL
        } else if width >= 30 {
            &SEARCH_COMPACT
        } else {
            &SEARCH_TIGHT
        }
    } else if app.contacts_open {
        if app.contacts_editing.is_some() {
            &CONTACTS_EDIT
        } else if width >= 32 {
            &CONTACTS_FULL
        } else {
            &CONTACTS_TIGHT
        }
    } else if app.theme_panel_open {
        if width >= 53 {
            &THEME_FULL
        } else if width >= 35 {
            &THEME_MEDIUM
        } else {
            &SETTINGS_TIGHT
        }
    } else if app.settings_open {
        if width >= 64 {
            &SETTINGS_FULL
        } else if width >= 38 {
            &SETTINGS_MEDIUM
        } else {
            &SETTINGS_TIGHT
        }
    } else if app.new_conversation && app.pending_address.is_none() {
        if app.new_contact_suggestions().is_empty() {
            if width >= 37 {
                &RECIPIENT_FULL
            } else if width >= 27 {
                &RECIPIENT_MEDIUM
            } else if width >= 19 {
                &RECIPIENT_COMPACT
            } else if width >= 12 {
                &RECIPIENT_TIGHT
            } else {
                &ENTER_ONLY
            }
        } else if width >= 38 {
            &NEW_CONTACT_FULL
        } else if width >= 16 {
            &NEW_CONTACT_COMPACT
        } else {
            &RECIPIENT_TIGHT
        }
    } else if app.renaming.is_some() {
        if width >= 30 {
            &RENAME_FULL
        } else {
            &RENAME_TIGHT
        }
    } else if app.command_menu {
        if width >= 37 {
            &COMMAND_FULL
        } else if width >= 16 {
            &COMMAND_COMPACT
        } else if width >= 2 {
            &COMMAND_TIGHT
        } else {
            return Line::default();
        }
    } else if !app.emoji_suggestions().is_empty() {
        &EMOJI_FULL
    } else if app.filter_query().is_some() {
        if width >= 45 {
            &FILTER_FULL
        } else if width >= 27 {
            &FILTER_COMPACT
        } else if width >= 12 {
            &FILTER_NARROW
        } else if width >= 8 {
            &FILTER_TIGHT
        } else {
            &FILTER_TIGHT[..1]
        }
    } else if app.focus == Focus::Sidebar {
        if width >= 64 {
            &SIDEBAR_FOCUS_FULL
        } else {
            &SIDEBAR_FOCUS_COMPACT
        }
    } else if wrapped_input {
        if width >= 60 {
            &WRAPPED_HOME_FULL
        } else {
            &WRAPPED_HOME_COMPACT
        }
    } else {
        match app.composer_prompt() {
            ComposerPrompt::Recipient if width >= 37 => &RECIPIENT_FULL,
            ComposerPrompt::Recipient if width >= 27 => &RECIPIENT_MEDIUM,
            ComposerPrompt::Recipient if width >= 19 => &RECIPIENT_COMPACT,
            ComposerPrompt::Recipient if width >= 12 => &RECIPIENT_TIGHT,
            ComposerPrompt::Recipient => &ENTER_ONLY,
            ComposerPrompt::Body(_) if width >= 34 => &START_FULL,
            ComposerPrompt::Body(_) if width >= 24 => &START_MEDIUM,
            ComposerPrompt::Body(_) if width >= 20 => &START_COMPACT,
            ComposerPrompt::Body(_) if width >= 13 => &START_TIGHT,
            ComposerPrompt::Body(_) => &ENTER_ONLY,
            ComposerPrompt::Message if width >= 60 => &HOME_FULL,
            ComposerPrompt::Message if width >= 45 => &HOME_MEDIUM,
            ComposerPrompt::Message if width >= 28 => &HOME_COMPACT,
            ComposerPrompt::Message if width >= 14 => &HOME_NARROW,
            ComposerPrompt::Message if width >= 2 => &HOME_TIGHT,
            ComposerPrompt::Message => return Line::default(),
            // Idle is only reachable if something cancelled the composer; offer the
            // same routes back into it.
            _ if width >= 60 => &HOME_FULL,
            _ if width >= 45 => &HOME_MEDIUM,
            _ if width >= 28 => &HOME_COMPACT,
            _ if width >= 14 => &HOME_NARROW,
            _ if width >= 2 => &HOME_TIGHT,
            _ => return Line::default(),
        }
    };
    let key_style = Style::default()
        .fg(palette.cyan)
        .add_modifier(Modifier::BOLD);
    let description_style = Style::default().fg(palette.muted);
    let mut count = hints.len();
    let required_width = |count: usize| {
        hints[..count]
            .iter()
            .map(|(key, description)| {
                unicode_width::UnicodeWidthStr::width(*key)
                    + unicode_width::UnicodeWidthStr::width(*description)
            })
            .sum::<usize>()
            + count.saturating_sub(1) * unicode_width::UnicodeWidthStr::width(" · ")
    };
    while count > 0 && required_width(count) > width {
        count -= 1;
    }
    if count == 0 {
        let key = truncate_display_graphemes(hints[0].0, width);
        return Line::from(Span::styled(key, key_style));
    }
    let mut spans = Vec::new();
    for (index, (key, description)) in hints[..count].iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" · "));
        }
        spans.push(Span::styled(*key, key_style));
        spans.push(Span::styled(*description, description_style));
    }
    Line::from(spans)
}

/// Composer geometry: a shaded band with padding around wrapped draft rows, plus
/// footer rows for status and hints kept outside the band.
#[derive(Clone, Copy)]
struct ComposerGeometry {
    band: Rect,
    input: Rect,
    footer: Option<Rect>,
}

fn composer_geometry(area: Rect, requested_input_rows: u16) -> ComposerGeometry {
    let input_rows = requested_input_rows.max(1).min(area.height.max(1));
    let footer_rows = 2u16.min(area.height.saturating_sub(input_rows));
    let pads = area.height.saturating_sub(input_rows + footer_rows);
    let pad_top = u16::from(pads >= 2);
    let pad_bottom = pads.saturating_sub(pad_top).min(1);
    let band = Rect::new(
        area.x,
        area.y,
        area.width,
        input_rows + pad_top + pad_bottom,
    );
    let input = Rect::new(
        area.x,
        area.y.saturating_add(pad_top),
        area.width,
        input_rows,
    );
    let footer = (footer_rows > 0).then(|| {
        Rect::new(
            area.x,
            area.y.saturating_add(band.height),
            area.width,
            footer_rows,
        )
    });
    ComposerGeometry {
        band,
        input,
        footer,
    }
}

/// The slash-command popup. Its surface matches an outgoing chat line, and it
/// takes priority over the transcript. The
/// returned rectangle is the hitbox the mouse layer uses.
fn render_command_menu(
    frame: &mut Frame<'_>,
    app: &App,
    content: Rect,
    pane: Rect,
    palette: Palette,
) -> Option<(Rect, Vec<(Rect, usize)>)> {
    if !app.command_menu || content.height < 1 || content.width < 1 {
        return None;
    }
    let suggestions = app.command_suggestions();
    let item_count = suggestions.len().max(1);
    let visible = item_count.min(11).min(content.height as usize);
    let first = app
        .command_selection
        .saturating_sub(visible.saturating_sub(1))
        .min(item_count.saturating_sub(visible));
    let panel_width = pane.width;
    let panel_height = visible.min(content.height as usize) as u16;
    // Anchor the picker directly above the composer. Keep its bottom edge
    // adjacent to the composer input row so no transcript row leaks between them.
    let area = Rect::new(
        pane.x,
        content.y + content.height.saturating_sub(panel_height),
        panel_width,
        panel_height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.input_surface)),
        area,
    );
    let inner = area;
    let text_area = Rect::new(
        inner.x.saturating_add(2),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let row_width = text_area.width as usize;
    let mut menu_rows: Vec<(Rect, usize)> = Vec::new();
    for row in 0..visible {
        let row_area = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let text_row = Rect::new(text_area.x, row_area.y, text_area.width, 1);
        if row_area.y >= inner.y + inner.height {
            break;
        }
        let row_index = first + row;
        if row_index < suggestions.len() {
            menu_rows.push((row_area, row_index));
        }
        let selected = !suggestions.is_empty() && row_index == app.command_selection;
        let row_bg = if selected {
            palette.selection
        } else {
            palette.input_surface
        };
        frame.render_widget(
            Block::default().style(Style::default().bg(row_bg)),
            row_area,
        );
        if suggestions.is_empty() {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(
                    "No matching commands",
                    row_width,
                ))
                .style(Style::default().fg(palette.muted).bg(row_bg)),
                text_row,
            );
            continue;
        }
        let (name, description) = suggestions[row_index];
        let marker_width = 2usize.min(row_width);
        let name_column = row_width.saturating_sub(marker_width + 2).min(16);
        let shown_name = truncate_display_graphemes(name, name_column);
        let shown_name_width = unicode_width::UnicodeWidthStr::width(shown_name.as_str());
        let description_width = row_width.saturating_sub(marker_width + name_column + 2);
        let shown_description = truncate_display_graphemes(description, description_width);
        let mut spans = Vec::new();
        if marker_width > 0 {
            spans.push(Span::styled(
                if selected { "› " } else { "  " },
                Style::default()
                    .fg(palette.cyan)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.extend([
            Span::styled(
                shown_name,
                Style::default()
                    .fg(palette.cyan)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(name_column.saturating_sub(shown_name_width) + 2),
                Style::default().bg(row_bg),
            ),
            Span::styled(
                shown_description,
                Style::default()
                    .fg(if selected {
                        palette.text
                    } else {
                        palette.muted
                    })
                    .bg(row_bg),
            ),
        ]);
        frame.render_widget(Paragraph::new(Line::from(spans)), text_row);
    }
    // Bridge the reserved row between transcript and composer with the same
    // surface, so the popup visibly touches the chat line.
    if pane.height > 0 {
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.input_surface)),
            Rect::new(
                pane.x,
                content.y.saturating_add(content.height),
                pane.width,
                1,
            ),
        );
    }
    // The panel itself remains the selectable hitbox.
    Some((area, menu_rows))
}

/// Shared measured geometry for titled pickers, bottom-anchored to the composer.
#[derive(Clone, Copy)]
struct TitledPickerGeometry {
    panel: Rect,
    title: Rect,
    tabs: Rect,
    rows: Rect,
    show_descriptions: bool,
    show_note: bool,
}

/// Measure required content first, then optional Settings descriptions/note and
/// finally outer breathing room. `optional_rows` is zero for simple list pickers.
fn titled_picker_geometry(
    bounds: Rect,
    band_top: u16,
    categories: usize,
    content_rows: usize,
    optional_rows: usize,
) -> TitledPickerGeometry {
    let inset = 2u16.min(bounds.width / 2);
    let available = bounds.height.min(band_top.saturating_sub(bounds.y));
    let has_tabs = u16::from(categories > 0);
    let content_rows = content_rows.min(u16::MAX as usize) as u16;
    let optional_rows = optional_rows.min(u16::MAX as usize) as u16;
    let essential = 1u16.saturating_add(has_tabs).saturating_add(content_rows);
    let mut remaining = available.saturating_sub(essential);

    let show_descriptions = optional_rows >= content_rows && remaining >= content_rows;
    let description_rows = if show_descriptions { content_rows } else { 0 };
    remaining = remaining.saturating_sub(description_rows);
    // Settings metadata is lower priority than options and descriptions. Keep a
    // bottom row after the note; Contacts passes no optional rows.
    let show_note = optional_rows > content_rows && remaining >= 2;
    if show_note {
        remaining = remaining.saturating_sub(2);
    }
    let shown_optional_rows = description_rows.saturating_add(u16::from(show_note));
    let top_pad = u16::from(remaining > 0);
    remaining = remaining.saturating_sub(top_pad);
    let tab_gap = u16::from(has_tabs > 0 && remaining > 0);
    remaining = remaining.saturating_sub(tab_gap);
    let bottom_pad = if show_note {
        1
    } else {
        u16::from(remaining > 0)
    };

    let desired = essential
        .saturating_add(shown_optional_rows)
        .saturating_add(top_pad)
        .saturating_add(tab_gap)
        .saturating_add(bottom_pad);
    let height = desired.min(available);
    let y = band_top.saturating_sub(height).max(bounds.y);
    let panel = Rect::new(bounds.x, y, bounds.width, height);
    let title_y = panel.y.saturating_add(top_pad);
    let tabs_y = title_y.saturating_add(1);
    let rows_y = tabs_y.saturating_add(has_tabs).saturating_add(tab_gap);
    let rows_bottom = panel
        .y
        .saturating_add(panel.height)
        .saturating_sub(bottom_pad);
    TitledPickerGeometry {
        panel,
        title: Rect::new(
            panel.x.saturating_add(inset),
            title_y,
            panel.width.saturating_sub(inset.saturating_mul(2)),
            u16::from(panel.height > top_pad),
        ),
        tabs: Rect::new(
            panel.x.saturating_add(inset),
            tabs_y,
            panel.width.saturating_sub(inset.saturating_mul(2)),
            has_tabs.min(panel.y.saturating_add(panel.height).saturating_sub(tabs_y)),
        ),
        rows: Rect::new(
            panel.x,
            rows_y,
            panel.width,
            rows_bottom.saturating_sub(rows_y),
        ),
        show_descriptions,
        show_note,
    }
}

fn settings_geometry(
    bounds: Rect,
    band_top: u16,
    categories: usize,
    options: usize,
) -> TitledPickerGeometry {
    titled_picker_geometry(
        bounds,
        band_top,
        categories,
        options,
        options.saturating_add(1),
    )
}

fn render_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
    show_descriptions: bool,
    show_note: bool,
) -> Vec<(Rect, usize)> {
    let mut rows = Vec::new();
    if area.width < 8 || area.height == 0 {
        return rows;
    }
    let theme_value = match app.theme_override {
        None => format!("Automatic ({})", app.system_theme.label()),
        Some(theme) => theme.label().to_owned(),
    };
    let sidebar_value = if app.compact_sidebar {
        "Compact"
    } else {
        "Comfortable"
    };
    let settings: Vec<(&str, String, &str)> = if app.settings_category == 0 {
        vec![(
            "Theme",
            theme_value,
            "Cycles Automatic, Dark, and Light terminal themes.",
        )]
    } else {
        vec![
            (
                "Sidebar density",
                sidebar_value.to_owned(),
                "Comfortable shows previews; Compact uses one line per chat.",
            ),
            (
                "Wrap composer",
                if app.wrap_composer { "On" } else { "Off" }.to_owned(),
                "Wrap long drafts across visible composer rows.",
            ),
        ]
    };
    let inset = 2u16.min(area.width / 2);
    let text_x = area.x.saturating_add(inset);
    let text_width = area.width.saturating_sub(inset.saturating_mul(2)).max(1);
    let bottom = area.y.saturating_add(area.height);
    let mut y = area.y;
    for (index, (label, value, description)) in settings.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let row = Rect::new(area.x, y, area.width, 1);
        rows.push((row, index));
        let selected = app.settings_selection == index;
        let background = if selected {
            palette.selection
        } else {
            palette.input_surface
        };
        let foreground = palette.text;
        frame.render_widget(Block::default().style(Style::default().bg(background)), row);
        let text_width_usize = text_width as usize;
        let label_width = 16usize.min(text_width_usize.saturating_sub(2));
        let value_width = text_width_usize.saturating_sub(label_width + 2);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default()
                        .fg(palette.cyan)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    display_pad(label, label_width),
                    Style::default()
                        .fg(if selected { foreground } else { palette.text })
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate_display_graphemes(value, value_width),
                    Style::default()
                        .fg(if selected { foreground } else { palette.cyan })
                        .bg(background),
                ),
            ])),
            Rect::new(text_x, y, text_width, 1),
        );
        y = y.saturating_add(1);
        if show_descriptions && y < bottom {
            frame.render_widget(
                Paragraph::new(truncate_with_ellipsis(description, text_width as usize))
                    .style(Style::default().fg(palette.muted).bg(palette.input_surface)),
                Rect::new(text_x, y, text_width, 1),
            );
            y = y.saturating_add(1);
        }
    }
    if show_note && y < bottom {
        frame.render_widget(
            Paragraph::new(truncate_with_ellipsis(
                "These choices are stored locally.",
                text_width as usize,
            ))
            .style(Style::default().fg(palette.muted).bg(palette.input_surface)),
            Rect::new(text_x, y, text_width, 1),
        );
    }
    rows
}

/// Draw a compact category row using the popup surface and return its hitboxes.
fn render_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    labels: &[&str],
    selected: usize,
    palette: Palette,
) -> Vec<(Rect, usize)> {
    let mut hits = Vec::new();
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.input_surface)),
        area,
    );
    let mut x = area.x;
    for (index, label) in labels.iter().enumerate() {
        if x >= area.x.saturating_add(area.width) {
            break;
        }
        let width = ((*label).len() as u16 + 4).min(area.x + area.width - x);
        let row = Rect::new(x, area.y, width, 1);
        let active = index == selected;
        let background = palette.input_surface;
        frame.render_widget(
            Paragraph::new(format!(" {} ", label)).style(
                Style::default()
                    .fg(if active { palette.text } else { palette.muted })
                    .bg(background)
                    .add_modifier(if active {
                        Modifier::BOLD | Modifier::UNDERLINED
                    } else {
                        Modifier::empty()
                    }),
            ),
            row,
        );
        hits.push((row, index));
        x = x.saturating_add(width).saturating_add(1);
    }
    hits
}

/// Draw the Contacts body. The shared picker owns title/tabs; this body owns
/// the search prompt, count, empty hint, and consecutive selectable contact rows.
fn render_contacts(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
) -> Vec<(Rect, usize)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let inset = 2u16.min(area.width / 2);
    let text_x = area.x.saturating_add(inset);
    let text_width = area.width.saturating_sub(inset.saturating_mul(2)).max(1);
    let handles = app.visible_contact_handles();
    let search_text = if app.contacts_query.is_empty() {
        "Search by number or nickname".to_owned()
    } else {
        format!("Search: {}", app.contacts_query)
    };
    frame.render_widget(
        Paragraph::new(truncate_display_graphemes(
            &search_text,
            text_width as usize,
        ))
        .style(Style::default().fg(if app.contacts_query.is_empty() {
            palette.muted
        } else {
            palette.text
        })),
        Rect::new(text_x, area.y, text_width, 1),
    );
    let show_count = area.height > 2;
    if show_count {
        let count_text = if !app.contacts_query.is_empty() {
            format!("{} matches · Enter to edit", handles.len())
        } else if app.contacts_tab == 0 {
            format!(
                "{} contact{} · Enter to nickname",
                handles.len(),
                if handles.len() == 1 { "" } else { "s" }
            )
        } else {
            format!("{} nicknames · Enter to edit", handles.len())
        };
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(&count_text, text_width as usize))
                .style(Style::default().fg(palette.muted)),
            Rect::new(text_x, area.y + 1, text_width, 1),
        );
    }
    if handles.is_empty() {
        let empty_row = area.y + if show_count { 2 } else { 1 };
        if empty_row < area.y.saturating_add(area.height) {
            let text = if !app.contacts_query.is_empty() {
                format!("No contacts match \"{}\".", app.contacts_query)
            } else if app.contacts_tab == 1 {
                "No nicknames yet. Select a contact and press Enter to add one.".to_owned()
            } else {
                "No contacts yet. Contacts come from chats Bloom already knows.".to_owned()
            };
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(&text, text_width as usize))
                    .style(Style::default().fg(palette.muted)),
                Rect::new(text_x, empty_row, text_width, 1),
            );
        }
        return Vec::new();
    }
    let list_offset = if show_count { 2 } else { 1 };
    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(list_offset),
        area.width,
        area.height.saturating_sub(list_offset),
    );
    let items = handles
        .iter()
        .map(|handle| {
            let label = app.contact_label(handle);
            let address = match handle.kind() {
                bloom_domain::HandleKind::Email => format!("mailto:{}", handle.value()),
                bloom_domain::HandleKind::Tel => format!("tel:{}", handle.value()),
                bloom_domain::HandleKind::Opaque => handle.value().to_owned(),
            };
            if !app
                .nicknames
                .contains_key(&crate::app::nickname_key(handle))
            {
                Line::from(Span::styled(
                    display_truncate(&address, text_width as usize),
                    Style::default().fg(palette.text),
                ))
            } else {
                let label_width = unicode_width::UnicodeWidthStr::width(label.as_str());
                let separator_width = 2;
                let address_width =
                    (text_width as usize).saturating_sub(label_width + separator_width);
                Line::from(vec![
                    Span::styled(
                        truncate_display_graphemes(
                            label.as_str(),
                            label_width.min(text_width as usize),
                        ),
                        Style::default()
                            .fg(palette.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", Style::default().fg(palette.muted)),
                    Span::styled(
                        display_truncate(&address, address_width),
                        Style::default().fg(palette.muted),
                    ),
                ])
            }
        })
        .collect::<Vec<_>>();
    let visible = list_area.height as usize;
    if visible == 0 {
        return Vec::new();
    }
    let selected = app.contacts_selection.min(handles.len() - 1);
    let first = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(handles.len().saturating_sub(visible));
    let mut hits = Vec::new();
    for (index, item) in items.iter().enumerate().skip(first).take(visible) {
        let row = Rect::new(
            list_area.x,
            list_area.y + (index - first) as u16,
            list_area.width,
            1,
        );
        let active = index == selected;
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        frame.render_widget(Block::default().style(Style::default().bg(background)), row);
        let mut spans = vec![Span::styled(
            if active { "› " } else { "  " },
            Style::default().fg(palette.cyan).bg(background),
        )];
        spans.extend(item.spans.iter().map(|span| {
            Span::styled(
                span.content.clone(),
                span.style.bg(background).fg(if active {
                    palette.text
                } else {
                    span.style.fg.unwrap_or(palette.text)
                }),
            )
        }));
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(text_x, row.y, text_width, 1),
        );
        hits.push((row, index));
    }
    hits
}

fn render_new_contacts(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
    tab_hits: &mut Vec<(Rect, usize)>,
) -> Vec<(Rect, usize)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    tab_hits.extend(render_tabs(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        &["Recent", "All"],
        app.new_contact_tab,
        palette,
    ));
    if area.height == 1 {
        return Vec::new();
    }
    let handles = app.new_contact_suggestions();
    let rows_area = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    if handles.is_empty() {
        let hint = if app.draft.trim().is_empty() {
            "No known contacts yet. Type an address to start."
        } else {
            "No matches. Press Enter to use this address."
        };
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(hint, area.width as usize))
                .style(Style::default().fg(palette.muted)),
            Rect::new(rows_area.x, rows_area.y, rows_area.width, 1),
        );
        return Vec::new();
    }

    let visible_count = rows_area.height as usize;
    let selected = app.new_contact_selection.min(handles.len() - 1);
    let first = selected
        .saturating_sub(visible_count.saturating_sub(1))
        .min(handles.len().saturating_sub(visible_count));
    let mut hits = Vec::new();
    let end = (first + visible_count).min(handles.len());
    for (index, handle) in handles.iter().enumerate().take(end).skip(first) {
        let y = rows_area.y + (index - first) as u16;
        let row = Rect::new(rows_area.x, y, rows_area.width, 1);
        let active = index == selected;
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        let foreground = palette.text;
        let address = match handle.kind() {
            bloom_domain::HandleKind::Email => format!("mailto:{}", handle.value()),
            bloom_domain::HandleKind::Tel => format!("tel:{}", handle.value()),
            bloom_domain::HandleKind::Opaque => handle.value().to_owned(),
        };
        let nickname = app.nicknames.get(&crate::app::nickname_key(handle));
        let mut spans = vec![Span::styled(
            if active { "› " } else { "  " },
            Style::default().fg(palette.cyan).bg(background),
        )];
        if let Some(label) = nickname {
            let label_width = unicode_width::UnicodeWidthStr::width(label.as_str());
            let address_width = (row.width as usize).saturating_sub(label_width + 3);
            spans.extend([
                Span::styled(
                    truncate_display_graphemes(label, label_width.min(row.width as usize)),
                    Style::default()
                        .fg(foreground)
                        .add_modifier(Modifier::BOLD)
                        .bg(background),
                ),
                Span::styled("  ", Style::default().fg(palette.muted).bg(background)),
                Span::styled(
                    display_truncate(&address, address_width),
                    Style::default()
                        .fg(if active { palette.text } else { palette.muted })
                        .bg(background),
                ),
            ]);
        } else {
            spans.push(Span::styled(
                display_truncate(&address, row.width.saturating_sub(2) as usize),
                Style::default()
                    .fg(foreground)
                    .add_modifier(Modifier::BOLD)
                    .bg(background),
            ));
        }
        frame.render_widget(Block::default().style(Style::default().bg(background)), row);
        frame.render_widget(Paragraph::new(Line::from(spans)), row);
        hits.push((row, index));
    }
    hits
}

fn render_emoji_suggestions(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
) -> Vec<(Rect, usize)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let suggestions = app.emoji_suggestions();
    if suggestions.is_empty() {
        return Vec::new();
    }
    let selected = app.emoji_selection.min(suggestions.len() - 1);
    let first = selected
        .saturating_sub(area.height.saturating_sub(1) as usize)
        .min(suggestions.len().saturating_sub(area.height as usize));
    let mut hits = Vec::new();
    let end = (first + area.height as usize).min(suggestions.len());
    for (index, suggestion) in suggestions.iter().enumerate().take(end).skip(first) {
        let row = Rect::new(area.x, area.y + (index - first) as u16, area.width, 1);
        let active = index == selected;
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        let text = format!(
            "{} {}  :{}:  {}",
            if active { "›" } else { " " },
            suggestion.emoji,
            suggestion.shortcode,
            suggestion.name
        );
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(&text, row.width as usize)).style(
                Style::default()
                    .fg(palette.text)
                    .bg(background)
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            row,
        );
        hits.push((row, index));
    }
    hits
}

fn search_conversation_name(app: &App, id: &bloom_domain::ConversationId) -> String {
    app.conversations
        .iter()
        .find(|conversation| &conversation.id == id || conversation.aliases.contains(id))
        .map_or_else(
            || id.value().to_owned(),
            |conversation| app.conversation_name(conversation),
        )
}

/// The one-line status above the search hits, with the count when results are ready.
fn search_status_text(state: &SearchState) -> Option<String> {
    if state.loading {
        Some("Searching…".to_owned())
    } else if let Some(error) = state.error.as_ref() {
        Some(error.clone())
    } else if state.completed && state.hits.is_empty() && !state.query.trim().is_empty() {
        Some("No messages found.".to_owned())
    } else if state.completed && !state.hits.is_empty() {
        Some(if state.hits.len() as u32 >= SEARCH_RESULT_LIMIT {
            format!("{} results (may be more)", state.hits.len())
        } else {
            format!("{} results", state.hits.len())
        })
    } else {
        None
    }
}

fn search_handle_kind_label(kind: &HandleKind) -> &'static str {
    match kind {
        HandleKind::Tel => "tel",
        HandleKind::Email => "email",
        HandleKind::Opaque => "opaque",
    }
}

/// Exact handle kind/value and transport, because labels and local nicknames are
/// never identity keys.
fn search_participant_identity(filter: &ParticipantFilter) -> String {
    format!(
        "{} · {}:{}",
        filter.transport.as_str(),
        search_handle_kind_label(filter.handle.kind()),
        filter.handle.value()
    )
}

fn search_scope_text(scope: Option<&ParticipantFilter>) -> String {
    match scope {
        None => "All chats · Ctrl+P chats with".to_owned(),
        Some(filter) => format!("Chats with {}", search_participant_identity(filter)),
    }
}

fn search_participant_candidate_identity(candidate: &SearchParticipantCandidate) -> String {
    candidate
        .filter()
        .map_or_else(|| "All chats".to_owned(), search_participant_identity)
}

#[derive(Default)]
struct SearchHitRows {
    results: Vec<(Rect, usize)>,
    participants: Vec<(Rect, Option<ParticipantFilter>)>,
}

fn render_search(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) -> SearchHitRows {
    let empty = SearchHitRows::default();
    if area.width == 0 || area.height == 0 {
        return empty;
    }
    let Some(state) = app.search.as_ref() else {
        return empty;
    };
    let inset = 2u16.min(area.width / 2);
    let text_x = area.x.saturating_add(inset);
    let text_width = area.width.saturating_sub(inset.saturating_mul(2)).max(1) as usize;
    if app.search_participant_picker_open() && area.height < 4 {
        // Compact terminals need selectable identities before redundant query
        // context. The panel title already says Chats with.
        return SearchHitRows {
            results: Vec::new(),
            participants: render_search_participant_picker(
                frame, area, app, palette, text_x, text_width,
            ),
        };
    }
    let prefix = "Search: ";
    let cursor = "▏";
    let query_width = text_width
        .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(cursor));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, Style::default().fg(palette.muted)),
            Span::styled(
                truncate_display_graphemes(&state.query, query_width),
                Style::default().fg(palette.text),
            ),
            Span::styled(cursor, Style::default().fg(palette.cyan)),
        ])),
        Rect::new(text_x, area.y, text_width as u16, 1),
    );

    let mut row = area.y.saturating_add(1);
    let bottom = area.y.saturating_add(area.height);
    if app.search_participant_picker_open() {
        let picker_area = Rect::new(area.x, row, area.width, bottom.saturating_sub(row));
        return SearchHitRows {
            results: Vec::new(),
            participants: render_search_participant_picker(
                frame,
                picker_area,
                app,
                palette,
                text_x,
                text_width,
            ),
        };
    }
    if row < bottom {
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(
                &search_scope_text(app.search_participant_scope()),
                text_width,
            ))
            .style(Style::default().fg(palette.muted)),
            Rect::new(text_x, row, text_width as u16, 1),
        );
        row = row.saturating_add(1);
    }
    let status = search_status_text(state).map(|message| {
        let style = if state.error.is_some() {
            Style::default().fg(palette.red)
        } else {
            Style::default().fg(palette.muted)
        };
        (message, style)
    });
    if let Some((message, style)) = status {
        if row < bottom {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(&message, text_width)).style(style),
                Rect::new(text_x, row, text_width as u16, 1),
            );
        }
        row = row.saturating_add(1);
    }
    let mut hits = Vec::new();
    if state.loading || state.error.is_some() || state.hits.is_empty() || row >= bottom {
        return SearchHitRows {
            results: hits,
            ..Default::default()
        };
    }
    let visible = (bottom - row).min(state.hits.len() as u16).min(8) as usize;
    if visible == 0 {
        return SearchHitRows {
            results: hits,
            ..Default::default()
        };
    }
    let selected = state.selected.min(state.hits.len().saturating_sub(1));
    let first = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(state.hits.len().saturating_sub(visible));
    for (index, hit) in state.hits.iter().enumerate().skip(first).take(visible) {
        let row_area = Rect::new(area.x, row, area.width, 1);
        let active = index == selected;
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        frame.render_widget(
            Block::default().style(Style::default().bg(background)),
            row_area,
        );
        let marker_width = 2usize;
        let body_width = text_width.saturating_sub(marker_width);
        let name = search_conversation_name(app, &hit.conversation_id);
        let name_width = body_width.min(20);
        let shown_name = truncate_display_graphemes(&name, name_width);
        let shown_name_width = unicode_width::UnicodeWidthStr::width(shown_name.as_str());
        let snippet_width = body_width.saturating_sub(shown_name_width + 2);
        let shown_snippet =
            truncate_display_graphemes(&sanitize_snippet(&hit.snippet), snippet_width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if active { "› " } else { "  " },
                    Style::default()
                        .fg(palette.cyan)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    shown_name,
                    Style::default()
                        .fg(palette.text)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default().bg(background)),
                Span::styled(
                    shown_snippet,
                    Style::default()
                        .fg(if active { palette.text } else { palette.muted })
                        .bg(background),
                ),
            ])),
            Rect::new(text_x, row, text_width as u16, 1),
        );
        hits.push((row_area, index));
        row = row.saturating_add(1);
    }
    SearchHitRows {
        results: hits,
        ..Default::default()
    }
}

fn participant_candidate_text(candidate: &SearchParticipantCandidate, width: usize) -> String {
    if candidate.is_all_chats() {
        return candidate.label().to_owned();
    }
    let identity = search_participant_candidate_identity(candidate);
    let labelled = format!("{}  {identity}", candidate.label());
    if unicode_width::UnicodeWidthStr::width(labelled.as_str()) <= width {
        labelled
    } else {
        identity
    }
}

fn participant_candidate_height(candidate: &SearchParticipantCandidate, width: usize) -> usize {
    Paragraph::new(participant_candidate_text(candidate, width))
        .wrap(Wrap { trim: false })
        .line_count(width.max(1) as u16)
        .max(1)
}

fn participant_picker_content_rows(app: &App, width: u16) -> usize {
    let inset = 2u16.min(width / 2);
    let body_width = width.saturating_sub(inset * 2).saturating_sub(2).max(1) as usize;
    let candidates = app.search_participant_candidates();
    let selected = app
        .selected_search_participant_candidate()
        .and_then(|selected| {
            candidates
                .iter()
                .position(|candidate| candidate.filter() == selected.filter())
        })
        .unwrap_or(0);
    2 + candidates
        .iter()
        .skip(selected.saturating_sub(5))
        .take(6)
        .map(|candidate| participant_candidate_height(candidate, body_width))
        .sum::<usize>()
}

/// Narrow participant selection uses the same full-width ownership as Chats,
/// leaving room for complete wrapped identities instead of ambiguous prefixes.
fn render_narrow_participant_picker(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    palette: Palette,
) -> HitMap {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.input_surface)),
        area,
    );
    let inset = 2u16.min(area.width / 2);
    let text_x = area.x + inset;
    let text_width = area.width.saturating_sub(inset * 2).max(1) as usize;
    frame.render_widget(
        Paragraph::new("Chats with").style(
            Style::default()
                .fg(palette.cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(text_x, area.y, text_width as u16, 1),
    );
    let rows = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
    );
    let participant_rows =
        render_search_participant_picker(frame, rows, app, palette, text_x, text_width);
    let hint = truncate_display_graphemes("↑↓ choose · Enter scope · Esc back", text_width);
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(palette.muted)),
        Rect::new(text_x, area.bottom() - 1, text_width as u16, 1),
    );
    HitMap {
        popup: Some(area),
        search_participant_rows: participant_rows,
        ..Default::default()
    }
}

/// The nested picker owns the rows below the body query. Its window is anchored
/// to the selected candidate's exact identity, not to a stored list position.
fn render_search_participant_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
    text_x: u16,
    text_width: usize,
) -> Vec<(Rect, Option<ParticipantFilter>)> {
    let mut hits = Vec::new();
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    let bottom = area.y.saturating_add(area.height);
    let mut row = area.y;
    if row < bottom {
        let prefix = "Find: ";
        let cursor = "▏";
        let query_width = text_width
            .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix))
            .saturating_sub(unicode_width::UnicodeWidthStr::width(cursor));
        let query = app.search_participant_picker_query().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(palette.muted)),
                Span::styled(
                    truncate_display_graphemes(query, query_width),
                    Style::default().fg(palette.text),
                ),
                Span::styled(cursor, Style::default().fg(palette.cyan)),
            ])),
            Rect::new(text_x, row, text_width as u16, 1),
        );
        row = row.saturating_add(1);
    }
    if row >= bottom {
        return hits;
    }
    let candidates = app.search_participant_candidates();
    if candidates.is_empty() {
        return hits;
    }
    let capacity = bottom.saturating_sub(row) as usize;
    let marker_width = 2usize;
    let body_width = text_width.saturating_sub(marker_width).max(1);
    let heights: Vec<_> = candidates
        .iter()
        .map(|candidate| participant_candidate_height(candidate, body_width))
        .collect();
    let selected = app
        .selected_search_participant_candidate()
        .and_then(|selected| {
            candidates
                .iter()
                .position(|candidate| candidate.filter() == selected.filter())
        });
    let mut first = selected.unwrap_or(0);
    let mut used = heights[first];
    while first > 0
        && selected.unwrap_or(0).saturating_sub(first) < 5
        && used.saturating_add(heights[first - 1]) <= capacity
    {
        first -= 1;
        used += heights[first];
    }
    for (index, candidate) in candidates.iter().enumerate().skip(first).take(6) {
        let remaining = bottom.saturating_sub(row) as usize;
        if remaining == 0 || (heights[index] > remaining && selected != Some(index)) {
            break;
        }
        let clipped = heights[index] > remaining;
        let height = heights[index].min(remaining) as u16;
        let row_area = Rect::new(area.x, row, area.width, height);
        let active = selected == Some(index);
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        frame.render_widget(
            Block::default().style(Style::default().bg(background)),
            row_area,
        );
        frame.render_widget(
            Paragraph::new(if active { "› " } else { "  " }).style(
                Style::default()
                    .fg(palette.cyan)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(text_x, row, marker_width as u16, 1),
        );
        let text = participant_candidate_text(candidate, body_width);
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: false }).style(
                Style::default()
                    .fg(if active { palette.text } else { palette.muted })
                    .bg(background),
            ),
            Rect::new(text_x + marker_width as u16, row, body_width as u16, height),
        );
        if clipped {
            // Never expose a mouse target for a partially inspectable identity.
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes("Resize for full ID", body_width))
                    .style(Style::default().fg(palette.muted).bg(background)),
                Rect::new(
                    text_x + marker_width as u16,
                    row_area.bottom() - 1,
                    body_width as u16,
                    1,
                ),
            );
        } else {
            hits.push((row_area, candidate.filter().cloned()));
        }
        row = row.saturating_add(height);
    }
    hits
}

fn render_filter_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
) -> Vec<(Rect, usize)> {
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let rows = app.visible_conversations();
    let rows_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    let inset = 2u16.min(area.width / 2);
    let content_x = area.x.saturating_add(inset);
    let content_width = area.width.saturating_sub(inset.saturating_mul(2)).max(1);
    let count = Rect::new(content_x, area.y, content_width, 1);
    frame.render_widget(
        Paragraph::new(format!("{} conversations", rows.len()))
            .style(Style::default().fg(palette.muted)),
        count,
    );
    if rows_area.height == 0 {
        return Vec::new();
    }
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching conversations").style(Style::default().fg(palette.muted)),
            Rect::new(content_x, rows_area.y, content_width, 1),
        );
        return Vec::new();
    }
    let visible_count = rows_area.height as usize;
    let selected = app
        .selected
        .as_ref()
        .and_then(|id| rows.iter().position(|conversation| &conversation.id == id))
        .unwrap_or(0);
    let first = selected
        .saturating_sub(visible_count.saturating_sub(1))
        .min(rows.len().saturating_sub(visible_count));
    let mut hits = Vec::new();
    let end = (first + visible_count).min(rows.len());
    for (index, conversation) in rows.iter().enumerate().take(end).skip(first) {
        let conversation = *conversation;
        let row = Rect::new(
            rows_area.x,
            rows_area.y + (index - first) as u16,
            rows_area.width,
            1,
        );
        let active = index == selected;
        let background = if active {
            palette.selection
        } else {
            palette.input_surface
        };
        let label = app.conversation_name(conversation);
        let kind = match conversation.kind {
            bloom_domain::ConversationKind::Direct => "Direct",
            bloom_domain::ConversationKind::Group => "Group",
        };
        let label_width = (content_width as usize).saturating_sub(kind.len() + 5);
        frame.render_widget(Block::default().style(Style::default().bg(background)), row);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if active { "› " } else { "  " },
                    Style::default().fg(palette.cyan).bg(background),
                ),
                Span::styled(
                    truncate_display_graphemes(&label, label_width),
                    Style::default().fg(palette.text).bg(background),
                ),
                Span::styled(
                    format!("  {kind}"),
                    Style::default()
                        .fg(if active { palette.text } else { palette.muted })
                        .bg(background),
                ),
            ])),
            Rect::new(content_x, row.y, content_width, 1),
        );
        hits.push((row, index));
    }
    hits
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

/// Remove C0/C1 control characters so stored text can never emit an escape
/// sequence, then let the caller truncate the safe result.
/// Stored text may contain anything. Drop control and bidirectional-format
/// characters so a snippet can never emit an escape sequence or reorder the line,
/// and keep tabs as a single space so words do not run together.
fn sanitize_snippet(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| {
            if character == '\t' {
                return Some(' ');
            }
            if character.is_control() || is_bidi_format(character) {
                return None;
            }
            Some(character)
        })
        .collect()
}

fn is_bidi_format(character: char) -> bool {
    matches!(
        character,
        '\u{200e}' | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn truncate_display_graphemes(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let grapheme_width = unicode_width::UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result
}

fn display_pad(value: &str, width: usize) -> String {
    let mut result = display_truncate(value, width);
    let used = unicode_width::UnicodeWidthStr::width(result.as_str());
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

pub fn spinner_frame(tick: u64) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick as usize % FRAMES.len()]
}

/// What is under a terminal cell, resolved from what the last frame actually
/// drew. Mouse input goes through this so a hitbox cannot drift from the pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    SidebarChat(bloom_domain::ConversationId),
    Composer,
    Transcript,
    MenuRow(usize),
    SettingsTab(usize),
    SettingsRow(usize),
    ContactRow(usize),
    ContactTab(usize),
    NewContactRow(usize),
    NewContactTab(usize),
    FilterRow(usize),
    FilterTab(usize),
    SearchRow(usize),
    SearchParticipant(Option<ParticipantFilter>),
    EmojiRow(usize),
    ThemeRow(usize),
    ContextMenuRow(usize),
    HeaderSettings,
    HeaderHelp,
    HeaderChats,
    DrawerChat(bloom_domain::ConversationId),
    Drawer,
    CloseDrawer,
    Latest,
    CloseOverlay,
}

#[derive(Clone, Debug, Default)]
pub struct HitMap {
    pub sidebar: Rect,
    pub transcript: Rect,
    pub transcript_view: Option<crate::transcript::TranscriptViewport>,
    pub latest: Option<Rect>,
    pub composer: Rect,
    pub composer_positions: Vec<(u16, u16, usize)>,
    pub sidebar_rows: Vec<(Rect, bloom_domain::ConversationId)>,
    /// The command popup's outer panel and its clickable rows.
    pub menu: Option<Rect>,
    pub menu_rows: Vec<(Rect, usize)>,
    pub popup: Option<Rect>,
    pub settings_tabs: Vec<(Rect, usize)>,
    pub settings_rows: Vec<(Rect, usize)>,
    pub contacts_rows: Vec<(Rect, usize)>,
    pub contacts_tabs: Vec<(Rect, usize)>,
    pub new_contact_rows: Vec<(Rect, usize)>,
    pub new_contact_tabs: Vec<(Rect, usize)>,
    pub filter_rows: Vec<(Rect, usize)>,
    pub filter_tabs: Vec<(Rect, usize)>,
    pub search_rows: Vec<(Rect, usize)>,
    /// Painted participant-scope rows with their exact filter identity, so a
    /// click never retargets a later, changed candidate list.
    pub search_participant_rows: Vec<(Rect, Option<ParticipantFilter>)>,
    pub emoji_rows: Vec<(Rect, usize)>,
    pub theme_rows: Vec<(Rect, usize)>,
    /// The conversation context menu and its items.
    pub context_menu: Option<Rect>,
    pub context_menu_rows: Vec<(Rect, usize)>,
    pub help: Option<Rect>,
    pub header_settings: Option<Rect>,
    pub header_help: Option<Rect>,
    pub header_chats: Option<Rect>,
    pub drawer: Option<Rect>,
    pub close_drawer: Option<Rect>,
    pub drawer_rows: Vec<(Rect, bloom_domain::ConversationId)>,
}

impl HitMap {
    fn contains(area: Rect, column: u16, row: u16) -> bool {
        column >= area.x
            && column < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height)
    }

    fn row_at(rows: &[(Rect, usize)], column: u16, row: u16) -> Option<usize> {
        rows.iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, index)| *index)
    }

    /// The exact painted participant scope under a pointer. The outer `Option`
    /// is presence, the inner one is All chats (`None`) versus a named scope.
    pub fn search_participant_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<Option<ParticipantFilter>> {
        self.search_participant_rows
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
            .map(|(_, filter)| filter.clone())
    }

    pub fn composer_cursor_at(&self, column: u16, row: u16) -> Option<usize> {
        self.composer_positions
            .iter()
            .min_by_key(|(x, y, _)| {
                u32::from(y.abs_diff(row)) * 1024 + u32::from(x.abs_diff(column))
            })
            .map(|(_, _, index)| *index)
    }

    pub fn overlay_contains(&self, column: u16, row: u16) -> bool {
        self.drawer.is_some()
            || self.help.is_some()
            || [self.menu, self.context_menu, self.popup]
                .into_iter()
                .flatten()
                .any(|area| Self::contains(area, column, row))
    }

    /// Resolve a pointer position. Overlays take priority over the panes below
    /// them, which is what makes the command popup own the mouse while it is up.
    pub fn at(&self, column: u16, row: u16) -> Option<Hit> {
        if self.drawer.is_some() {
            if self
                .close_drawer
                .is_some_and(|area| Self::contains(area, column, row))
            {
                return Some(Hit::CloseDrawer);
            }
            return self
                .drawer_rows
                .iter()
                .find(|(area, _)| Self::contains(*area, column, row))
                .map(|(_, id)| Hit::DrawerChat(id.clone()))
                .or(Some(Hit::Drawer));
        }
        if self.help.is_some() {
            // Help is modal: clicks anywhere dismiss it without reaching panes
            // rendered underneath, even when the pointer is outside the sheet.
            return Some(Hit::CloseOverlay);
        }
        if let Some(menu) = self.menu {
            if Self::contains(menu, column, row) {
                return Self::row_at(&self.menu_rows, column, row)
                    .map(Hit::MenuRow)
                    .or(Some(Hit::CloseOverlay));
            }
        }
        if self
            .context_menu
            .is_some_and(|area| Self::contains(area, column, row))
        {
            return Self::row_at(&self.context_menu_rows, column, row)
                .map(Hit::ContextMenuRow)
                .or(Some(Hit::CloseOverlay));
        }
        if self
            .popup
            .is_some_and(|area| Self::contains(area, column, row))
        {
            if let Some(index) = Self::row_at(&self.settings_tabs, column, row) {
                return Some(Hit::SettingsTab(index));
            }
            if let Some(index) = Self::row_at(&self.settings_rows, column, row) {
                return Some(Hit::SettingsRow(index));
            }
            if let Some(index) = Self::row_at(&self.contacts_rows, column, row) {
                return Some(Hit::ContactRow(index));
            }
            if let Some(index) = Self::row_at(&self.contacts_tabs, column, row) {
                return Some(Hit::ContactTab(index));
            }
            if let Some(index) = Self::row_at(&self.new_contact_rows, column, row) {
                return Some(Hit::NewContactRow(index));
            }
            if let Some(index) = Self::row_at(&self.new_contact_tabs, column, row) {
                return Some(Hit::NewContactTab(index));
            }
            if let Some(index) = Self::row_at(&self.filter_rows, column, row) {
                return Some(Hit::FilterRow(index));
            }
            if let Some(index) = Self::row_at(&self.filter_tabs, column, row) {
                return Some(Hit::FilterTab(index));
            }
            if let Some(filter) = self.search_participant_at(column, row) {
                return Some(Hit::SearchParticipant(filter));
            }
            if let Some(index) = Self::row_at(&self.search_rows, column, row) {
                return Some(Hit::SearchRow(index));
            }
            if let Some(index) = Self::row_at(&self.emoji_rows, column, row) {
                return Some(Hit::EmojiRow(index));
            }
            if let Some(index) = Self::row_at(&self.theme_rows, column, row) {
                return Some(Hit::ThemeRow(index));
            }
            return Some(Hit::CloseOverlay);
        }
        if let Some(index) = Self::row_at(&self.settings_tabs, column, row) {
            return Some(Hit::SettingsTab(index));
        }
        if let Some(index) = Self::row_at(&self.settings_rows, column, row) {
            return Some(Hit::SettingsRow(index));
        }
        if let Some(index) = Self::row_at(&self.contacts_rows, column, row) {
            return Some(Hit::ContactRow(index));
        }
        if let Some(index) = Self::row_at(&self.contacts_tabs, column, row) {
            return Some(Hit::ContactTab(index));
        }
        if let Some(index) = Self::row_at(&self.new_contact_rows, column, row) {
            return Some(Hit::NewContactRow(index));
        }
        if let Some(index) = Self::row_at(&self.new_contact_tabs, column, row) {
            return Some(Hit::NewContactTab(index));
        }
        if let Some(index) = Self::row_at(&self.filter_rows, column, row) {
            return Some(Hit::FilterRow(index));
        }
        if let Some(index) = Self::row_at(&self.filter_tabs, column, row) {
            return Some(Hit::FilterTab(index));
        }
        if let Some(index) = Self::row_at(&self.emoji_rows, column, row) {
            return Some(Hit::EmojiRow(index));
        }
        if let Some(index) = Self::row_at(&self.theme_rows, column, row) {
            return Some(Hit::ThemeRow(index));
        }
        if self
            .header_chats
            .is_some_and(|area| Self::contains(area, column, row))
        {
            return Some(Hit::HeaderChats);
        }
        if self
            .header_settings
            .is_some_and(|area| Self::contains(area, column, row))
        {
            return Some(Hit::HeaderSettings);
        }
        if self
            .header_help
            .is_some_and(|area| Self::contains(area, column, row))
        {
            return Some(Hit::HeaderHelp);
        }
        if self
            .latest
            .is_some_and(|area| Self::contains(area, column, row))
        {
            return Some(Hit::Latest);
        }
        if let Some((_, id)) = self
            .sidebar_rows
            .iter()
            .find(|(area, _)| Self::contains(*area, column, row))
        {
            return Some(Hit::SidebarChat(id.clone()));
        }
        if Self::contains(self.composer, column, row) {
            return Some(Hit::Composer);
        }
        if Self::contains(self.transcript, column, row) {
            return Some(Hit::Transcript);
        }
        None
    }
}

// Keep only a few loaded-history variants, never a persistent plaintext cache.
const TRANSCRIPT_CACHE_CAPACITY: usize = 4;

struct TranscriptLayout {
    lines: Vec<Line<'static>>,
    header_lines: Vec<usize>,
    message_lines: Vec<(MessageId, usize)>,
    line_starts: Vec<usize>,
    rows: usize,
}

struct TranscriptKey {
    selected: Option<ConversationId>,
    conversation: Option<Conversation>,
    messages: Vec<Message>,
    nicknames: std::collections::HashMap<String, String>,
    palette: Palette,
    width: u16,
    new_message: bool,
}

impl TranscriptKey {
    fn messages(app: &App, new_message: bool) -> &[Message] {
        if new_message {
            return &[];
        }
        app.displayed_messages()
    }

    fn matches(&self, app: &App, width: u16, palette: Palette, new_message: bool) -> bool {
        self.width == width
            && self.palette == palette
            && self.new_message == new_message
            && self.selected == app.selected
            && self.conversation.as_ref() == app.selected_conversation()
            && self.nicknames == app.nicknames
            && self.messages.as_slice() == Self::messages(app, new_message)
    }

    fn new(app: &App, width: u16, palette: Palette, new_message: bool) -> Self {
        Self {
            selected: app.selected.clone(),
            conversation: app.selected_conversation().cloned(),
            messages: Self::messages(app, new_message).to_vec(),
            nicknames: app.nicknames.clone(),
            palette,
            width,
            new_message,
        }
    }
}

struct CachedTranscript {
    key: TranscriptKey,
    layout: TranscriptLayout,
}

/// Renderer-owned, bounded LRU of loaded transcript layouts. App remains immutable.
#[derive(Default)]
pub struct RenderCache {
    transcripts: Vec<CachedTranscript>,
    builds: usize,
}

impl RenderCache {
    /// A deterministic work counter for regression tests, not content diagnostics.
    pub fn transcript_layout_builds(&self) -> usize {
        self.builds
    }

    fn layout(
        &mut self,
        app: &App,
        width: u16,
        palette: Palette,
        new_message: bool,
        build: impl FnOnce() -> TranscriptLayout,
    ) -> &TranscriptLayout {
        if let Some(index) = self
            .transcripts
            .iter()
            .position(|entry| entry.key.matches(app, width, palette, new_message))
        {
            if index + 1 != self.transcripts.len() {
                let entry = self.transcripts.remove(index);
                self.transcripts.push(entry);
            }
        } else {
            if !new_message {
                // Keep width/theme variants, not obsolete copies of edited or
                // removed history. New-chat empty states do not evict the parked thread.
                let messages = TranscriptKey::messages(app, false);
                self.transcripts.retain(|entry| {
                    entry.key.selected != app.selected || entry.key.messages.as_slice() == messages
                });
            }
            let layout = build();
            let key = TranscriptKey::new(app, width, palette, new_message);
            if self.transcripts.len() == TRANSCRIPT_CACHE_CAPACITY {
                self.transcripts.remove(0);
            }
            self.transcripts.push(CachedTranscript { key, layout });
            self.builds += 1;
        }
        &self.transcripts.last().expect("layout was inserted").layout
    }
}

/// One dim annotation for an accepted edit or unsend. An unsent message keeps its
/// locally retained body, so the note says so rather than implying deletion.
fn mutation_note(message: &Message) -> Option<&'static str> {
    if message.unsent_at.is_some() {
        Some("Unsent · retained locally")
    } else if message.edited_at.is_some() {
        Some("Edited")
    } else {
        None
    }
}

/// One aggregate line for a message's reactions: counts per kind, kinds sorted
/// alphabetically, for example `· heart 2 · like 1`. Sender and time are left
/// out on purpose; this is a summary, not a ledger. `None` when there are no
/// reactions, so an unreacted message renders exactly as before.
fn reaction_summary(message: &Message) -> Option<String> {
    if message.reactions.is_empty() {
        return None;
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for reaction in &message.reactions {
        *counts.entry(reaction.kind.as_str()).or_default() += 1;
    }
    Some(
        counts
            .into_iter()
            .map(|(kind, count)| format!("· {kind} {count}"))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn transcript_window(
    layout: &TranscriptLayout,
    scroll: u16,
    height: u16,
    muted: bool,
    palette: Palette,
) -> (Vec<Line<'static>>, u16) {
    let top = usize::from(scroll);
    let first = layout
        .line_starts
        .partition_point(|start| *start <= top)
        .saturating_sub(1);
    let end = layout
        .line_starts
        .partition_point(|start| *start < top + usize::from(height))
        .max(first + 1)
        .min(layout.lines.len());
    let relative_scroll =
        top.saturating_sub(layout.line_starts.get(first).copied().unwrap_or_default());
    let mut lines = layout.lines[first..end].to_vec();
    if muted {
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = Style::default().fg(palette.muted).bg(palette.surface);
            }
        }
    }
    (lines, relative_scroll as u16)
}

pub fn render(frame: &mut Frame<'_>, app: &App) -> HitMap {
    render_with_mode(frame, app, monochrome())
}

pub fn render_with_cache(frame: &mut Frame<'_>, app: &App, cache: &mut RenderCache) -> HitMap {
    render_cached_with_mode(frame, app, monochrome(), cache)
}

fn render_with_mode(frame: &mut Frame<'_>, app: &App, monochrome: bool) -> HitMap {
    render_cached_with_mode(frame, app, monochrome, &mut RenderCache::default())
}

fn render_cached_with_mode(
    frame: &mut Frame<'_>,
    app: &App,
    monochrome: bool,
    cache: &mut RenderCache,
) -> HitMap {
    let mut palette = match app.theme {
        Theme::Dark => DARK,
        Theme::Light => LIGHT,
    };
    if monochrome {
        palette.bg = Color::Reset;
        palette.surface = Color::Reset;
        palette.input_surface = Color::Reset;
        palette.rule = Color::Reset;
        palette.text = Color::Reset;
        palette.muted = Color::Reset;
        palette.cyan = Color::Reset;
        palette.green = Color::Reset;
        palette.yellow = Color::Reset;
        palette.magenta = Color::Reset;
        palette.red = Color::Reset;
        palette.selection = Color::Reset;
    }
    let area = frame.area();
    // Codex runs flush to the window edges: no outer frame, content starts at the
    // first column. A background fill keeps the surface colour without a border.
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.surface).fg(palette.text)),
        area,
    );
    let inner = area;
    if app.chat_drawer.is_some() {
        return render_chat_drawer(frame, app, inner, palette);
    }
    // A terminal can be arbitrarily small. Below the size where the layout is
    // meaningful, show a compact fallback instead of asking ratatui for regions
    // outside the buffer, which panics.
    if inner.width < 20 || inner.height < 5 {
        let (prefix, value, cursor_byte, editable, muted) =
            composer_content(app, inner.width as usize);
        let compact = Rect::new(inner.x, inner.y, inner.width, u16::from(inner.height > 0));
        let layout = ComposerLayout::new(compact, &prefix, &value, cursor_byte, editable, false);
        if inner.width > 0 && inner.height > 0 {
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.input_surface)),
                compact,
            );
        }
        if layout.input.width > 0 && layout.input.height > 0 {
            render_composer_layout(frame, &layout, muted, palette, palette.input_surface);
        }
        let help = render_help(frame, app, inner, palette);
        return HitMap {
            help,
            composer: layout.input,
            composer_positions: if !muted && app.filter_query().is_none() {
                layout.cursor_positions
            } else {
                Vec::new()
            },
            ..Default::default()
        };
    }
    if app.search_participant_picker_open() && inner.width < 70 {
        return render_narrow_participant_picker(frame, app, inner, palette);
    }
    let mut hits = HitMap::default();

    // Measured from the mockup: the rail is about 21% of the window. Compact mode
    // gives more room to the transcript and uses one-line sidebar entries.
    // Below the comfortable threshold the rail collapses so chats stay identifiable.
    let compact = app.compact_sidebar || inner.width < COMFORTABLE_RAIL_MIN_WIDTH;
    let sidebar_width = sidebar_width_for(inner.width, app.compact_sidebar);
    let body = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Length(u16::from(sidebar_width > 0)),
        Constraint::Min(1),
    ])
    .split(inner);
    let visible = app.visible_conversations();
    // Keep the sidebar section label plain and quiet, rather than letter-spaced or
    // treated like a display heading.
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
        None if app.focus == Focus::Sidebar => ("› Chats".to_owned(), false),
        None => ("Chats".to_owned(), false),
    };
    let label_style = if is_filter || app.focus == Focus::Sidebar {
        Style::default()
            .fg(palette.cyan)
            .bg(palette.input_surface)
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
    // Match the transcript's header-to-content breathing room in the sidebar.
    let sidebar_spacer = u16::from(body[0].height >= 6);
    // The rail carries one item per chat: a name line and, unless the sidebar is
    // compact, a preview of the newest message. The selection fills both lines.
    const SELECTION_MARKER_WIDTH: usize = 2;
    let entry_height = if compact { 1u16 } else { 2u16 };
    let entry_width = (body[0].width as usize).saturating_sub(SELECTION_MARKER_WIDTH);
    let entries = visible
        .iter()
        .map(|conversation| {
            let mut lines = vec![sidebar_name_line(app, conversation, entry_width, palette)];
            if !compact {
                lines.push(sidebar_preview_line(conversation, entry_width, palette));
            }
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    // Render with state so a selection deep in a long list scrolls into view
    // rather than moving off-screen invisibly.
    let selected_index = app
        .selected
        .as_ref()
        .filter(|_| !app.new_conversation)
        .and_then(|id| {
            visible
                .iter()
                .position(|conversation| &conversation.id == id)
        });
    let mut list_state = ListState::default();
    list_state.select(selected_index);
    let list_area = Rect::new(
        body[0].x,
        body[0].y + 1 + sidebar_spacer,
        body[0].width,
        body[0].height.saturating_sub(1 + sidebar_spacer),
    );
    frame.render_stateful_widget(
        List::new(entries)
            .style(Style::default().bg(palette.bg))
            .highlight_style(Style::default().bg(palette.input_surface))
            .highlight_symbol("▎ ")
            .repeat_highlight_symbol(true),
        list_area,
        &mut list_state,
    );
    // The list scrolls itself, so derive each chat's rows from the state it drew
    // with. A chat scrolled out of view simply gets no hitbox.
    let offset = list_state.offset();
    hits.sidebar = body[0];
    hits.sidebar_rows = visible
        .iter()
        .enumerate()
        .filter_map(|(index, conversation)| {
            let row = (index.checked_sub(offset)? as u16).checked_mul(entry_height)?;
            (body[0].width > 0 && row + entry_height <= list_area.height).then(|| {
                (
                    Rect::new(
                        list_area.x,
                        list_area.y + row,
                        list_area.width,
                        entry_height,
                    ),
                    conversation.id.clone(),
                )
            })
        })
        .collect();

    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(palette.rule)),
        body[1],
    );
    let header_height = if inner.height >= 12 { 2u16 } else { 1u16 };
    let spacer_rows = u16::from(inner.height >= 6 && !app.settings_open && !app.help_open);
    let min_transcript_rows = if inner.height >= 12 { 3u16 } else { 1u16 };
    let composer_width = body[2].width.saturating_sub(4).max(1) as usize;
    let (prefix, value, cursor_byte, editable, muted) = composer_content(app, composer_width);
    let wrap_draft =
        (app.wrap_composer || value.contains('\n')) && editable && !muted && !value.is_empty();
    // On a very short screen, give a wrapped draft its rows before spending space
    // on the usual input padding and footer. This preserves the overflow viewport.
    let composer_overhead = if wrap_draft && inner.height < 8 {
        0u16
    } else if inner.height >= 12 {
        4u16
    } else if inner.height >= 10 {
        3u16
    } else if inner.height >= 6 {
        2u16
    } else {
        0u16
    };
    let wrapped_rows = if wrap_draft {
        crate::composer::wrap_draft(&prefix, &value, composer_width)
            .lines
            .len()
    } else {
        1
    };
    let max_input_rows = inner
        .height
        .saturating_sub(header_height + min_transcript_rows + spacer_rows + composer_overhead)
        .clamp(1, 5) as usize;
    let input_rows = wrapped_rows.min(max_input_rows).max(1) as u16;
    let composer_height = input_rows + composer_overhead;
    let right = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(composer_height),
    ])
    .split(body[2]);
    // Two columns of breathing room after the sidebar divider, the way Codex
    // indents its content, without wasting the right-hand side of the pane.
    let gutter = ratatui::layout::Margin {
        horizontal: 2,
        vertical: 0,
    };
    let transcript_area = if right[1].height > spacer_rows {
        Rect::new(
            right[1].x,
            right[1].y,
            right[1].width,
            right[1].height - spacer_rows,
        )
    } else {
        right[1]
    };
    let content = transcript_area.inner(gutter);
    let composer_area = right[2].inner(gutter);
    let geometry = composer_geometry(composer_area, input_rows);
    hits.transcript = content;
    hits.composer = geometry.input;
    let selected = app.selected_conversation();
    let new_message = app.new_conversation;
    // The left of the header leads with the chat itself; the app name lives on the
    // right, next to the other quiet affordances.
    let mut heading = Vec::new();
    if new_message {
        heading.push(Span::styled(
            "New message",
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ));
    } else if let Some(c) = selected {
        let name = app.conversation_name(c);
        let subtitle = conversation_subtitle(c);
        heading.push(Span::styled(
            name.clone(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ));
        // An untitled direct chat is named after the handle already, so a second
        // copy would just repeat it.
        if subtitle != name && right[0].width >= 32 {
            heading.extend([
                Span::raw("  "),
                Span::styled(subtitle, Style::default().fg(palette.muted)),
            ]);
        }
        if let Some(badge) = unread_badge(app, c) {
            heading.extend([
                Span::raw("  "),
                Span::styled(format!("· {badge}"), Style::default().fg(palette.muted)),
            ]);
        }
    }
    let pane = right[0];
    let gutter = 2u16.min(pane.width / 4);
    let mut left = pane.x.saturating_add(gutter);
    let right_edge = pane.x.saturating_add(pane.width).saturating_sub(gutter);
    if uses_chat_drawer(inner.width) && right_edge.saturating_sub(left) >= 18 {
        let area = Rect::new(left, pane.y, 7, 1);
        frame.render_widget(
            Paragraph::new("‹ Chats").style(Style::default().fg(palette.cyan)),
            area,
        );
        hits.header_chats = Some(area);
        left = left.saturating_add(8);
    }
    let available = right_edge.saturating_sub(left);
    let compact = available < 48 && pane.height > 1;
    let title_row = pane.y;
    let status_row = if compact && !new_message {
        pane.y + 1
    } else {
        pane.y
    };
    let context_row = (pane.height > 1 && (!compact || new_message)).then_some(pane.y + 1);

    let status_text = status_text(app).map(|status| {
        let tone = status_tone(app);
        let label = match app.connection_state {
            crate::daemon::ConnectionState::Disconnected => {
                format!("{} Offline", spinner_frame(app.frame))
            }
            crate::daemon::ConnectionState::Connecting => {
                format!("{} Connecting", spinner_frame(app.frame))
            }
            _ if status == "Syncing" => format!("{} Syncing", spinner_frame(app.frame)),
            _ => status,
        };
        (label, tone)
    });
    let mut controls_width = 0u16;
    let mut help = false;
    let mut settings = false;
    let mut theme = false;
    let mut branding = false;
    let reserve_status = status_text.as_ref().map_or(0, |(text, _)| {
        (unicode_width::UnicodeWidthStr::width(text.as_str()) as u16 + 3).min(
            if compact && !new_message {
                available
            } else {
                available.saturating_sub(10)
            },
        )
    });
    let title_min = 24u16.min(available / 2);
    let budget = available
        .saturating_sub(title_min)
        .saturating_sub(reserve_status);
    if !compact && !uses_chat_drawer(inner.width) {
        // Narrow screens spend their budget on Chats, identity and status.
        // Drop optional branding first; identity and status always get their budget.
        if controls_width + 2 <= budget {
            help = true;
            controls_width += 2;
        }
        if controls_width + 4 <= budget {
            settings = true;
            controls_width += 4;
        }
        if controls_width + 9 <= budget {
            theme = true;
            controls_width += 9;
        }
        if controls_width + 6 <= budget {
            branding = true;
            controls_width += 6;
        }
    }
    let title_width =
        available
            .saturating_sub(controls_width)
            .saturating_sub(if status_row == title_row {
                reserve_status
            } else {
                0
            });
    if branding {
        frame.render_widget(
            Paragraph::new("Bloom").style(Style::default().fg(palette.muted)),
            Rect::new(right_edge - 5, title_row, 5, 1),
        );
    }
    let mut control_x = right_edge.saturating_sub(u16::from(branding) * 6);
    if help {
        control_x = control_x.saturating_sub(1);
        let area = Rect::new(control_x, title_row, 1, 1);
        hits.header_help = Some(area);
        frame.render_widget(
            Paragraph::new("?").style(Style::default().fg(palette.muted)),
            area,
        );
        control_x = control_x.saturating_sub(1);
    }
    if settings {
        control_x = control_x.saturating_sub(3);
        let area = Rect::new(control_x, title_row, 3, 1);
        hits.header_settings = Some(Rect::new(area.x + 1, area.y, 1, 1));
        frame.render_widget(
            Paragraph::new(" ⚙ ")
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted)),
            area,
        );
    }
    let heading_width: usize = heading
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if heading_width > usize::from(title_width) {
        // The unread count is optional, so it yields before identity and status.
        if heading.len() > 1 {
            heading.truncate(heading.len().saturating_sub(2));
        }
        let remaining_width: usize = heading
            .iter()
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        if remaining_width > usize::from(title_width) {
            heading.truncate(1);
        }
    }
    let title_text: String = heading.iter().map(|span| span.content.as_ref()).collect();
    frame.render_widget(
        Paragraph::new(truncate_with_ellipsis(&title_text, title_width as usize)).style(
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(left, title_row, title_width, 1),
    );
    if theme {
        let label = match app.theme {
            Theme::Dark => " Dark ",
            Theme::Light => " Light ",
        };
        let width = unicode_width::UnicodeWidthStr::width(label) as u16 + 2;
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted))
                .block(
                    Block::default()
                        .borders(Borders::LEFT | Borders::RIGHT)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.rule)),
                ),
            Rect::new(control_x.saturating_sub(width), title_row, width, 1),
        );
    }
    if let Some((status, tone)) = status_text {
        let width = reserve_status;
        let status = truncate_with_ellipsis(&status, usize::from(width.saturating_sub(2)));
        if width >= 3 {
            let status_x = right_edge
                .saturating_sub(controls_width)
                .saturating_sub(width);
            let dot_color = match tone {
                StatusTone::Healthy => palette.green,
                StatusTone::Attention => palette.yellow,
                StatusTone::Failure => palette.red,
            };
            let label_color = if tone == StatusTone::Healthy {
                palette.muted
            } else {
                dot_color
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("● ", Style::default().fg(dot_color)),
                    Span::styled(status, Style::default().fg(label_color)),
                ])),
                Rect::new(status_x, status_row, width, 1),
            );
        }
    }
    if let Some(context_row) = context_row {
        let context_active = !new_message && app.search_context_active();
        let loading_context = context_active
            && app
                .search_context
                .as_ref()
                .is_some_and(|context| context.loading);
        let context = if new_message {
            app.pending_address
                .as_ref()
                .map(|address| format!("To: {address}"))
                .unwrap_or_else(|| "Choose a recipient below".into())
        } else if context_active {
            if loading_context {
                "Search context · loading exact message · End/Latest returns to newest".into()
            } else {
                "Search context · End/Latest returns to newest history".into()
            }
        } else {
            "─".repeat(available as usize)
        };
        frame.render_widget(
            Paragraph::new(truncate_with_ellipsis(&context, available as usize)).style(
                Style::default().fg(if new_message || context_active {
                    palette.cyan
                } else {
                    palette.rule
                }),
            ),
            Rect::new(left, context_row, available, 1),
        );
    }
    let layout = cache.layout(app, content.width.max(1), palette, new_message, || {
        let transcript_width = content.width.max(1) as usize;
        let mut lines = Vec::new();
        let mut header_lines: Vec<usize> = Vec::new();
        let mut message_lines = Vec::new();
        {
            let messages = if new_message {
                &[][..]
            } else {
                app.displayed_messages()
            };
            // Pad speaker names to a common column so timestamps line up, the way the
            // mockup's transcript does. Capped so one long name cannot push the times
            // off the visible line.
            const SPEAKER_COLUMN_CAP: usize = 12;
            let speaker_column = messages
                .iter()
                .map(|message| {
                    let label = app.speaker_label_for(message, app.selected_conversation());
                    unicode_width::UnicodeWidthStr::width(label.as_str())
                })
                .max()
                .unwrap_or(0)
                .clamp(1, SPEAKER_COLUMN_CAP);

            // Group consecutive messages from one sender the way Slack does: the name
            // and time appear once per run, a day divider marks each new day, and a
            // blank row separates runs. The boundary is the name, not just whitespace.
            const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
            let mut previous: Option<(String, bool, i64, (i32, i32))> = None;
            for message in messages {
                message_lines.push((message.id().clone(), lines.len()));
                let speaker = app.speaker_label_for(message, app.selected_conversation());
                let (year, yday, day_text) = crate::app::day_label(message.created_at);
                let new_day = previous
                    .as_ref()
                    .is_none_or(|(_, _, _, day)| *day != (year, yday));
                let new_group = new_day
                    || previous.as_ref().is_some_and(|(name, mine, at, _)| {
                        *name != speaker
                            || *mine != message.from_me
                            || message.created_at - at > GROUP_GAP_MS
                    });
                if new_day && !day_text.is_empty() {
                    if previous.is_some() {
                        lines.push(Line::raw(""));
                    }
                    let label = format!(" {day_text} ");
                    let label_width = unicode_width::UnicodeWidthStr::width(label.as_str());
                    let side = transcript_width.saturating_sub(label_width) / 2;
                    let rule = "─".repeat(side.min(6));
                    let pad = " ".repeat(side.saturating_sub(side.min(6)));
                    lines.push(Line::from(vec![
                        Span::raw(pad),
                        Span::styled(rule.clone(), Style::default().fg(palette.rule)),
                        Span::styled(label, Style::default().fg(palette.muted)),
                        Span::styled(rule, Style::default().fg(palette.rule)),
                    ]));
                } else if new_group {
                    lines.push(Line::raw(""));
                }
                previous = Some((
                    speaker.clone(),
                    message.from_me,
                    message.created_at,
                    (year, yday),
                ));
                let padded = display_pad(&speaker, speaker_column);
                let timestamp = timestamp(message.created_at);
                let message_surface = if message.from_me {
                    palette.input_surface
                } else {
                    palette.surface
                };
                let mut header = vec![
                    Span::styled(
                        padded.clone(),
                        Style::default()
                            .fg(speaker_color(&speaker, message.from_me, &palette))
                            .bg(message_surface)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", Style::default().bg(message_surface)),
                    Span::styled(
                        timestamp.clone(),
                        Style::default().fg(palette.muted).bg(message_surface),
                    ),
                ];
                let header_width = unicode_width::UnicodeWidthStr::width(padded.as_str())
                    + 2
                    + unicode_width::UnicodeWidthStr::width(timestamp.as_str());
                if message.from_me && header_width < transcript_width {
                    header.push(Span::styled(
                        " ".repeat(transcript_width - header_width),
                        Style::default().bg(message_surface),
                    ));
                }
                // Remember where each byline lands, so the viewport can avoid ending on one
                // with its body below the fold. A continuing run has no byline at all.
                if new_group {
                    header_lines.push(lines.len());
                    lines.push(Line::from(header));
                }

                let text = message_text(message);
                let mut body_style = Style::default().fg(palette.text);
                if message.from_me {
                    body_style = body_style.bg(message_surface);
                }
                let mut body_width = unicode_width::UnicodeWidthStr::width(text.as_str());
                let mut body = vec![Span::styled(text, body_style)];
                if message.from_me {
                    let state = match message.delivery {
                        // Keep the status icon attached to its word when wrapping.
                        DeliveryState::Queued => "⠋\u{00a0}queued",
                        DeliveryState::Sent => "✓\u{00a0}sent",
                        DeliveryState::Delivered => "✓\u{00a0}delivered",
                        DeliveryState::Read => "✓\u{00a0}read",
                        DeliveryState::Failed => "failed",
                    };
                    let state_style = match message.delivery {
                        DeliveryState::Queued => Style::default().add_modifier(Modifier::DIM),
                        DeliveryState::Sent => Style::default().add_modifier(Modifier::DIM),
                        DeliveryState::Delivered | DeliveryState::Read => {
                            Style::default().add_modifier(Modifier::BOLD)
                        }
                        DeliveryState::Failed => {
                            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                        }
                    }
                    .fg(if state.starts_with('✓') {
                        palette.green
                    } else {
                        palette.muted
                    })
                    .bg(message_surface);
                    let state_width = unicode_width::UnicodeWidthStr::width(state);
                    body.extend([
                        Span::styled("  ", Style::default().bg(message_surface)),
                        Span::styled(state, state_style),
                    ]);
                    body_width += 2 + state_width;
                    if body_width < transcript_width {
                        body.push(Span::styled(
                            " ".repeat(transcript_width - body_width),
                            Style::default().bg(message_surface),
                        ));
                    }
                }
                lines.push(Line::from(body));
                if let Some(note) = mutation_note(message) {
                    let mut style = Style::default().fg(palette.muted);
                    if message.from_me {
                        style = style.bg(message_surface);
                    }
                    lines.push(Line::from(Span::styled(note, style)));
                }
                if let Some(summary) = reaction_summary(message) {
                    // Styled as transcript secondary text so it sits under the body
                    // without competing with it, and wrapped by the same paragraph
                    // pass as every other body row.
                    let mut style = Style::default().fg(palette.muted);
                    if message.from_me {
                        style = style.bg(message_surface);
                    }
                    lines.push(Line::from(Span::styled(summary, style)));
                }
            }
        }
        // Show the newest messages by default and let the user scroll back. Rows are
        // counted with the same wrap the paragraph performs, so the offset lands on
        // the row the renderer will actually use; scroll 0 pins to the newest content
        // and an over-height message therefore shows its tail rather than vanishing.
        let mut line_starts: Vec<usize> = Vec::with_capacity(lines.len());
        let mut rows = 0usize;
        for line in &lines {
            let count = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(content.width.max(1))
                .max(1);
            line_starts.push(rows);
            rows += count;
        }
        TranscriptLayout {
            lines,
            header_lines,
            message_lines,
            line_starts,
            rows,
        }
    });
    let lines = &layout.lines;
    let header_lines = &layout.header_lines;
    let message_lines = &layout.message_lines;
    let line_starts = &layout.line_starts;
    let rows = layout.rows;
    let mut viewport = app
        .displayed_transcript_conversation()
        .filter(|_| !new_message)
        .map(|id| crate::transcript::TranscriptViewport {
            conversation: id.clone(),
            messages: message_lines
                .iter()
                .enumerate()
                .map(|(index, (id, line))| crate::transcript::MessageRows {
                    id: id.clone(),
                    start: line_starts[*line],
                    end: message_lines
                        .get(index + 1)
                        .map_or(rows, |(_, next)| line_starts[*next]),
                })
                .collect(),
            rows,
            height: content.height as usize,
            top: 0,
        });
    let max_scroll = rows.saturating_sub(content.height as usize);
    let mut back = viewport
        .as_ref()
        .map_or(app.scroll.min(max_scroll), |view| {
            max_scroll.saturating_sub(app.transcript_top(view))
        });
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
    if let Some(view) = viewport.as_mut() {
        view.top = usize::from(scroll);
    }
    hits.transcript_view = viewport;
    // Use the existing breathing row, not the feedback/error footer or a message
    // row. Panels and tiny layouts never acquire a hidden latest hitbox.
    let panel_open = app.help_open
        || app.settings_open
        || app.theme_panel_open
        || app.contacts_open
        || app.command_menu
        || app.filter_query().is_some()
        || app.search.is_some()
        || (app.new_conversation && app.pending_address.is_none())
        || !app.emoji_suggestions().is_empty()
        || app.context_menu.is_some();
    // A short context can sit at scroll zero and still needs a measured exit.
    let latest_available = back > 0 || app.search_context_active();
    if latest_available && spacer_rows > 0 && !panel_open && content.width >= 8 {
        let label = if content.width >= 14 {
            "↓ Latest · End"
        } else {
            "↓ Latest"
        };
        let width = unicode_width::UnicodeWidthStr::width(label) as u16;
        let area = Rect::new(
            content.right().saturating_sub(width),
            content.bottom(),
            width,
            1,
        );
        frame.render_widget(
            Paragraph::new(label).style(
                Style::default()
                    .fg(palette.cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
        hits.latest = Some(area);
    }
    if lines.is_empty() && app.filter_query().is_none() {
        let (title, key, description) = if new_message {
            if app.pending_address.is_some() {
                ("New conversation", "", "Write a message below")
            } else {
                ("New conversation", "", "Enter a phone number or email")
            }
        } else if app.search_context_active() {
            ("Search context", "", "Loading the exact message context")
        } else if app.connection_state == crate::daemon::ConnectionState::Disconnected {
            (
                "Daemon unavailable",
                "",
                "Reconnecting; drafts stay on this device",
            )
        } else if app.connection_state == crate::daemon::ConnectionState::Connecting {
            ("Connecting to Bloom", "", "Waiting for the local daemon")
        } else if app.thread_loading {
            (
                "Loading stored messages",
                "",
                "Reading this conversation from Bloom",
            )
        } else if app.filter_has_no_matches() {
            ("No matching conversations", "Esc", " clear the filter")
        } else if app.conversations.is_empty() {
            ("No conversations yet", "/new", " start a conversation")
        } else if app.selected.is_none() {
            ("Choose a conversation", "↑↓", " browse · Enter compose")
        } else {
            ("No stored messages", "", "Older history is not imported")
        };
        let empty_state = vec![
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    key,
                    Style::default()
                        .fg(palette.cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(description, Style::default().fg(palette.muted)),
            ]),
        ];
        let height = content.height.min(empty_state.len() as u16);
        let y = content
            .y
            .saturating_add(content.height.saturating_sub(height) / 2);
        frame.render_widget(
            Paragraph::new(empty_state).alignment(Alignment::Center),
            Rect::new(content.x, y, content.width, height),
        );
    } else {
        // Start at the logical line containing the visible row. Reflowing every
        // preceding message just to discard it made ordinary panel switches slow.
        let (visible_lines, relative_scroll) =
            transcript_window(layout, scroll, content.height, app.command_menu, palette);
        frame.render_widget(
            Paragraph::new(visible_lines)
                .wrap(Wrap { trim: false })
                .scroll((relative_scroll, 0))
                .style(Style::default().bg(palette.surface)),
            content,
        );
    }
    let new_recipient_open = app.new_conversation && app.pending_address.is_none();
    let filter_open = app.filter_query().is_some();
    let search_open = app.search.is_some();
    let emoji_open = !app.emoji_suggestions().is_empty();
    if app.contacts_open
        || app.settings_open
        || app.theme_panel_open
        || new_recipient_open
        || filter_open
        || search_open
        || emoji_open
    {
        let picker_bounds = Rect::new(body[2].x, content.y, body[2].width, content.height);
        let settings_layout = app.settings_open.then(|| {
            let options = if app.settings_category == 0 { 1 } else { 2 };
            settings_geometry(picker_bounds, geometry.band.y, 2, options)
        });
        let filter_layout = filter_open.then(|| {
            let rows = app.visible_conversations().len().max(1) + 1;
            titled_picker_geometry(picker_bounds, geometry.band.y, 3, rows, 0)
        });
        let contacts_layout = app.contacts_open.then(|| {
            let handles = app.visible_contact_handles();
            let body_rows = 2 + handles.len().max(1);
            titled_picker_geometry(picker_bounds, geometry.band.y, 2, body_rows, 0)
        });
        let search_layout = search_open.then(|| {
            let state = app.search.as_ref().expect("search state was checked");
            let picker_open = app.search_participant_picker_open();
            let status = u16::from(!picker_open && search_status_text(state).is_some());
            let results = if state.loading || state.error.is_some() {
                0
            } else {
                state.hits.len().min(8) as u16
            };
            let content_rows = if picker_open {
                // The title names the picker; rows hold body-query context when
                // it fits, a Find line, and a bounded candidate window.
                participant_picker_content_rows(app, picker_bounds.width)
            } else {
                // Body query, scope/discovery row, optional status, then results.
                2 + usize::from(status) + usize::from(results)
            };
            titled_picker_geometry(picker_bounds, geometry.band.y, 0, content_rows, 0)
        });
        let theme_layout = app
            .theme_panel_open
            .then(|| titled_picker_geometry(picker_bounds, geometry.band.y, 0, 3, 0));
        let panel = if let Some(settings) = settings_layout {
            settings.panel
        } else if let Some(contacts) = contacts_layout {
            contacts.panel
        } else if let Some(filter) = filter_layout {
            filter.panel
        } else if let Some(search) = search_layout {
            search.panel
        } else if let Some(theme) = theme_layout {
            theme.panel
        } else {
            let available_height = content.height;
            let panel_height = if new_recipient_open || filter_open || search_open {
                available_height.min(9)
            } else if emoji_open {
                available_height.min(8)
            } else {
                available_height.min(7)
            };
            Rect::new(
                body[2].x,
                content
                    .y
                    .saturating_add(content.height.saturating_sub(panel_height)),
                body[2].width,
                panel_height,
            )
        };
        if panel.width > 0 && panel.height > 0 {
            hits.popup = Some(panel);
            // Keep the popup attached to the shaded composer across the
            // transcript's reserved spacer row.
            let bridge_end = geometry.band.y;
            let bridge_start = panel.y.saturating_add(panel.height);
            if bridge_end > bridge_start {
                frame.render_widget(
                    Block::default().style(Style::default().bg(palette.input_surface)),
                    Rect::new(
                        panel.x,
                        bridge_start,
                        panel.width,
                        bridge_end - bridge_start,
                    ),
                );
            }
            frame.render_widget(Clear, panel);
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.input_surface)),
                panel,
            );
            let title = if app.settings_open {
                "Settings"
            } else if app.contacts_open {
                "Contacts"
            } else if new_recipient_open {
                "New conversation"
            } else if filter_open {
                "Filter conversations"
            } else if search_open {
                if app.search_participant_picker_open() {
                    "Chats with"
                } else {
                    "Search"
                }
            } else if emoji_open {
                "Emoji suggestions"
            } else {
                "Theme"
            };
            let title_area = settings_layout
                .or(contacts_layout)
                .or(filter_layout)
                .or(search_layout)
                .or(theme_layout)
                .map_or_else(
                    || {
                        Rect::new(
                            panel.x.saturating_add(1),
                            panel.y,
                            panel.width.saturating_sub(1),
                            1,
                        )
                    },
                    |picker| picker.title,
                );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    title,
                    Style::default()
                        .fg(palette.cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                title_area,
            );
            let top_inset = if panel.height >= 10 {
                2
            } else {
                u16::from(panel.height >= 6)
            };
            if app.settings_open {
                let settings = settings_layout.expect("settings geometry was measured");
                hits.settings_tabs = render_tabs(
                    frame,
                    settings.tabs,
                    &["Appearance", "Layout"],
                    app.settings_category,
                    palette,
                );
                hits.settings_rows = render_settings(
                    frame,
                    settings.rows,
                    app,
                    palette,
                    settings.show_descriptions,
                    settings.show_note,
                );
            } else {
                let rows_area = Rect::new(
                    panel.x,
                    panel.y.saturating_add(1 + top_inset),
                    panel.width,
                    panel.height.saturating_sub(1 + top_inset),
                );
                if app.contacts_open {
                    let contacts = contacts_layout.expect("contacts geometry was measured");
                    hits.contacts_tabs = render_tabs(
                        frame,
                        contacts.tabs,
                        &["All", "Nicknamed"],
                        app.contacts_tab,
                        palette,
                    );
                    hits.contacts_rows = render_contacts(frame, contacts.rows, app, palette);
                } else if new_recipient_open {
                    hits.new_contact_rows = render_new_contacts(
                        frame,
                        rows_area,
                        app,
                        palette,
                        &mut hits.new_contact_tabs,
                    );
                } else if filter_open {
                    let filter = filter_layout.expect("filter geometry was measured");
                    hits.filter_tabs = render_tabs(
                        frame,
                        filter.tabs,
                        &["All", "Direct", "Groups"],
                        app.filter_category,
                        palette,
                    );
                    hits.filter_rows = render_filter_panel(frame, filter.rows, app, palette);
                } else if search_open {
                    let search = search_layout.expect("search geometry was measured");
                    let rows = render_search(frame, search.rows, app, palette);
                    hits.search_rows = rows.results;
                    hits.search_participant_rows = rows.participants;
                } else if emoji_open {
                    hits.emoji_rows = render_emoji_suggestions(frame, rows_area, app, palette);
                } else {
                    let options = ["Automatic", "Dark", "Light"];
                    let theme = theme_layout.expect("theme geometry was measured");
                    let inset = 2u16.min(theme.rows.width / 2);
                    let text = Rect::new(
                        theme.rows.x.saturating_add(inset),
                        theme.rows.y,
                        theme.rows.width.saturating_sub(inset.saturating_mul(2)),
                        theme.rows.height,
                    );
                    for (index, option) in
                        options.iter().enumerate().take(theme.rows.height as usize)
                    {
                        let row = Rect::new(
                            theme.rows.x,
                            theme.rows.y + index as u16,
                            theme.rows.width,
                            1,
                        );
                        let selected = app.theme_panel_selection == index;
                        let bg = if selected {
                            palette.selection
                        } else {
                            palette.input_surface
                        };
                        frame.render_widget(Block::default().style(Style::default().bg(bg)), row);
                        frame.render_widget(
                            Paragraph::new(if selected {
                                format!("› {option}")
                            } else {
                                format!("  {option}")
                            })
                            .style(Style::default().fg(palette.text).bg(bg)),
                            Rect::new(text.x, row.y, text.width, 1),
                        );
                        hits.theme_rows.push((row, index));
                    }
                }
            }
        }
    }
    if let Some((panel, rows)) = render_command_menu(frame, app, content, body[2], palette) {
        hits.menu = Some(panel);
        hits.menu_rows = rows;
    }
    if let Some((panel, rows)) = render_context_menu(frame, app, &hits, palette) {
        hits.context_menu = Some(panel);
        hits.context_menu_rows = rows;
    }
    let layout = ComposerLayout::new(
        geometry.input,
        &prefix,
        &value,
        cursor_byte,
        editable,
        wrap_draft,
    );
    if !muted && app.filter_query().is_none() {
        let draft_graphemes = app.draft.graphemes(true).count();
        hits.composer_positions = layout
            .cursor_positions
            .iter()
            .copied()
            .filter(|(_, _, index)| *index <= draft_graphemes)
            .collect();
    }
    // The input surface is a band with padding above and below the input row.
    // Footer rows stay outside the shaded band.
    // The shaded band spans the full chat pane; the input text keeps its gutter.
    let input_band = Rect::new(
        body[2].x,
        geometry.band.y,
        body[2].width,
        geometry.band.height,
    );
    let input_surface = palette.input_surface;
    if input_band.width > 0 && input_band.height > 0 {
        frame.render_widget(
            Block::default().style(Style::default().bg(input_surface)),
            input_band,
        );
    }
    if layout.input.width > 0 && layout.input.height > 0 {
        render_composer_layout(frame, &layout, muted, palette, input_surface);
    }
    if let Some(footer_area) = geometry.footer {
        if footer_area.width > 0 && footer_area.height > 0 {
            let feedback = composer_feedback(app, &palette, back);
            let hints = footer_hints(
                footer_area.width as usize,
                palette,
                app,
                wrap_draft && input_rows > 1,
            );
            let row = |offset: u16| {
                Rect::new(footer_area.x, footer_area.y + offset, footer_area.width, 1)
            };
            let two_rows = footer_area.height >= 2;
            match (two_rows, feedback) {
                (true, Some((message, color))) => {
                    render_footer_row(frame, row(0), &message, color);
                    frame.render_widget(Paragraph::new(hints), row(1));
                }
                (true, None) => {
                    frame.render_widget(Paragraph::new(hints), row(1));
                }
                (false, Some((message, color))) => {
                    render_footer_row(frame, row(0), &message, color)
                }
                (false, None) => {
                    frame.render_widget(Paragraph::new(hints), row(0));
                }
            }
        }
    }
    if let Some(popup) = hits.popup {
        let excerpt_rows = popup.y.saturating_sub(content.y);
        if excerpt_rows > 0 && excerpt_rows < 3 {
            // One orphan body/byline row above a tall picker has no useful context.
            let excerpt = Rect::new(body[2].x, content.y, body[2].width, excerpt_rows);
            frame.render_widget(Clear, excerpt);
            frame.render_widget(
                Block::default().style(Style::default().bg(palette.surface)),
                excerpt,
            );
        }
    }
    // Help is a modal over the entire terminal, including the composer on
    // shorter viewports. Draw it last so the composer cannot erase its bottom.
    hits.help = render_help(frame, app, inner, palette);
    hits
}

/// Full-screen navigation owns both keyboard and mouse, including at tiny sizes.
fn render_chat_drawer(frame: &mut Frame<'_>, app: &App, area: Rect, palette: Palette) -> HitMap {
    let mut hits = HitMap {
        drawer: Some(area),
        ..Default::default()
    };
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    let gutter = if area.width >= 6 { 2 } else { 0 };
    let width = area.width.saturating_sub(gutter * 2).min(66);
    let pane = Rect::new(
        area.x + (area.width - width) / 2,
        area.y,
        width,
        area.height,
    );
    let rows = app.drawer_conversations();
    let selected = app.drawer_selection();
    let close_width = if pane.width >= 18 { 9 } else { 0 };
    frame.render_widget(
        Paragraph::new(truncate_with_ellipsis(
            &format!("Chats · {}", rows.len()),
            pane.width.saturating_sub(close_width) as usize,
        ))
        .style(
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(pane.x, pane.y, pane.width.saturating_sub(close_width), 1),
    );
    if close_width > 0 {
        let close = Rect::new(pane.right() - 8, pane.y, 8, 1);
        frame.render_widget(
            Paragraph::new("Esc back").style(Style::default().fg(palette.muted)),
            close,
        );
        hits.close_drawer = Some(close);
    }
    let header = if area.height >= 6 { 4 } else { 1 };
    if header == 4 {
        frame.render_widget(
            Paragraph::new("─".repeat(pane.width as usize))
                .style(Style::default().fg(palette.rule)),
            Rect::new(pane.x, pane.y + 1, pane.width, 1),
        );
        let query = app
            .chat_drawer
            .as_ref()
            .map_or("", |drawer| drawer.query.as_str());
        let input = Rect::new(pane.x, pane.y + 2, pane.width, 1);
        let layout = ComposerLayout::new(input, "Find: ", query, query.len(), true, false);
        render_composer_layout(frame, &layout, false, palette, palette.input_surface);
    }
    let footer = u16::from(area.height >= 2);
    let list_area = Rect::new(
        pane.x,
        pane.y + header,
        pane.width,
        pane.height.saturating_sub(header + footer),
    );
    let entry_height = if pane.height >= 12 && pane.width >= 35 {
        2
    } else {
        1
    };
    let entry_width = usize::from(pane.width.saturating_sub(2));
    let entries: Vec<_> = rows
        .iter()
        .map(|conversation| {
            let mut lines = vec![sidebar_name_line(app, conversation, entry_width, palette)];
            if entry_height == 2 {
                lines.push(sidebar_preview_line(conversation, entry_width, palette));
            }
            ListItem::new(lines)
        })
        .collect();
    let mut state = ListState::default();
    state.select(
        rows.iter()
            .position(|row| Some(&row.id) == selected.as_ref()),
    );
    frame.render_stateful_widget(
        List::new(entries)
            .highlight_symbol("› ")
            .highlight_style(Style::default().bg(palette.input_surface))
            .repeat_highlight_symbol(false),
        list_area,
        &mut state,
    );
    hits.drawer_rows = rows
        .iter()
        .enumerate()
        .filter_map(|(index, conversation)| {
            let row = (index.checked_sub(state.offset())? as u16).checked_mul(entry_height)?;
            (list_area.width > 0 && row + entry_height <= list_area.height).then(|| {
                (
                    Rect::new(
                        list_area.x,
                        list_area.y + row,
                        list_area.width,
                        entry_height,
                    ),
                    conversation.id.clone(),
                )
            })
        })
        .collect();
    if rows.is_empty() && list_area.height > 0 {
        let text = if app.conversations.is_empty() {
            "No conversations yet"
        } else {
            "No matching chats"
        };
        frame.render_widget(
            Paragraph::new(truncate_with_ellipsis(text, list_area.width as usize))
                .style(Style::default().fg(palette.muted)),
            list_area,
        );
    }
    if footer > 0 {
        let hint = if pane.width >= 37 {
            "↑↓ choose · Enter open · Esc back"
        } else if pane.width >= 27 {
            "↑↓ choose · ↵ open · Esc back"
        } else {
            "↑↓ · ↵ open · Esc back"
        };
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(hint, pane.width as usize))
                .style(Style::default().fg(palette.muted)),
            Rect::new(pane.x, pane.bottom() - 1, pane.width, 1),
        );
    }
    hits
}

/// The shortcut sheet is modal; a click anywhere dismisses without click-through.
fn render_help(frame: &mut Frame<'_>, app: &App, content: Rect, palette: Palette) -> Option<Rect> {
    if !app.help_open || content.height == 0 || content.width == 0 {
        return None;
    }
    let rows: [(&str, &str, &str); crate::app::HELP_ROW_COUNT] = [
        ("↑↓ / wheel", "cursor, chats, or scroll", "move / scroll"),
        ("PgUp / PgDn", "scroll a screenful", "page scroll"),
        ("Ctrl+Home / End", "oldest / latest", "oldest / latest"),
        ("Tab", "toggle composer / chat list", "toggle focus"),
        ("Enter", "send the message", "send"),
        ("Ctrl+J", "new line (Alt/Shift+Enter too)", "new line"),
        (
            "Ctrl+R",
            "recover a saved failed draft",
            "recover failed draft",
        ),
        (
            "Ctrl+F",
            "search stored text and filenames",
            "search messages",
        ),
        ("Ctrl+Z / Ctrl+Y", "undo / redo the draft", "undo / redo"),
        ("/new", "start a new conversation", "new chat"),
        (
            "/sync",
            "refresh conversation-list metadata",
            "refresh list",
        ),
        ("/", "commands", "commands"),
        ("?", "this sheet", "help"),
        (",", "settings", "settings"),
        ("Ctrl+B n/p", "next / previous chat", "next / prev chat"),
        ("Ctrl+P", "previous chat", "previous chat"),
        (
            "Ctrl+B f/s/t/u",
            "filter / settings / theme / unread",
            "filter / settings / theme / unread",
        ),
        ("/unread", "first unread message", "first unread"),
        ("Ctrl+C", "clear input; quit when empty", "clear / quit"),
    ];
    if content.height < 4 || content.width < 4 {
        let panel = content;
        let selected = app.help_scroll.min(rows.len() - 1);
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Block::default().style(Style::default().bg(palette.input_surface)),
            panel,
        );
        if panel.height >= 2 {
            let (key, _, compact) = rows[selected];
            let entry = format!("{key}: {compact}");
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(&entry, panel.width as usize))
                    .style(Style::default().fg(palette.text).bg(palette.input_surface)),
                Rect::new(panel.x, panel.y, panel.width, 1),
            );
        }
        if panel.height >= 3 {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(
                    "↑↓ scroll · ? close",
                    panel.width as usize,
                ))
                .style(Style::default().fg(palette.muted).bg(palette.input_surface)),
                Rect::new(panel.x, panel.y + panel.height - 1, panel.width, 1),
            );
        } else if panel.height == 1 {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(
                    "? help · ↑↓ scroll",
                    panel.width as usize,
                ))
                .style(Style::default().fg(palette.muted).bg(palette.input_surface)),
                panel,
            );
        } else {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(
                    "↑↓ scroll · ? close",
                    panel.width as usize,
                ))
                .style(Style::default().fg(palette.muted).bg(palette.input_surface)),
                Rect::new(panel.x, panel.y + 1, panel.width, 1),
            );
        }
        return Some(panel);
    }
    let width = content.width.min(56);
    let height = (rows.len() as u16 + 3).min(content.height);
    let panel = Rect::new(
        content.x + (content.width - width) / 2,
        content.y + content.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::default()
            .title(Span::styled(
                " Keyboard shortcuts ",
                Style::default().fg(palette.cyan),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.rule))
            .style(Style::default().bg(palette.input_surface)),
        panel,
    );
    let inner = panel.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let visible_rows = inner.height.saturating_sub(1) as usize;
    let first = app.help_scroll.min(rows.len().saturating_sub(visible_rows));
    for (index, (key, description, compact_description)) in
        rows.iter().skip(first).take(visible_rows).enumerate()
    {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        if inner.width < 36 {
            let text = format!("{key}: {compact_description}");
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(&text, inner.width as usize))
                    .style(Style::default().fg(palette.text)),
                row,
            );
        } else {
            let key_width = 18usize.min(inner.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        display_pad(key, key_width),
                        Style::default()
                            .fg(palette.cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_display_graphemes(
                            description,
                            (inner.width as usize).saturating_sub(key_width),
                        ),
                        Style::default().fg(palette.muted),
                    ),
                ])),
                row,
            );
        }
    }
    if inner.height > 1 {
        let hint = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        let above = first;
        let below = rows.len().saturating_sub(first + visible_rows);
        let hint_text = if inner.width < 36 {
            "↑↓ scroll · ? close".to_owned()
        } else if above > 0 || below > 0 {
            format!("↑ {above} above · ↓ {below} more · ↑↓ scroll · esc / ? close")
        } else {
            "esc / ? close".to_owned()
        };
        frame.render_widget(
            Paragraph::new(truncate_display_graphemes(&hint_text, hint.width as usize))
                .style(Style::default().fg(palette.muted)),
            hint,
        );
    }
    Some(panel)
}

/// A stable colour per speaker, so consecutive senders are easy to tell apart the
/// way a Slack-style client reads. Our own messages always use the same colour.
fn speaker_color(label: &str, from_me: bool, palette: &Palette) -> Color {
    if from_me {
        return palette.green;
    }
    let hash = label.bytes().fold(2166136261u32, |acc, byte| {
        (acc ^ byte as u32).wrapping_mul(16777619)
    });
    [palette.cyan, palette.yellow, palette.magenta][hash as usize % 3]
}

/// The right-click menu for a chat, drawn beside its sidebar row so it reads as
/// belonging to that row. Returns the panel and each action's hitbox.
fn render_context_menu(
    frame: &mut Frame<'_>,
    app: &App,
    hits: &HitMap,
    palette: Palette,
) -> Option<(Rect, Vec<(Rect, usize)>)> {
    let menu = app.context_menu.as_ref()?;
    let row = hits
        .sidebar_rows
        .iter()
        .find(|(_, id)| *id == menu.conversation)
        .map(|(area, _)| *area)?;
    let area = frame.area();
    let width = 14u16.min(area.width);
    let height = (crate::app::ContextMenu::ACTIONS.len() as u16 + 2).min(area.height);
    // Open just past the rail so the chat's name stays readable underneath.
    let x = (row.x + row.width).min(area.width.saturating_sub(width));
    let y = row.y.min(area.height.saturating_sub(height));
    let panel = Rect::new(x, y, width, height);
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.rule))
            .style(Style::default().bg(palette.input_surface)),
        panel,
    );
    let inner = panel.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut rows = Vec::new();
    for (index, (label, _)) in crate::app::ContextMenu::ACTIONS.iter().enumerate() {
        if index as u16 >= inner.height {
            break;
        }
        let item = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        let selected = index == menu.selection;
        let bg = if selected {
            palette.selection
        } else {
            palette.input_surface
        };
        let fg = palette.text;
        frame.render_widget(
            Paragraph::new(format!("{} {label}", if selected { "›" } else { " " }))
                .style(Style::default().fg(fg).bg(bg)),
            item,
        );
        rows.push((item, index));
    }
    Some((panel, rows))
}

fn render_footer_row(frame: &mut Frame<'_>, area: Rect, message: &str, color: Color) {
    frame.render_widget(
        Paragraph::new(truncate_display_graphemes(message, area.width as usize))
            .style(Style::default().fg(color)),
        area,
    );
}

/// The one-line status under the composer, if there is anything worth saying.
/// Precedence: an error, then a notice, then send feedback, then the reading-back
/// cue. Hints are always rendered on their own row when two rows are available.
fn composer_feedback(app: &App, palette: &Palette, back: usize) -> Option<(String, Color)> {
    if let Some(message) = app.error.as_deref() {
        let message = if app.deferred_sends.is_empty() {
            message.to_owned()
        } else if app
            .deferred_sends
            .front()
            .is_some_and(|draft| draft.uncertain)
        {
            "Outcome uncertain; verify before retry. Ctrl+R: saved draft.".into()
        } else {
            format!("Ctrl+R: saved draft. {message}")
        };
        return Some((message, palette.red));
    }
    if let Some(message) = app.notice.as_deref() {
        return Some((message.to_owned(), palette.green));
    }
    if let Some(feedback) = app.visible_send_feedback() {
        let (message, color) = match feedback.phase {
            crate::app::SendPhase::Sending => ("Sending…", palette.yellow),
            crate::app::SendPhase::Sent if feedback.conversation.is_none() => {
                ("New-chat message sent.", palette.green)
            }
            crate::app::SendPhase::Sent => ("Message sent.", palette.green),
        };
        let message = if app.deferred_sends.is_empty() {
            message.to_owned()
        } else if app.pending_send_attempt.is_some() {
            format!("{message} {} saved draft(s).", app.deferred_sends.len())
        } else {
            format!(
                "{message} Ctrl+R: {} saved draft(s).",
                app.deferred_sends.len()
            )
        };
        return Some((message, color));
    }
    if !app.deferred_sends.is_empty() {
        return Some((
            format!(
                "Ctrl+R: recover {} saved draft(s).",
                app.deferred_sends.len()
            ),
            palette.yellow,
        ));
    }
    // Older-history feedback sits below everything actionable, so a page failure
    // never hides a send result or a saved draft the user can recover.
    if let Some(message) = app.older_error.as_deref() {
        return Some((message.to_owned(), palette.red));
    }
    if app
        .older_loading
        .as_ref()
        .is_some_and(|id| app.selected.as_ref() == Some(id))
    {
        return Some(("Loading older messages…".to_owned(), palette.muted));
    }
    if app.is_scrolled_back() && back > 0 {
        return Some((
            "↑ reading back · End returns to the newest".to_owned(),
            palette.cyan,
        ));
    }
    None
}

#[cfg(test)]
mod animation_accessibility_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn no_color_decision_handles_unset_empty_and_nonempty() {
        assert!(!monochrome_from(None));
        assert!(!monochrome_from(Some(OsString::new())));
        assert!(monochrome_from(Some(OsString::from("1"))));
    }

    #[test]
    fn spinner_cycles_through_all_frames() {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        for (tick, expected) in frames.iter().enumerate() {
            assert_eq!(spinner_frame(tick as u64), *expected);
        }
        assert_eq!(spinner_frame(10), frames[0]);
    }

    #[test]
    fn selected_chat_marker_remains_visible_without_color() {
        let id = bloom_domain::ConversationId::new(
            bloom_domain::TransportId::new("openbubbles").unwrap(),
            "selected",
        )
        .unwrap();
        let conversation = bloom_domain::Conversation {
            id: id.clone(),
            remote_id: "selected".into(),
            kind: bloom_domain::ConversationKind::Direct,
            title: Some("Selected chat".into()),
            participants: vec![bloom_domain::Participant {
                handle: bloom_domain::Handle::new(bloom_domain::HandleKind::Opaque, "me").unwrap(),
                display_name: None,
                is_me: true,
            }],
            created_at: 1,
            last_message_at: Some(1),
            aliases: Vec::new(),
            preview: Some("Selected preview".into()),
        };
        for focus in [Focus::Composer, Focus::Sidebar] {
            let app = App {
                selected: Some(id.clone()),
                conversations: vec![conversation.clone()],
                focus,
                ..Default::default()
            };
            let backend = ratatui::backend::TestBackend::new(100, 30);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_with_mode(frame, &app, true);
                })
                .unwrap();
            let grid = terminal.backend().buffer();
            assert!(
                (0..grid.area.height)
                    .any(|y| (0..grid.area.width).any(|x| grid[(x, y)].symbol() == "▎")),
                "the selected-chat marker remains visible without color"
            );
            for y in 0..grid.area.height {
                for x in 0..grid.area.width {
                    assert_eq!(grid[(x, y)].fg, Color::Reset);
                    assert_eq!(grid[(x, y)].bg, Color::Reset);
                }
            }
        }
    }

    #[test]
    fn popup_selection_has_a_readable_marker_with_and_without_color() {
        for monochrome in [false, true] {
            let mut app = App::default();
            app.open_settings();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(66, 18)).unwrap();
            let mut hits = HitMap::default();
            terminal
                .draw(|frame| hits = render_with_mode(frame, &app, monochrome))
                .unwrap();
            let row = hits.settings_rows[0].0;
            let cell = &terminal.backend().buffer()[(row.x + 2, row.y)];
            assert_eq!(cell.symbol(), "›");
            assert!(cell.modifier.contains(Modifier::BOLD));
            if monochrome {
                assert_eq!(cell.fg, Color::Reset);
                assert_eq!(cell.bg, Color::Reset);
            } else {
                assert_eq!(cell.fg, DARK.cyan);
                assert_eq!(cell.bg, DARK.selection);
                assert_ne!(cell.fg, DARK.bg);
            }
        }
    }

    #[test]
    fn monochrome_preserves_rendered_text_but_changes_styles() {
        let app = App {
            connection: "Connecting".into(),
            connection_state: crate::daemon::ConnectionState::Connecting,
            ..Default::default()
        };
        let render = |mono| {
            let backend = ratatui::backend::TestBackend::new(100, 30);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    render_with_mode(frame, &app, mono);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        let color = render(false);
        let mono = render(true);
        for y in 0..30 {
            for x in 0..100 {
                let a = &color[(x, y)];
                let b = &mono[(x, y)];
                assert_eq!(a.symbol(), b.symbol());
            }
        }
        assert!((0..30).any(|y| (0..100).any(|x| color[(x, y)].fg != mono[(x, y)].fg)));
        for y in 0..30 {
            for x in 0..100 {
                assert_eq!(mono[(x, y)].fg, Color::Reset);
                assert_eq!(mono[(x, y)].bg, Color::Reset);
            }
        }
    }
}
