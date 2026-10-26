//! Standalone GPUI spike for a three-pane Bloom messenger surface.
//!
//! This binary is intentionally synthetic: no daemon, IPC, account or engine
//! code is involved. Run with no arguments for the window, `--measure` for the
//! headless transcript benchmark, or `--frame-measure` for GPUI callback timing.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, fill, hsla, point, prelude::*, px, relative, rgb, rgba, size, uniform_list, App,
    Application, Bounds, ClickEvent, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, InspectorElementId,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, Render, ScrollStrategy, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use unicode_segmentation::UnicodeSegmentation;

const WINDOW_WIDTH: f32 = 1200.0;
const WINDOW_HEIGHT: f32 = 780.0;
const MIN_WIDTH: f32 = 480.0;
const MIN_HEIGHT: f32 = 520.0;
const CONVERSATIONS_WIDTH: f32 = 220.0;
const NARROW_CONVERSATIONS_WIDTH: f32 = 160.0;
const DETAILS_WIDTH: f32 = 260.0;
const MESSAGE_COUNT: usize = 10_000;
// Uniform rows must accommodate the worst synthetic combination: three body
// lines plus metadata, attachment and reaction rows. Clipping would make the
// virtualization result look faster than the actual surface.
const ROW_HEIGHT: f32 = 176.0;
const DEFAULT_MEASURE_ROWS: usize = 10_000;
const MAX_MEASURE_ROWS: usize = 100_000;
const MEASURE_SLICES: usize = 100;
// Approximately the number of 176 px rows visible in the 780 px window.
const VIEWPORT_ROWS: usize = 6;
const FRAME_MEASURE_FRAMES: usize = 120;
// Throttled compositor runs have taken 28 s for 120 callbacks, so the watchdog
// only exists to catch a wedged loop rather than to bound normal runtime.
const FRAME_MEASURE_TIMEOUT: Duration = Duration::from_secs(180);

actions!(
    spike,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Quit,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Launch {
    Window,
    Measure { rows: usize },
    FrameMeasure { rows: usize },
}

fn parse_args<I>(args: I) -> Result<Launch, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut measure = false;
    let mut frame_measure = false;
    let mut rows: Option<usize> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--measure" => {
                if measure {
                    return Err("--measure specified more than once".into());
                }
                measure = true;
            }
            "--frame-measure" => {
                if frame_measure {
                    return Err("--frame-measure specified more than once".into());
                }
                frame_measure = true;
            }
            "--rows" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--rows requires a value".to_string())?;
                if rows.is_some() {
                    return Err("--rows specified more than once".into());
                }
                rows = Some(parse_rows(value)?);
            }
            value if value.starts_with("--rows=") => {
                if rows.is_some() {
                    return Err("--rows specified more than once".into());
                }
                rows = Some(parse_rows(&value["--rows=".len()..])?);
            }
            value => return Err(format!("unrecognized argument: {value}")),
        }
        index += 1;
    }

    if measure && frame_measure {
        return Err("--measure and --frame-measure are mutually exclusive".into());
    }
    if rows.is_some() && !measure && !frame_measure {
        return Err("--rows requires --measure or --frame-measure".into());
    }

    Ok(if measure {
        Launch::Measure {
            rows: rows.unwrap_or(DEFAULT_MEASURE_ROWS),
        }
    } else if frame_measure {
        Launch::FrameMeasure {
            rows: rows.unwrap_or(DEFAULT_MEASURE_ROWS),
        }
    } else {
        Launch::Window
    })
}

