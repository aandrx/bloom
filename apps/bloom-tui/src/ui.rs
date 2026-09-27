use crate::app::{
    conversation_subtitle, message_text, status_text, status_tone, timestamp, App, ComposerPrompt,
    StatusTone, Theme,
};

use bloom_domain::DeliveryState;
use ratatui::{prelude::*, widgets::*};
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

fn monochrome_from(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn monochrome() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| monochrome_from(std::env::var_os("NO_COLOR")))
}

#[derive(Clone, Copy)]
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
}

const DARK: Palette = Palette {
    bg: Color::Rgb(30, 30, 46),
    surface: Color::Rgb(24, 24, 37),
    input_surface: Color::Rgb(49, 50, 68),
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
    input_surface: Color::Rgb(239, 239, 239),
    rule: Color::Rgb(199, 199, 199),
    text: Color::Rgb(0, 0, 0),
    muted: Color::Rgb(127, 127, 127),
    cyan: Color::Rgb(0, 117, 138),
    green: Color::Rgb(40, 117, 47),
    yellow: Color::Rgb(128, 78, 0),
    red: Color::Rgb(160, 30, 45),
};

/// Below this header width the identity and the theme chip stop fitting, so the
/// chip is dropped. It has a name so the guard can mutate one value and prove the
/// narrow layout still keeps the control.
const THEME_MIN_WIDTH: u16 = 38;

#[derive(Clone)]
struct ComposerLayout {
    input: Rect,
    footer: Option<Rect>,
    prompt: String,
    cursor: Option<(u16, u16)>,
}

impl ComposerLayout {
    fn new(
        rect: Rect,
        prefix: &str,
        value: &str,
        cursor_byte: usize,
        editable: bool,
        allow_border: bool,
    ) -> Self {
        let bordered = allow_border && rect.height >= 2;
        let input_height = rect.height.min(if bordered {
            if rect.height >= 4 {
                3
            } else {
                2
            }
        } else {
            1
        });
        let input = Rect::new(rect.x, rect.y, rect.width, input_height);
        let footer = (rect.height > input_height).then(|| {
            Rect::new(
                rect.x,
                rect.y + input_height,
                rect.width,
                rect.height - input_height,
            )
        });
        let width = usize::from(rect.width);
        let prefix_limit = width.saturating_sub(if editable { width.min(10) } else { 0 });
        let visible_prefix = truncate_display_graphemes(prefix, prefix_limit);
        let prefix_width = unicode_width::UnicodeWidthStr::width(visible_prefix.as_str());
        let available = width.saturating_sub(prefix_width);
        let graphemes: Vec<(usize, &str)> = value.grapheme_indices(true).collect();
        let cursor_byte = cursor_byte.min(value.len());
        let cursor_index = graphemes.partition_point(|(start, _)| *start < cursor_byte);
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
        let mut used = 0;
        for (_, grapheme) in graphemes.iter().skip(start_index) {
            let grapheme_width = unicode_width::UnicodeWidthStr::width(*grapheme);
            if used + grapheme_width > available {
                break;
            }
            visible_value.push_str(grapheme);
            used += grapheme_width;
        }
        let mut prompt = visible_prefix;
        prompt.push_str(&visible_value);
        let text_y = rect.y.saturating_add(u16::from(bordered));
        let cursor =
            (editable && rect.width > 0 && input_height > u16::from(bordered)).then(|| {
                let column = prefix_width.saturating_add(cursor_width);
                let x = rect
                    .x
                    .saturating_add(column.min(width.saturating_sub(1)) as u16);
                (x, text_y)
            });
        Self {
            input,
            footer,
            prompt,
            cursor,
        }
    }
}