fn parse_rows(value: &str) -> Result<usize, String> {
    let rows: usize = value
        .parse()
        .map_err(|_| format!("invalid row count: {value}"))?;
    if !(1..=MAX_MEASURE_ROWS).contains(&rows) {
        return Err(format!(
            "row count must be between 1 and {MAX_MEASURE_ROWS}, got {rows}"
        ));
    }
    Ok(rows)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Message {
    index: usize,
    sender: String,
    time: String,
    body: String,
    outgoing: bool,
    edited: bool,
    unsent: bool,
    reactions: Vec<String>,
    attachment: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Conversation {
    name: &'static str,
    preview: &'static str,
    time: &'static str,
    unread: Option<u32>,
}

#[derive(Clone, Debug)]
struct Model {
    messages: Vec<Message>,
    conversations: Vec<Conversation>,
}

const SENDERS: [&str; 6] = ["Ada", "Grace", "Linus", "Katherine", "Margaret", "Alan"];

const BODIES: [&str; 12] = [
    "Synthetic transcript row. No daemon, IPC, or account is attached.",
    "Café notes: naïve façade, résumé, and a ☕ by the keyboard.",
    "東京からの合成メッセージです。文字幅と折り返しを確認します。",
    "مرحبا بالعالم، هذه رسالة تجريبية متعددة الأسطر.",
    "Line one of a synthetic multiline message.\nLine two keeps wrapping in Unicode: 你好, мир, مرحبا.",
    "Attachment preview placeholder keeps the row height predictable.",
    "A quiet row that exercises the virtualized viewport boundary.",
    "Nickel-and-dime details: sender, timestamp, flags, reactions.",
    "Final line of a three-line fixture with an emoji 🚀 and a combining mark e\u{301}.",
    "Reaction and edited-state surfaces are synthetic and deterministic.",
    "Unsigned outgoing message that was never sent in this offline fixture.",
    "One more synthetic line to fill the fixed-height transcript row.",
];

impl Model {
    fn synthetic(count: usize) -> Self {
        let mut messages = Vec::with_capacity(count);
        for index in 0..count {
            let outgoing = index % 3 == 0;
            let sender = if outgoing {
                "You".to_string()
            } else {
                SENDERS[index % SENDERS.len()].to_string()
            };
            let seconds = 9 * 3600 + index * 97;
            let time = format!("{:02}:{:02}", (seconds / 3600) % 24, (seconds / 60) % 60);
            let mut body = BODIES[index % BODIES.len()].to_string();
            if index % 7 == 0 {
                body.push_str("\nWrapped synthetic continuation: ＡＢＣ ｄｅｆ １２３.");
            }

            let mut reactions = Vec::new();
            if index % 11 == 0 {
                reactions.push("👍 2".to_string());
            }
            if index % 29 == 0 {
                reactions.push("🎉 1".to_string());
            }

            let attachment = if index % 23 == 0 {
                Some(format!(
                    "attachment-{index:04}.png · {} KB",
                    120 + index % 400
                ))
            } else {
                None
            };

            messages.push(Message {
                index,
                sender,
                time,
                body,
                outgoing,
                edited: index % 13 == 0,
                unsent: outgoing && index % 41 == 0,
                reactions,
                attachment,
            });
        }

        Self {
            messages,
            conversations: vec![
                Conversation {
                    name: "Synthetic Studio",
                    preview: "Ada: benchmark fixture ready",
                    time: "09:42",
                    unread: Some(3),
                },
                Conversation {
                    name: "Ada Lovelace",
                    preview: "The virtualized rows look steady.",
                    time: "09:31",
                    unread: None,
                },
                Conversation {
                    name: "Grace Hopper",
                    preview: "Multiline Unicode wrapped cleanly.",
                    time: "08:58",
                    unread: Some(1),
                },
                Conversation {
                    name: "Linus T.",
                    preview: "Attachment placeholder is expected.",
                    time: "Yesterday",
                    unread: None,
                },
                Conversation {
                    name: "Katherine Johnson",
                    preview: "Checksums are deterministic.",
                    time: "Yesterday",
                    unread: None,
                },
                Conversation {
                    name: "Margaret Hamilton",
                    preview: "No sending in this spike.",
                    time: "Mon",
                    unread: None,
                },
            ],
        }
    }

    fn checksum(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        for message in &self.messages {
            mix(&mut hash, message.index as u64);
            mix(&mut hash, u64::from(message.outgoing));
            mix(&mut hash, message.sender.len() as u64);
            mix_str(&mut hash, &message.sender);
            mix_str(&mut hash, &message.time);
            mix_str(&mut hash, &message.body);
            mix(&mut hash, u64::from(message.edited));
            mix(&mut hash, u64::from(message.unsent));
            mix(&mut hash, message.reactions.len() as u64);
            for reaction in &message.reactions {
                mix_str(&mut hash, reaction);
            }
            match &message.attachment {
                Some(value) => {
                    mix(&mut hash, 1);
                    mix_str(&mut hash, value);
                }
                None => mix(&mut hash, 0),
            }
        }
        hash
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn mix(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(FNV_PRIME);
}

fn mix_str(hash: &mut u64, value: &str) {
    for byte in value.as_bytes() {
        mix(hash, u64::from(*byte));
    }
    mix(hash, 0xff);
}

/// Clamped viewport slice. An offset past the end yields an empty slice.
fn viewport_slice(total: usize, offset: usize, height: usize) -> Range<usize> {
    if total == 0 || height == 0 {
        return 0..0;
    }
    let height = height.min(total);
    let start = offset.min(total - height);
    start..start + height
}

fn slice_checksum(messages: &[Message], iterations: usize, viewport_rows: usize) -> u64 {
    if messages.is_empty() || iterations == 0 || viewport_rows == 0 {
        return FNV_OFFSET;
    }
    let height = viewport_rows.min(messages.len());
    let max_offset = messages.len() - height;
    let mut hash = FNV_OFFSET;
    for iteration in 0..iterations {
        let offset = if iterations > 1 {
            max_offset.saturating_mul(iteration) / (iterations - 1)
        } else {
            0
        };
        for message in &messages[viewport_slice(messages.len(), offset, height)] {
            mix(&mut hash, message.index as u64);
            mix_str(&mut hash, &message.body);
            mix(&mut hash, u64::from(message.outgoing));
        }
    }
    hash
}

fn run_measure(rows: usize) {
    let build_start = Instant::now();
    let model = Model::synthetic(rows);
    let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
    let model_checksum = model.checksum();

    let slice_start = Instant::now();
    let checksum = model_checksum ^ slice_checksum(&model.messages, MEASURE_SLICES, VIEWPORT_ROWS);
    let slices_ms = slice_start.elapsed().as_secs_f64() * 1000.0;

    println!(
        "rows={rows} build_ms={build_ms:.3} slices_ms={slices_ms:.3} checksum={checksum:016x}"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
}

impl Theme {
    fn toggled(self) -> Self {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Palette {
    window_bg: u32,
    sidebar_bg: u32,
    chat_bg: u32,
    details_bg: u32,
    border: u32,
    text: u32,
    muted: u32,
    subtle: u32,
    bubble_incoming: u32,
    bubble_outgoing: u32,
    bubble_border_incoming: u32,
    bubble_border_outgoing: u32,
    composer_bg: u32,
    input_bg: u32,
    button_bg: u32,
    accent: u32,
    unread_text: u32,
}

fn palette(theme: Theme) -> Palette {
    match theme {
        Theme::Dark => Palette {
            window_bg: 0x111318,
            sidebar_bg: 0x171a21,
            chat_bg: 0x14171d,
            details_bg: 0x171a21,
            border: 0x272c36,
            text: 0xdfe3ea,
            muted: 0x8992a3,
            subtle: 0x6f7887,
            bubble_incoming: 0x1d222b,
            bubble_outgoing: 0x2b3a63,
            bubble_border_incoming: 0x2b313d,
            bubble_border_outgoing: 0x3a4d7d,
            composer_bg: 0x14171d,
            input_bg: 0x1d222b,
            button_bg: 0x303a4f,
            accent: 0x6d8dff,
            unread_text: 0xffffff,
        },
        Theme::Light => Palette {
            window_bg: 0xf3f5f8,
            sidebar_bg: 0xe9edf3,
            chat_bg: 0xf7f8fb,
            details_bg: 0xe9edf3,
            border: 0xd3d9e3,
            text: 0x1b2028,
            muted: 0x5b6473,
            subtle: 0x767f8d,
            bubble_incoming: 0xffffff,
            bubble_outgoing: 0xdbe4ff,
            bubble_border_incoming: 0xd8dee8,
            bubble_border_outgoing: 0xb9c8f2,
            composer_bg: 0xf7f8fb,
            input_bg: 0xffffff,
            button_bg: 0xdbe4ff,
            accent: 0x3b5bdb,
            unread_text: 0xffffff,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutMode {
    Wide,
    Compact,
    Narrow,
}

impl LayoutMode {
    fn for_width(width: f32) -> Self {
        if width >= 1100.0 {
            LayoutMode::Wide
        } else if width >= 800.0 {
            LayoutMode::Compact
        } else {
            LayoutMode::Narrow
        }
    }

    fn shows_details(self) -> bool {
        matches!(self, LayoutMode::Wide)
    }

    fn sidebar_width(self) -> f32 {
        match self {
            LayoutMode::Narrow => NARROW_CONVERSATIONS_WIDTH,
            LayoutMode::Wide | LayoutMode::Compact => CONVERSATIONS_WIDTH,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LayoutMode::Wide => "wide",
            LayoutMode::Compact => "compact",
            LayoutMode::Narrow => "narrow",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EditorState {
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
}

impl EditorState {
    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    fn select_all(&mut self) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
    }

    fn left(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    fn right(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    fn home(&mut self) {
        self.move_to(0);
    }

    fn end(&mut self) {
        self.move_to(self.content.len());
    }

    fn backspace(&mut self) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()));
        }
        self.replace_text(None, "");
    }

    fn delete(&mut self) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()));
        }
        self.replace_text(None, "");
    }

    fn replace_text(&mut self, range: Option<Range<usize>>, new_text: &str) {
        let range = range
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let start = range.start.min(self.content.len());
        let end = range.end.min(self.content.len()).max(start);
        self.content.replace_range(start..end, new_text);
        self.selected_range = start + new_text.len()..start + new_text.len();
        self.selection_reversed = false;
        self.marked_range = None;
    }

    fn replace_and_mark(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let start = range.start.min(self.content.len());
        let end = range.end.min(self.content.len()).max(start);
        self.content.replace_range(start..end, new_text);
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(start..start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|selected| {
                let sel_start = start + offset_from_utf16(new_text, selected.start);
                let sel_end = start + offset_from_utf16(new_text, selected.end);
                sel_start..sel_end
            })
            .unwrap_or_else(|| start + new_text.len()..start + new_text.len());
        self.selection_reversed = false;
    }

    fn unmark(&mut self) {
        self.marked_range = None;
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        offset_from_utf16(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        offset_to_utf16(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in text.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

struct TextElement {
    input: Entity<Composer>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let state = &input.state;
        let content = state.content.clone();
        let selected_range = state.selected_range.clone();
        let cursor = state.cursor_offset();
        let style = window.text_style();
        let p = palette(input.theme);

        let (display_text, text_color) = if content.is_empty() {
            (
                SharedString::from("Message #synthetic-studio (spike composer)"),
                hsla(0., 0., 0., 0.2),
            )
        } else {
            (SharedString::from(content), rgb(p.text).into())
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = state.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    rgb(p.accent),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x3311ff30),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().unwrap();
        line.paint(bounds.origin, window.line_height(), window, cx)
            .unwrap();

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

struct Composer {
    focus_handle: FocusHandle,
    state: EditorState,
    theme: Theme,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl Composer {
    fn new(cx: &mut Context<Self>, theme: Theme) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            state: EditorState::default(),
            theme,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.state.backspace();
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.state.delete();
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.state.left();
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.state.right();
        cx.notify();
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.state
            .select_to(self.state.previous_boundary(self.state.cursor_offset()));
        cx.notify();
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.state
            .select_to(self.state.next_boundary(self.state.cursor_offset()));
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.state.home();
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.state.end();
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        self.focus_handle.focus(window);
        if event.modifiers.shift {
            self.state
                .select_to(self.index_for_mouse_position(event.position));
        } else {
            self.state
                .move_to(self.index_for_mouse_position(event.position));
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.state
                .select_to(self.index_for_mouse_position(event.position));
            cx.notify();
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.state.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.state.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.state.range_from_utf16(&range_utf16);
        actual_range.replace(self.state.range_to_utf16(&range));
        Some(self.state.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.state.range_to_utf16(&self.state.selected_range),
            reversed: self.state.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.state
            .marked_range
            .as_ref()
            .map(|range| self.state.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.state.unmark();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.state.range_from_utf16(range_utf16));
        self.state.replace_text(range, new_text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state
            .replace_and_mark(range_utf16, new_text, new_selected_range_utf16);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.state.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // `localize` already returns a position relative to the element origin,
        // so `line_point.x` is the local x used for hit testing.
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(line_point.x)?;
        Some(self.state.offset_to_utf16(utf8_index))
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(self.theme);
        let draft = if self.state.content.is_empty() {
            "(empty)".to_string()
        } else {
            self.state.content.replace('\n', "\\n")
        };
        let marked = self
            .state
            .marked_range
            .as_ref()
            .map(|range| format!("{}..{}", range.start, range.end))
            .unwrap_or_else(|| "none".to_string());
        let status = format!(
            "draft: {draft} · selection {}..{} · marked {marked} · {} theme · no sending",
            self.state.selected_range.start,
            self.state.selected_range.end,
            self.theme.label()
        );

        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.composer_bg))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(p.input_bg))
                    .border_1()
                    .border_color(rgb(p.border))
                    .key_context("Composer")
                    .track_focus(&self.focus_handle)
                    .cursor(CursorStyle::IBeam)
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::left))
                    .on_action(cx.listener(Self::right))
                    .on_action(cx.listener(Self::select_left))
                    .on_action(cx.listener(Self::select_right))
                    .on_action(cx.listener(Self::select_all))
                    .on_action(cx.listener(Self::home))
                    .on_action(cx.listener(Self::end))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(24.))
                            .text_size(px(14.))
                            .line_height(px(22.))
                            .child(TextElement { input: cx.entity() }),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(p.button_bg))
                            .text_xs()
                            .text_color(rgb(p.muted))
                            .child("Send (disabled)"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(p.subtle))
                    .child(div().flex_1().min_w_0().truncate().child(status))
                    .child("Editable spike composer"),
            )
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct FrameMeasure {
    rows: usize,
    row_hashes: Vec<u64>,
    scroll_handle: UniformListScrollHandle,
    frames: usize,
    target: usize,
    start: Instant,
    last: Instant,
    per_frame: Vec<f64>,
    checksum: u64,
    last_top: usize,
}

fn row_for_tick(rows: usize, frame: usize, target: usize) -> usize {
    if target <= 1 {
        return 0;
    }
    rows.saturating_sub(1).saturating_mul(frame.min(target - 1)) / (target - 1)
}

// The applied scroll offset is the only observable proof that a scroll request
// reached layout. `UniformListScrollState::base_handle` is public and the
// uniform list keeps its pixel offset there.
fn observed_top(handle: &UniformListScrollHandle, rows: usize) -> usize {
    let offset = handle.0.borrow().base_handle.offset();
    let scrolled = (-f32::from(offset.y) / ROW_HEIGHT).floor();
    if scrolled <= 0.0 {
        return 0;
    }
    (scrolled as usize).min(rows.saturating_sub(1))
}

struct BloomSpike {
    model: Model,
    theme: Theme,
    composer: Entity<Composer>,
    scroll_handle: UniformListScrollHandle,
}

impl BloomSpike {
    fn toggle_theme(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.theme = self.theme.toggled();
        let theme = self.theme;
        self.composer.update(cx, |composer, cx| {
            composer.theme = theme;
            cx.notify();
        });
        cx.notify();
    }
}

impl Render for BloomSpike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let layout = LayoutMode::for_width(width);
        let p = palette(self.theme);
        let message_count = self.model.messages.len();
        let toggle = cx.listener(Self::toggle_theme);

        let mut root = div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(p.window_bg))
            .text_color(rgb(p.text))
            .child(conversations_pane(
                &self.model.conversations,
                &p,
                layout.sidebar_width(),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(rgb(p.chat_bg))
                    .child(chat_header(self.theme, &p, layout, message_count, toggle))
                    .child(
                        div().flex_1().min_h_0().overflow_hidden().child(
                            uniform_list(
                                "synthetic-transcript",
                                message_count,
                                cx.processor(|this, range: Range<usize>, _window, _cx| {
                                    let p = palette(this.theme);
                                    range
                                        .map(|index| {
                                            message_row(&this.model.messages[index], index, &p)
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(self.scroll_handle.clone())
                            .h_full(),
                        ),
                    )
                    .child(self.composer.clone()),
            );

        if layout.shows_details() {
            root = root.child(details_pane(&p));
        }

        root
    }
}

fn conversations_pane(conversations: &[Conversation], p: &Palette, width: f32) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_1().px_2().py_2();
    for (index, conversation) in conversations.iter().enumerate() {
        let selected = index == 0;
        list = list.child(
            div()
                .id(index)
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(if selected {
                    rgb(p.bubble_incoming)
                } else {
                    rgb(p.sidebar_bg)
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(conversation.name),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(p.muted))
                                .child(conversation.time),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap_2()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(conversation.preview),
                        )
                        .child(match conversation.unread {
                            Some(count) => div()
                                .px_1()
                                .rounded_full()
                                .bg(rgb(p.accent))
                                .text_color(rgb(p.unread_text))
                                .child(format!("{count}"))
                                .into_any_element(),
                            None => div().into_any_element(),
                        }),
                ),
        );
    }

    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(width))
        .h_full()
        .bg(rgb(p.sidebar_bg))
        .border_r_1()
        .border_color(rgb(p.border))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .py_4()
                .border_b_1()
                .border_color(rgb(p.border))
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Bloom"),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(p.input_bg))
                        .text_xs()
                        .text_color(rgb(p.subtle))
                        .child("Search conversations"),
                ),
        )
        .child(list)
        .child(
            div()
                .px_4()
                .py_3()
                .text_xs()
                .text_color(rgb(p.subtle))
                .child("Synthetic offline fixtures"),
        )
}

fn chat_header(
    theme: Theme,
    p: &Palette,
    layout: LayoutMode,
    message_count: usize,
    toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let mut chips = div().flex().flex_row().flex_none().gap_2().items_center();
    // A narrow window cannot fit three labelled chips beside the title, so only
    // the interactive control survives; the layout state is already visible
    // from the pane arrangement.
    if layout != LayoutMode::Narrow {
        chips = chips.child(
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(p.input_bg))
                .text_xs()
                .text_color(rgb(p.muted))
                .child(format!("Switch to {} theme", theme.toggled().label())),
        );
    }
    chips = chips.child(
        div()
            .id("theme-toggle")
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(p.button_bg))
            .text_xs()
            .text_color(rgb(p.text))
            .cursor_pointer()
            .on_click(toggle)
            .child(format!("{} theme", theme.label())),
    );
    if layout != LayoutMode::Narrow {
        chips = chips.child(
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(p.input_bg))
                .text_xs()
                .text_color(rgb(p.muted))
                .child(if layout.shows_details() {
                    "Details visible"
                } else {
                    "Details hidden"
                }),
        );
    }

    div()
        .flex()
        .flex_row()
        .gap_3()
        .items_center()
        .justify_between()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(p.border))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .truncate()
                        .child("Synthetic Studio"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .truncate()
                        .child(format!(
                            "{message_count}-message fixture · offline · {} layout",
                            layout.label()
                        )),
                ),
        )
        .child(chips)
}

fn message_row(message: &Message, index: usize, p: &Palette) -> impl IntoElement {
    let mut meta = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .text_xs()
        .text_color(rgb(p.muted))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if message.outgoing {
                    rgb(p.accent)
                } else {
                    rgb(p.text)
                })
                .child(message.sender.clone()),
        )
        .child(div().child(message.time.clone()));

    if message.edited {
        meta = meta.child(div().text_color(rgb(p.subtle)).child("edited"));
    }
    if message.unsent {
        meta = meta.child(
            div()
                .px_1()
                .rounded_sm()
                .bg(rgb(0x3a2529))
                .text_color(rgb(0xff9d9d))
                .child("not sent"),
        );
    }

    let bubble_bg = if message.outgoing {
        rgb(p.bubble_outgoing)
    } else {
        rgb(p.bubble_incoming)
    };
    let bubble_border = if message.outgoing {
        rgb(p.bubble_border_outgoing)
    } else {
        rgb(p.bubble_border_incoming)
    };

    let mut bubble = div()
        .max_w(px(560.))
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(bubble_border)
        .bg(bubble_bg)
        .child(
            div()
                .text_sm()
                .line_height(px(20.))
                .line_clamp(3)
                .child(message.body.clone()),
        );

    if let Some(attachment) = &message.attachment {
        bubble = bubble.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(p.input_bg))
                .text_xs()
                .text_color(rgb(p.muted))
                .child(div().text_color(rgb(p.accent)).child("▣"))
                .child(div().truncate().child(attachment.clone())),
        );
    }

    if !message.reactions.is_empty() {
        let mut reactions = div().flex().flex_row().gap_1();
        for reaction in &message.reactions {
            reactions = reactions.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_full()
                    .bg(rgb(p.input_bg))
                    .text_xs()
                    .child(reaction.clone()),
            );
        }
        bubble = bubble.child(reactions);
    }

    let mut row = div()
        .id(index)
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px_4()
        .overflow_hidden();

    row = if message.outgoing {
        row.items_end().child(meta)
    } else {
        row.items_start().child(meta)
    };

    row.child(bubble)
}

fn details_pane(p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(DETAILS_WIDTH))
        .h_full()
        .bg(rgb(p.details_bg))
        .border_l_1()
        .border_color(rgb(p.border))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .px_4()
                .py_5()
                .border_b_1()
                .border_color(rgb(p.border))
                .child(
                    div()
                        .size(px(56.))
                        .rounded_full()
                        .bg(rgb(p.bubble_outgoing))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("SS"),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Synthetic Studio"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(p.muted))
                        .child("6 synthetic participants · fixture only"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .px_4()
                .py_4()
                .text_xs()
                .text_color(rgb(p.muted))
                .child(section("About", "Offline GPUI rendering spike.", p))
                .child(section(
                    "Shared media",
                    "Attachment placeholders throughout.\nNo files are read or opened.",
                    p,
                ))
                .child(section(
                    "Pinned",
                    "Transcript rows are uniform height and virtualized.",
                    p,
                ))
                .child(section(
                    "Limits",
                    "Composer input is local. No sending, no daemon.",
                    p,
                )),
        )
}

fn section(title: &'static str, body: &'static str, p: &Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(p.text))
                .child(title),
        )
        .child(div().line_height(px(18.)).child(body))
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn frame_tick(state: &Rc<RefCell<Option<FrameMeasure>>>, window: &mut Window) -> bool {
    let mut guard = state.borrow_mut();
    let Some(measure) = guard.as_mut() else {
        return true;
    };
    let now = Instant::now();
    let frame = measure.frames + 1;
    let target = measure.target;
    let rows = measure.rows;
    measure.frames = frame;
    measure
        .per_frame
        .push(now.duration_since(measure.last).as_secs_f64() * 1000.0);
    measure.last = now;

    // Record the position the list actually reached before requesting the next
    // one. If a scroll request never reaches layout, this stays at 0 and the
    // printed value exposes that instead of implying movement.
    measure.last_top = observed_top(&measure.scroll_handle, rows);

    let row = row_for_tick(rows, frame, target);
    measure
        .scroll_handle
        .scroll_to_item(row, ScrollStrategy::Top);
    let viewport = viewport_slice(rows, row, VIEWPORT_ROWS);
    let mut checksum = measure.checksum;
    for index in viewport {
        mix(&mut checksum, index as u64);
        mix(&mut checksum, measure.row_hashes[index]);
    }
    measure.checksum = checksum;

    if frame >= target {
        let total_ms = now.duration_since(measure.start).as_secs_f64() * 1000.0;
        let mut sorted = measure.per_frame.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let mean_ms = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let p95_ms = percentile(&sorted, 95.0);
        let last_top = measure.last_top;
        println!(
            "rows={rows} frames={target} total_ms={total_ms:.3} mean_ms={mean_ms:.3} p95_ms={p95_ms:.3} last_top={last_top} checksum={checksum:016x}"
        );
        *guard = None;
        return true;
    }

    window.refresh();
    false
}