fn composer_content(app: &App, width: usize) -> (String, String, usize, bool, bool) {
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
        ComposerPrompt::Body(address) => (
            if width < 24 {
                "›  ".to_owned()
            } else {
                body_prefix(&address, width)
            },
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

fn body_prefix(address: &str, width: usize) -> String {
    const LEAD: &str = "›  To ";
    const TAIL: &str = " - ";
    let prefix_budget = (width / 4).max(9).min(width.saturating_sub(10));
    let address_budget = prefix_budget
        .saturating_sub(unicode_width::UnicodeWidthStr::width(LEAD))
        .saturating_sub(unicode_width::UnicodeWidthStr::width(TAIL));
    if address_budget == 0 {
        return format!("{LEAD}…{TAIL}");
    }
    let shown_address = if unicode_width::UnicodeWidthStr::width(address) <= address_budget {
        address.to_owned()
    } else if address_budget == 1 {
        "…".to_owned()
    } else {
        format!(
            "{}…",
            truncate_display_graphemes(address, address_budget - 1)
        )
    };
    format!("{LEAD}{shown_address}{TAIL}")
}

fn sidebar_entry_line(
    app: &App,
    conversation: &bloom_domain::Conversation,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let selected = Some(&conversation.id) == app.selected.as_ref();
    let marker_width = 1usize.min(width);
    let count = conversation.participants.len().to_string();
    let count_width = unicode_width::UnicodeWidthStr::width(count.as_str()).min(width);
    let gap_width = usize::from(width > marker_width + count_width);
    let name_budget = width.saturating_sub(marker_width + count_width + gap_width);
    let name = truncate_display_graphemes(&app.conversation_name(conversation), name_budget);
    let name_width = unicode_width::UnicodeWidthStr::width(name.as_str());
    let gap = width.saturating_sub(marker_width + name_width + count_width);
    let marker = if selected { "▎" } else { " " };
    let name_style = Style::default()
        .fg(if selected { palette.cyan } else { palette.text })
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    Line::from(vec![
        Span::styled(marker, Style::default().fg(palette.cyan)),
        Span::styled(name, name_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            count,
            Style::default().fg(if selected {
                palette.cyan
            } else {
                palette.muted
            }),
        ),
    ])
}

fn footer_hints(width: usize, palette: Palette, app: &App) -> Line<'static> {
    // The Codex-shaped home hints: arrows scroll the transcript, Tab moves to the
    // chat list, and the rest is discoverable through commands and the sheet.
    const HOME_FULL: [(&str, &str); 5] = [
        ("↑↓", " scroll"),
        ("Tab", " chats"),
        ("Enter", " send"),
        ("/", " commands"),
        ("?", " help"),
    ];
    const HOME_MEDIUM: [(&str, &str); 4] = [
        ("↑↓", " scroll"),
        ("Tab", " chats"),
        ("/", " commands"),
        ("?", " help"),
    ];
    const HOME_COMPACT: [(&str, &str); 3] =
        [("↑↓", " scroll"), ("/", " cmds"), ("?", " help")];
    const HOME_NARROW: [(&str, &str); 2] = [("↑↓", " scroll"), ("/", " cmds")];
    const HOME_TIGHT: [(&str, &str); 1] = [("↑↓", "")];
    const LEADER_FULL: [(&str, &str); 6] = [
        ("h/l", " chats"),
        ("j/k", " scroll"),
        ("n", " new"),
        ("s", " settings"),
        ("t", " theme"),
        ("Esc", " cancel"),
    ];
    const LEADER_COMPACT: [(&str, &str); 2] = [("hjkl", " navigate"), ("Esc", " cancel")];
    const FILTER_FULL: [(&str, &str); 3] =
        [("↑↓", " select"), ("Enter", " open"), ("Esc", " cancel")];
    const FILTER_MEDIUM: [(&str, &str); 3] = [("↑↓", " select"), ("Enter", ""), ("Esc", "")];
    const FILTER_NARROW: [(&str, &str); 2] = [("↑↓", " select"), ("Esc", "")];
    const FILTER_TIGHT: [(&str, &str); 2] = [("↑↓", ""), ("Esc", "")];
    const RECIPIENT_FULL: [(&str, &str); 3] =
        [("Enter", " continue"), ("Esc", " cancel"), ("←→", " edit")];
    const RECIPIENT_MEDIUM: [(&str, &str); 2] = [("Enter", " continue"), ("Esc", " cancel")];
    const RECIPIENT_COMPACT: [(&str, &str); 2] = [("↵", " next"), ("Esc", " cancel")];
    const RECIPIENT_TIGHT: [(&str, &str); 2] = [("↵", " next"), ("Esc", "")];
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
    const SETTINGS_FULL: [(&str, &str); 3] =
        [("↑↓", " select"), ("Enter", " change"), ("Esc", " close")];
    const SETTINGS_TIGHT: [(&str, &str); 1] = [("Esc", " close")];

    let hints: &[(&str, &str)] = if app.leader_pending {
        if width >= 64 {
            &LEADER_FULL
        } else if width >= 27 {
            &LEADER_COMPACT
        } else {
            &LEADER_COMPACT[..1]
        }
    } else if app.settings_open {
        if width >= 32 {
            &SETTINGS_FULL
        } else {
            &SETTINGS_TIGHT
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
    } else if app.filter_query().is_some() {
        if width >= 35 {
            &FILTER_FULL
        } else if width >= 23 {
            &FILTER_MEDIUM
        } else if width >= 15 {
            &FILTER_NARROW
        } else if width >= 8 {
            &FILTER_TIGHT
        } else {
            &FILTER_TIGHT[..1]
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

/// The slash-command popup. It floats above the composer over a cleared panel so
/// it takes priority over the transcript, the way a command palette should. The
/// returned rectangle is the hitbox the mouse layer uses.
fn render_command_menu(
    frame: &mut Frame<'_>,
    app: &App,
    content: Rect,
    composer: Rect,
    palette: Palette,
) -> Option<(Rect, Vec<(Rect, usize)>)> {
    if !app.command_menu || content.height < 3 || content.width < 12 {
        return None;
    }
    let suggestions = app.command_suggestions();
    let item_count = suggestions.len().max(1);
    let visible = item_count
        .min(8)
        .min((content.height as usize).saturating_sub(2))
        .max(1);
    let first = app
        .command_selection
        .saturating_sub(visible.saturating_sub(1))
        .min(item_count.saturating_sub(visible));
    let panel_width = content.width.min(64);
    let panel_height = (visible + 2).min(content.height as usize) as u16;
    let area = Rect::new(
        content.x + (content.width - panel_width) / 2,
        content.y + content.height.saturating_sub(panel_height),
        panel_width,
        panel_height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(Span::styled(" Commands ", Style::default().fg(palette.muted)))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.rule))
            .style(Style::default().bg(palette.surface)),
        area,
    );
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let row_width = inner.width as usize;
    let mut menu_rows: Vec<(Rect, usize)> = Vec::new();
    for row in 0..visible {
        let row_area = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        if row_area.y >= inner.y + inner.height {
            break;
        }
        let row_index = first + row;
        if row_index < suggestions.len() {
            menu_rows.push((row_area, row_index));
        }
        let selected = !suggestions.is_empty() && row_index == app.command_selection;
        let row_bg = if selected { palette.cyan } else { palette.surface };
        frame.render_widget(Block::default().style(Style::default().bg(row_bg)), row_area);
        if suggestions.is_empty() {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes("No matching commands", row_width))
                    .style(Style::default().fg(palette.muted).bg(row_bg)),
                row_area,
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
                    .fg(if selected { palette.bg } else { palette.cyan })
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.extend([
            Span::styled(
                shown_name,
                Style::default()
                    .fg(if selected { palette.bg } else { palette.cyan })
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
                    .fg(if selected { palette.bg } else { palette.muted })
                    .bg(row_bg),
            ),
        ]);
        frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
    }
    // The panel sits above the composer; its hitbox is the panel itself.
    let _ = composer;
    Some((area, menu_rows))
}


/// Draw the settings panel and return the rect of each selectable row, so the
/// mouse layer can toggle exactly what was drawn.
/// Draw the settings pane and return the rect of each selectable row. It is a
/// left-aligned list filling the content area, the way a Codex picker fills the
/// pane, rather than a small floating box.
fn render_settings(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    palette: Palette,
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
    let settings: [(&str, String, &str); 2] = [
        (
            "Theme",
            theme_value,
            "Follows the terminal background unless pinned to Dark or Light.",
        ),
        (
            "Sidebar",
            sidebar_value.to_owned(),
            "Comfortable names each chat and its count; Compact uses one line.",
        ),
    ];
    let indent = 1u16.min(area.width);
    let x = area.x + indent;
    let width = area.width.saturating_sub(indent).max(1);
    let mut y = area.y;
    for (index, (label, value, description)) in settings.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let row = Rect::new(x, y, width, 1);
        rows.push((row, index));
        let selected = app.settings_selection == index;
        let background = if selected {
            palette.cyan
        } else {
            palette.surface
        };
        let foreground = if selected { palette.bg } else { palette.text };
        frame.render_widget(
            Block::default().style(Style::default().bg(background)),
            row,
        );
        let label_width = 12usize.min(width.saturating_sub(2) as usize);
        let value_width = (width as usize).saturating_sub(label_width + 2);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default()
                        .fg(if selected { palette.bg } else { palette.cyan })
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
            row,
        );
        y += 1;
        if y < area.y + area.height {
            frame.render_widget(
                Paragraph::new(truncate_display_graphemes(description, width as usize))
                    .style(Style::default().fg(palette.muted).bg(palette.surface)),
                Rect::new(x, y, width, 1),
            );
            y += 1;
        }
        y += 1;
    }
    if y < area.y + area.height {
        frame.render_widget(
            Paragraph::new("Theme and sidebar choices are stored locally.")
                .style(Style::default().fg(palette.muted).bg(palette.surface)),
            Rect::new(x, y, width, 1),
        );
    }
    rows
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
    SettingsRow(usize),
    HeaderSettings,
    HeaderHelp,
    CloseOverlay,
}

#[derive(Clone, Debug, Default)]
pub struct HitMap {
    pub sidebar: Rect,
    pub transcript: Rect,
    pub composer: Rect,
    pub sidebar_rows: Vec<(Rect, bloom_domain::ConversationId)>,
    /// The command popup's outer panel and its clickable rows.
    pub menu: Option<Rect>,
    pub menu_rows: Vec<(Rect, usize)>,
    pub settings_rows: Vec<(Rect, usize)>,
    pub help: Option<Rect>,
    pub header_settings: Option<Rect>,
    pub header_help: Option<Rect>,
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

    /// Resolve a pointer position. Overlays take priority over the panes below
    /// them, which is what makes the command popup own the mouse while it is up.
    pub fn at(&self, column: u16, row: u16) -> Option<Hit> {
        if let Some(help) = self.help {
            if Self::contains(help, column, row) {
                return Some(Hit::CloseOverlay);
            }
        }
        if let Some(menu) = self.menu {
            if Self::contains(menu, column, row) {
                return Self::row_at(&self.menu_rows, column, row)
                    .map(Hit::MenuRow)
                    .or(Some(Hit::CloseOverlay));
            }
        }
        if let Some(index) = Self::row_at(&self.settings_rows, column, row) {
            return Some(Hit::SettingsRow(index));
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

pub fn render(frame: &mut Frame<'_>, app: &App) -> HitMap {
    render_with_mode(frame, app, monochrome())
}

fn render_with_mode(frame: &mut Frame<'_>, app: &App, monochrome: bool) -> HitMap {
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
        palette.red = Color::Reset;
    }
    let area = frame.area();
    // Codex runs flush to the window edges: no outer frame, content starts at the
    // first column. A background fill keeps the surface colour without a border.
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.surface).fg(palette.text)),
        area,
    );
    let inner = area;
    // A terminal can be arbitrarily small. Below the size where the layout is
    // meaningful, show a compact fallback instead of asking ratatui for regions
    // outside the buffer, which panics.
    if inner.width < 20 || inner.height < 5 {
        let (prefix, value, cursor_byte, editable, muted) =
            composer_content(app, inner.width as usize);
        let compact = Rect::new(inner.x, inner.y, inner.width, u16::from(inner.height > 0));
        let layout = ComposerLayout::new(compact, &prefix, &value, cursor_byte, editable, false);
        if layout.input.width > 0 && layout.input.height > 0 {
            frame.render_widget(
                Paragraph::new(layout.prompt).style(
                    Style::default()
                        .fg(if muted { palette.muted } else { palette.text })
                        .bg(palette.input_surface),
                ),
                layout.input,
            );
            if let Some(cursor) = layout.cursor {
                frame.set_cursor_position(cursor);
            }
        }
        return HitMap {
            composer: layout.input,
            ..Default::default()
        };
    }
    let mut hits = HitMap::default();

    // Measured from the mockup: the rail is about 21% of the window. Compact mode
    // gives more room to the transcript and uses one-line sidebar entries.
    let rail_percent = if app.compact_sidebar { 15 } else { 21 };
    let desired = (inner.width as u32 * rail_percent + 50) / 100;
    // Preserve transcript room on narrow terminals; each chat remains identifiable
    // even when the title is clipped to a smaller responsive rail.
    let sidebar_min = if inner.width < 45 {
        10
    } else if inner.width < 70 {
        12
    } else {
        16
    };
    let sidebar_width = desired
        .clamp(sidebar_min, 28)
        .min(inner.width.saturating_sub(3) as u32) as u16;
    let body = Layout::horizontal([
        Constraint::Length(sidebar_width),
        Constraint::Length(1),
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
        None => ("Chats".to_owned(), false),
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
        .map(|conversation| {
            ListItem::new(sidebar_entry_line(
                app,
                conversation,
                body[0].width as usize,
                palette,
            ))
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
    let list_area = Rect::new(
        body[0].x,
        body[0].y + 1,
        body[0].width,
        body[0].height.saturating_sub(1),
    );
    frame.render_stateful_widget(
        List::new(entries).style(Style::default().bg(palette.bg)),
        list_area,
        &mut list_state,
    );
    // The list scrolls itself, so derive each chat's row from the state it drew
    // with. A row scrolled out of view simply gets no hitbox.
    let offset = list_state.offset();
    hits.sidebar = body[0];
    hits.sidebar_rows = visible
        .iter()
        .enumerate()
        .filter_map(|(index, conversation)| {
            let row = index.checked_sub(offset)? as u16;
            (row < list_area.height).then(|| {
                (
                    Rect::new(list_area.x, list_area.y + row, list_area.width, 1),
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
    // Shrink the header and composer blocks on short terminals so their heights can
    // never exceed the area they are given.
    let (header_height, composer_height) = if inner.height >= 12 {
        (2u16, 3u16)
    } else if inner.height >= 6 {
        (1u16, 3u16)
    } else if inner.height >= 5 {
        (1u16, 2u16)
    } else {
        (1u16, 1u16)
    };
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
    let content = right[1].inner(gutter);
    let composer = right[2].inner(gutter);
    hits.transcript = content;
    hits.composer = composer;
    let selected = app.selected_conversation();
    let mut heading = vec![Span::styled(
        "Bloom",
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    )];
    if app.settings_open {
        heading.extend([
            Span::raw(" / "),
            Span::styled(
                "Settings",
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    } else if right[0].width >= 38 {
        if let Some(c) = selected {
            let name = app.conversation_name(c);
            let subtitle = conversation_subtitle(c);
            heading.extend([
                Span::raw(" / "),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            // An untitled direct chat is named after the handle already, so a
            // second copy would just repeat it.
            if subtitle != name {
                heading.extend([
                    Span::raw("  "),
                    Span::styled(subtitle, Style::default().fg(palette.muted)),
                ]);
            }
        }
    }
    let mut title_width = right[0].width;
    let mut theme_control = None;
    let mut status_control = None;
    // Small clickable controls at the right edge, the way Codex tucks its
    // affordances there: `?` for the shortcut sheet, `⚙` for settings.
    let mut cursor = right[0].x.saturating_add(right[0].width);
    if right[0].width >= 26 {
        let area = Rect::new(cursor.saturating_sub(1), right[0].y, 1, 1);
        hits.header_help = Some(area);
        frame.render_widget(
            Paragraph::new("?").style(Style::default().fg(palette.muted)),
            area,
        );
        cursor = area.x;
    }
    if right[0].width >= 30 {
        let area = Rect::new(cursor.saturating_sub(3), right[0].y, 3, 1);
        hits.header_settings = Some(Rect::new(area.x + 1, area.y, 1, 1));
        frame.render_widget(
            Paragraph::new(" ⚙ ")
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted)),
            area,
        );
        cursor = area.x;
    }
    let mut status_edge = cursor;
    if right[0].width >= 38 {
        let label = match app.theme {
            Theme::Dark => " Dark ",
            Theme::Light => " Light ",
        };
        let width = unicode_width::UnicodeWidthStr::width(label) as u16 + 2;
        let x = cursor.saturating_sub(width);
        theme_control = Some((label, x, width));
        title_width = x.saturating_sub(right[0].x.saturating_add(1));
        status_edge = x;
    }
    if let Some(mut status) = status_text(app) {
        let tone = status_tone(app);
        if matches!(
            app.connection_state,
            crate::daemon::ConnectionState::Connecting
                | crate::daemon::ConnectionState::Disconnected
        ) {
            status = format!("{} {status}", spinner_frame(app.frame));
        }
        let max_status_width =
            usize::from(status_edge.saturating_sub(right[0].x.saturating_add(7)));
        if max_status_width >= 8 {
            if unicode_width::UnicodeWidthStr::width(status.as_str()) + 2 > max_status_width {
                status = match app.connection_state {
                    crate::daemon::ConnectionState::Connecting => {
                        format!("{} Connecting", spinner_frame(app.frame))
                    }
                    crate::daemon::ConnectionState::Disconnected => {
                        format!("{} Offline", spinner_frame(app.frame))
                    }
                    _ => status,
                };
            }
            let text_budget = max_status_width.saturating_sub(2);
            let status = truncate_display_graphemes(&status, text_budget);
            let status_width = (2 + unicode_width::UnicodeWidthStr::width(status.as_str()))
                .min(max_status_width) as u16;
            if status_width > 2 {
                let status_x = status_edge.saturating_sub(status_width + 1);
                title_width = status_x.saturating_sub(right[0].x.saturating_add(1));
                status_control = Some((status, status_x, status_width, tone));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(heading)),
        Rect::new(right[0].x, right[0].y, title_width, 1),
    );
    if let Some((label, x, width)) = theme_control {
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
            Rect::new(x, right[0].y, width, 1),
        );
    }
    if let Some((status, x, width, tone)) = status_control {
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
            Rect::new(x, right[0].y, width, 1),
        );
    }
    if right[0].height > 1 {
        frame.render_widget(
            Paragraph::new("─".repeat(right[0].width as usize))
                .style(Style::default().fg(palette.rule)),
            Rect::new(right[0].x, right[0].y + 1, right[0].width, 1),
        );
    }
    let transcript_width = content.width.max(1) as usize;
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
                let label = app.speaker_label_for(message, app.selected_conversation());
                unicode_width::UnicodeWidthStr::width(label.as_str())
            })
            .max()
            .unwrap_or(0)
            .clamp(1, SPEAKER_COLUMN_CAP);

        for message in messages {
            let speaker = app.speaker_label_for(message, app.selected_conversation());
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
                        .fg(palette.text)
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
            // with its body below the fold.
            header_lines.push(lines.len());
            lines.push(Line::from(header));

            let text = message_text(message);
            let mut body_style = Style::default().fg(palette.text);
            if message.from_me {
                body_style = body_style.bg(message_surface);
            }
            let mut body_width = unicode_width::UnicodeWidthStr::width(text.as_str());
            let mut body = vec![Span::styled(text, body_style)];
            if message.from_me {
                let state = match message.delivery {
                    DeliveryState::Queued => "⠋ queued",
                    DeliveryState::Sent => "✓ sent",
                    DeliveryState::Delivered => "✓ delivered",
                    DeliveryState::Read => "✓ read",
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
            // One quiet spacer keeps messages distinct without leaving a large void.
            lines.push(Line::raw(""));
        }
    }
    // Show the newest messages by default and let the user scroll back. Rows are
    // counted with the same wrap the paragraph performs, so the offset lands on
    // the row the renderer will actually use; scroll 0 pins to the newest content
    // and an over-height message therefore shows its tail rather than vanishing.
    if app.command_menu {
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = Style::default().fg(palette.muted).bg(palette.surface);
            }
        }
    }
    let width = transcript_width;
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
    if app.settings_open {
        hits.settings_rows = render_settings(frame, content, app, palette);
    } else if lines.is_empty() {
        let (title, key, description) = if app.filter_has_no_matches() {
            ("No matching conversations", "Esc", " clear the filter")
        } else if app.conversations.is_empty() {
            ("No conversations yet", "n", " start a conversation")
        } else if app.selected.is_none() {
            ("Choose a conversation", "↑↓", " browse · Enter compose")
        } else {
            ("No messages yet", "Enter", " write the first message")
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
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .style(Style::default().bg(palette.surface)),
            content,
        );
    }
    if let Some((panel, rows)) = render_command_menu(frame, app, content, composer, palette) {
        hits.menu = Some(panel);
        hits.menu_rows = rows;
    }
    hits.help = render_help(frame, app, content, palette);
    let (prefix, value, cursor_byte, editable, muted) = if app.settings_open {
        // The settings pane carries its own copy; leave the composer row quiet and
        // keep the footer hints visible underneath it.
        (String::new(), String::new(), 0, false, true)
    } else {
        composer_content(app, composer.width as usize)
    };
    // Codex's composer is bare: a prompt glyph and the draft, no frame and no
    // filled box. The feedback and hint rows sit underneath.
    let layout = ComposerLayout::new(composer, &prefix, &value, cursor_byte, editable, false);
    if layout.input.width > 0 && layout.input.height > 0 {
        frame.render_widget(
            Paragraph::new(layout.prompt.clone()).style(Style::default().fg(if muted {
                palette.muted
            } else {
                palette.text
            })),
            layout.input,
        );
    }
    if let Some(cursor) = layout.cursor {
        frame.set_cursor_position(cursor);
    }
    if let Some(footer_area) = layout.footer {
        if footer_area.width > 0 && footer_area.height > 0 {
            let feedback = composer_feedback(app, &palette, back);
            let row = |offset: u16| Rect::new(footer_area.x, footer_area.y + offset, footer_area.width, 1);
            let two_rows = footer_area.height >= 2;
            match (two_rows, feedback) {
                (true, Some((message, color))) => {
                    render_footer_row(frame, row(0), &message, color);
                    frame.render_widget(
                        Paragraph::new(footer_hints(footer_area.width as usize, palette, app)),
                        row(1),
                    );
                }
                (true, None) => {
                    frame.render_widget(
                        Paragraph::new(footer_hints(footer_area.width as usize, palette, app)),
                        row(1),
                    );
                }
                (false, Some((message, color))) => render_footer_row(frame, row(0), &message, color),
                (false, None) => {
                    frame.render_widget(
                        Paragraph::new(footer_hints(footer_area.width as usize, palette, app)),
                        row(0),
                    );
                }
            }
        }
    }
    hits
}

/// The `?` shortcut sheet, mirroring Codex's grouped key list. Returns its panel
/// so a click anywhere on it dismisses.
fn render_help(frame: &mut Frame<'_>, app: &App, content: Rect, palette: Palette) -> Option<Rect> {
    if !app.help_open || content.height < 4 || content.width < 20 {
        return None;
    }
    let rows: [(&str, &str); 12] = [
        ("↑↓ / wheel", "scroll the transcript"),
        ("PgUp / PgDn", "scroll a screenful"),
        ("Ctrl+Home / End", "oldest / latest"),
        ("Tab", "focus the chat list"),
        ("Enter", "send the message"),
        ("/", "commands"),
        ("?", "this sheet"),
        (",", "settings"),
        ("Ctrl+B h/l", "previous / next chat"),
        ("Ctrl+B s", "settings"),
        ("Ctrl+W", "delete a word"),
        ("Ctrl+C", "quit"),
    ];
    let width = content.width.min(56);
    let height = (rows.len() as u16 + 2).min(content.height);
    let panel = Rect::new(
        content.x + (content.width - width) / 2,
        content.y + content.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::default()
            .title(Span::styled(" Keyboard shortcuts ", Style::default().fg(palette.cyan)))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.rule))
            .style(Style::default().bg(palette.surface)),
        panel,
    );
    let inner = panel.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    for (index, (key, description)) in rows.iter().enumerate() {
        if index as u16 >= inner.height.saturating_sub(1) {
            break;
        }
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
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
    if inner.height > 1 {
        let hint = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new("esc / ? close").style(Style::default().fg(palette.muted)),
            hint,
        );
    }
    Some(panel)
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
        return Some((message.to_owned(), palette.red));
    }
    if let Some(message) = app.notice.as_deref() {
        return Some((message.to_owned(), palette.green));
    }
    if let Some(feedback) = app.visible_send_feedback() {
        let (message, color) = match feedback.phase {
            crate::app::SendPhase::Sending => ("Sending…", palette.yellow),
            crate::app::SendPhase::Sent => ("Message sent.", palette.green),
        };
        return Some((message.to_owned(), color));
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