fn schedule_frame_tick(state: Rc<RefCell<Option<FrameMeasure>>>, window: &mut Window) {
    let next = state.clone();
    window.on_next_frame(move |window, cx| {
        let done = frame_tick(&next, window);
        if done {
            cx.quit();
        } else {
            schedule_frame_tick(next.clone(), window);
        }
    });
}

fn start_frame_measure(
    rows: usize,
    row_hashes: Vec<u64>,
    scroll_handle: UniformListScrollHandle,
    window: &mut Window,
) {
    let now = Instant::now();
    let state = Rc::new(RefCell::new(Some(FrameMeasure {
        rows,
        row_hashes,
        scroll_handle,
        frames: 0,
        target: FRAME_MEASURE_FRAMES,
        start: now,
        last: now,
        per_frame: Vec::with_capacity(FRAME_MEASURE_FRAMES),
        checksum: FNV_OFFSET,
        last_top: 0,
    })));
    schedule_frame_tick(state, window);
    window.refresh();
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

fn open_spike_window(cx: &mut App, model: Model) -> gpui::WindowHandle<BloomSpike> {
    let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(MIN_WIDTH), px(MIN_HEIGHT))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(SharedString::from("Bloom GPUI spike")),
                    ..Default::default()
                }),
                app_id: Some("dev.bloom.gpui-spike".to_string()),
                ..Default::default()
            },
            move |_, cx| {
                let composer = cx.new(|cx| Composer::new(cx, Theme::Dark));
                cx.new(|_| BloomSpike {
                    model,
                    theme: Theme::Dark,
                    composer,
                    scroll_handle: UniformListScrollHandle::new(),
                })
            },
        )
        .expect("failed to open GPUI window");
    // Wayland ignores TitlebarOptions.title, so set the toplevel title explicitly.
    window
        .update(cx, |_view, window, _cx| {
            window.set_window_title("Bloom GPUI spike");
            window.set_app_id("dev.bloom.gpui-spike");
        })
        .expect("failed to set spike window title");
    window
}

fn run_window() {
    let model = Model::synthetic(MESSAGE_COUNT);
    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        let window = open_spike_window(cx, model);
        window
            .update(cx, |view, window, cx| {
                let focus_handle = view.composer.read(cx).focus_handle.clone();
                window.focus(&focus_handle);
            })
            .expect("failed to focus spike window");
        cx.activate(true);
    });
}

fn run_frame_measure(rows: usize) {
    std::thread::spawn(|| {
        std::thread::sleep(FRAME_MEASURE_TIMEOUT);
        eprintln!("frame-measure timed out after 180s");
        std::process::exit(1);
    });

    let model = Model::synthetic(rows);
    let row_hashes: Vec<u64> = model
        .messages
        .iter()
        .map(|message| {
            let mut hash = FNV_OFFSET;
            mix(&mut hash, message.index as u64);
            mix_str(&mut hash, &message.body);
            hash
        })
        .collect();
    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        let window = open_spike_window(cx, model);
        let scroll_handle = window
            .update(cx, |view, window, cx| {
                let focus_handle = view.composer.read(cx).focus_handle.clone();
                window.focus(&focus_handle);
                view.scroll_handle.clone()
            })
            .expect("failed to read spike scroll handle");
        window
            .update(cx, |_view, window, _cx| {
                start_frame_measure(rows, row_hashes, scroll_handle, window);
            })
            .expect("failed to start frame measurement");
        cx.activate(true);
    });
}

fn main() {
    let launch = match parse_args(std::env::args().skip(1)) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };

    match launch {
        Launch::Window => run_window(),
        Launch::Measure { rows } => run_measure(rows),
        Launch::FrameMeasure { rows } => run_frame_measure(rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_window_by_default() {
        assert_eq!(parse_args(args(&[])), Ok(Launch::Window));
    }

    #[test]
    fn parses_measure_with_default_rows() {
        assert_eq!(
            parse_args(args(&["--measure"])),
            Ok(Launch::Measure {
                rows: DEFAULT_MEASURE_ROWS
            })
        );
    }

    #[test]
    fn parses_frame_measure_with_rows() {
        assert_eq!(
            parse_args(args(&["--frame-measure"])),
            Ok(Launch::FrameMeasure {
                rows: DEFAULT_MEASURE_ROWS
            })
        );
        assert_eq!(
            parse_args(args(&["--frame-measure", "--rows", "7"])),
            Ok(Launch::FrameMeasure { rows: 7 })
        );
        assert_eq!(
            parse_args(args(&["--rows=9", "--frame-measure"])),
            Ok(Launch::FrameMeasure { rows: 9 })
        );
    }

    #[test]
    fn parses_measure_rows() {
        assert_eq!(
            parse_args(args(&["--measure", "--rows", "7"])),
            Ok(Launch::Measure { rows: 7 })
        );
        assert_eq!(
            parse_args(args(&["--measure", "--rows=9"])),
            Ok(Launch::Measure { rows: 9 })
        );
        assert_eq!(
            parse_args(args(&["--rows=100000", "--measure"])),
            Ok(Launch::Measure { rows: 100_000 })
        );
    }

    #[test]
    fn rejects_bad_arguments_and_bounds() {
        assert!(parse_args(args(&["--rows=0", "--measure"])).is_err());
        assert!(parse_args(args(&["--rows=0", "--frame-measure"])).is_err());
        assert!(parse_args(args(&["--rows=100001", "--measure"])).is_err());
        assert!(parse_args(args(&["--rows=abc", "--measure"])).is_err());
        assert!(parse_args(args(&["--rows=5"])).is_err());
        assert!(parse_args(args(&["--rows"])).is_err());
        assert!(parse_args(args(&["--wat"])).is_err());
        assert!(parse_args(args(&["--measure", "--measure"])).is_err());
        assert!(parse_args(args(&["--frame-measure", "--frame-measure"])).is_err());
        assert!(parse_args(args(&["--measure", "--frame-measure"])).is_err());
        assert!(parse_args(args(&["--measure", "--rows=1", "--rows=2"])).is_err());
    }

    #[test]
    fn model_is_deterministic_and_checksum_stable() {
        let first = Model::synthetic(512);
        let second = Model::synthetic(512);
        assert_eq!(first.messages, second.messages);
        assert_eq!(first.checksum(), second.checksum());

        let mut changed = first.clone();
        changed.messages[7].body.push('!');
        assert_ne!(first.checksum(), changed.checksum());
    }

    #[test]
    fn model_contains_required_surfaces() {
        let model = Model::synthetic(500);
        assert_eq!(model.messages.len(), 500);
        assert!(model.messages.iter().any(|message| message.outgoing));
        assert!(model.messages.iter().any(|message| !message.outgoing));
        assert!(model.messages.iter().any(|message| message.edited));
        assert!(model.messages.iter().any(|message| message.unsent));
        assert!(model
            .messages
            .iter()
            .any(|message| !message.reactions.is_empty()));
        assert!(model
            .messages
            .iter()
            .any(|message| message.attachment.is_some()));
        assert!(model
            .messages
            .iter()
            .any(|message| message.body.contains('\n')));
        assert!(model
            .messages
            .iter()
            .any(|message| !message.body.is_ascii()));
    }

    #[test]
    fn viewport_slices_are_bounded() {
        assert_eq!(viewport_slice(0, 0, 40), 0..0);
        assert_eq!(viewport_slice(10, 0, 0), 0..0);
        assert_eq!(viewport_slice(10, 0, 5), 0..5);
        assert_eq!(viewport_slice(10, 3, 5), 3..8);
        assert_eq!(viewport_slice(10, 8, 5), 5..10);
        assert_eq!(viewport_slice(10, 99, 5), 5..10);
        assert_eq!(viewport_slice(10, 0, 99), 0..10);
    }

    #[test]
    fn slice_checksum_is_deterministic_and_sensitive() {
        let model = Model::synthetic(2_000);
        let first = slice_checksum(&model.messages, 100, VIEWPORT_ROWS);
        let second = slice_checksum(&model.messages, 100, VIEWPORT_ROWS);
        assert_eq!(first, second);
        assert_ne!(first, slice_checksum(&model.messages, 99, VIEWPORT_ROWS));

        let mut changed = model;
        changed.messages[0].body.push('?');
        assert_ne!(first, slice_checksum(&changed.messages, 100, VIEWPORT_ROWS));
    }

    #[test]
    fn editor_inserts_unicode_and_moves_by_grapheme() {
        let mut state = EditorState::default();
        state.replace_text(None, "aé☕e\u{301}");
        assert_eq!(state.content, "aé☕e\u{301}");
        assert_eq!(state.cursor_offset(), state.content.len());
        state.left();
        assert_eq!(state.cursor_offset(), "aé☕".len());
        state.left();
        assert_eq!(state.cursor_offset(), "aé".len());
        state.right();
        assert_eq!(state.cursor_offset(), "aé☕".len());
        state.home();
        assert_eq!(state.cursor_offset(), 0);
        state.end();
        assert_eq!(state.cursor_offset(), state.content.len());
    }

    #[test]
    fn editor_replaces_selection_and_deletes_graphemes() {
        let mut state = EditorState::default();
        state.replace_text(None, "hello 東京");
        state.select_all();
        assert_eq!(state.selected_range, 0..state.content.len());
        state.replace_text(None, "draft");
        assert_eq!(state.content, "draft");
        assert_eq!(state.selected_range, 5..5);

        state.replace_text(None, " ☕x");
        assert_eq!(state.content, "draft ☕x");
        state.backspace();
        assert_eq!(state.content, "draft ☕");

        state.home();
        state.delete();
        assert_eq!(state.content, "raft ☕");

        state.end();
        state.select_to(0);
        assert_eq!(state.selected_range, 0..state.content.len());
        assert!(state.selection_reversed);
        state.replace_text(None, "final");
        assert_eq!(state.content, "final");
        assert!(!state.selection_reversed);
    }

    #[test]
    fn editor_marked_text_replaces_and_unmarks() {
        let mut state = EditorState::default();
        state.replace_text(None, "ab");
        state.replace_and_mark(None, "にほ", Some(0..2));
        assert_eq!(state.content, "abにほ");
        assert_eq!(state.marked_range, Some("ab".len().."abにほ".len()));
        assert_eq!(state.selected_range, "ab".len().."abにほ".len());

        state.replace_and_mark(None, "ん", Some(0..1));
        assert_eq!(state.content, "abん");
        assert_eq!(state.marked_range, Some("ab".len().."abん".len()));

        state.unmark();
        assert_eq!(state.marked_range, None);
    }

    #[test]
    fn editor_utf16_conversion_handles_non_bmp() {
        let mut state = EditorState::default();
        state.replace_text(None, "a🚀é");
        assert_eq!(state.offset_to_utf16("a".len()), 1);
        assert_eq!(state.offset_to_utf16("a🚀".len()), 3);
        assert_eq!(state.range_from_utf16(&(1..3)), "a".len().."a🚀".len());
        assert_eq!(state.range_to_utf16(&("a".len().."a🚀".len())), 1..3);
    }

    #[test]
    fn theme_toggles_and_palettes_differ() {
        assert_eq!(Theme::Dark.toggled(), Theme::Light);
        assert_eq!(Theme::Light.toggled(), Theme::Dark);
        let dark = palette(Theme::Dark);
        let light = palette(Theme::Light);
        assert_ne!(dark.window_bg, light.window_bg);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.bubble_incoming, light.bubble_incoming);
    }

    #[test]
    fn responsive_layout_uses_window_width_boundaries() {
        assert_eq!(LayoutMode::for_width(1200.0), LayoutMode::Wide);
        assert_eq!(LayoutMode::for_width(1100.0), LayoutMode::Wide);
        assert_eq!(LayoutMode::for_width(1099.9), LayoutMode::Compact);
        assert_eq!(LayoutMode::for_width(800.0), LayoutMode::Compact);
        assert_eq!(LayoutMode::for_width(799.9), LayoutMode::Narrow);
        assert_eq!(LayoutMode::for_width(480.0), LayoutMode::Narrow);
        assert!(LayoutMode::Wide.shows_details());
        assert!(!LayoutMode::Compact.shows_details());
        assert!(!LayoutMode::Narrow.shows_details());
        assert_eq!(LayoutMode::Wide.sidebar_width(), 220.0);
        assert_eq!(LayoutMode::Compact.sidebar_width(), 220.0);
        assert_eq!(LayoutMode::Narrow.sidebar_width(), 160.0);
    }

    #[test]
    fn frame_percentile_is_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 50.0), 50.0);
        assert_eq!(percentile(&values, 95.0), 95.0);
        assert_eq!(percentile(&values, 100.0), 100.0);
        assert_eq!(percentile(&[], 95.0), 0.0);
    }

    #[test]
    fn frame_row_advances_from_first_to_last_row() {
        assert_eq!(row_for_tick(10_000, 0, 120), 0);
        assert_eq!(row_for_tick(10_000, 1, 120), 84);
        assert_eq!(row_for_tick(10_000, 119, 120), 9_999);
        assert_eq!(row_for_tick(1, 0, 120), 0);
        assert_eq!(row_for_tick(1, 119, 120), 0);
        assert_eq!(row_for_tick(10, 5, 1), 0);

        let mut previous = 0;
        for frame in 0..=120 {
            let row = row_for_tick(10_000, frame, 120);
            assert!(row >= previous, "row went backwards at frame {frame}");
            assert!(row < 10_000);
            previous = row;
        }
    }
}
