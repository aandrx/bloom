//! Standalone GPUI spike for a three-pane Bloom messenger surface.
//!
//! This binary is intentionally synthetic: no daemon, IPC, account or engine
//! code is involved. Run with no arguments for the window, or `--measure` for
//! the headless transcript benchmark.

use std::ops::Range;
use std::time::Instant;

use gpui::{
    div, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, Render,
    SharedString, Window, WindowBounds, WindowOptions,
};

const WINDOW_WIDTH: f32 = 1200.0;
const WINDOW_HEIGHT: f32 = 780.0;
const MIN_WIDTH: f32 = 820.0;
const MIN_HEIGHT: f32 = 520.0;
const CONVERSATIONS_WIDTH: f32 = 220.0;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Launch {
    Window,
    Measure { rows: usize },
}

fn parse_args<I>(args: I) -> Result<Launch, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut measure = false;
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

    if rows.is_some() && !measure {
        return Err("--rows requires --measure".into());
    }

    Ok(if measure {
        Launch::Measure {
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

struct BloomSpike {
    model: Model,
}

impl Render for BloomSpike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message_count = self.model.messages.len();

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x111318))
            .text_color(rgb(0xdfe3ea))
            .child(conversations_pane(&self.model.conversations))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(rgb(0x14171d))
                    .child(chat_header())
                    .child(
                        div().flex_1().min_h_0().overflow_hidden().child(
                            uniform_list(
                                "synthetic-transcript",
                                message_count,
                                cx.processor(|this, range: Range<usize>, _window, _cx| {
                                    range
                                        .map(|index| {
                                            message_row(&this.model.messages[index], index)
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .h_full(),
                        ),
                    )
                    .child(composer()),
            )
            .child(details_pane())
    }
}

fn conversations_pane(conversations: &[Conversation]) -> impl IntoElement {
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
                    rgb(0x252b36)
                } else {
                    rgb(0x171a21)
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
                                .text_color(rgb(0x8992a3))
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
                        .text_color(rgb(0x9aa3b2))
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
                                .bg(rgb(0x6d8dff))
                                .text_color(rgb(0xffffff))
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
        .w(px(CONVERSATIONS_WIDTH))
        .h_full()
        .bg(rgb(0x171a21))
        .border_r_1()
        .border_color(rgb(0x272c36))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .py_4()
                .border_b_1()
                .border_color(rgb(0x272c36))
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
                        .bg(rgb(0x101319))
                        .text_xs()
                        .text_color(rgb(0x7d8797))
                        .child("Search conversations"),
                ),
        )
        .child(list)
        .child(
            div()
                .px_4()
                .py_3()
                .text_xs()
                .text_color(rgb(0x737d8c))
                .child("Synthetic offline fixtures"),
        )
}

fn chat_header() -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(0x272c36))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Synthetic Studio"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8992a3))
                        .child("10,000-message fixture · offline"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(0x1d222b))
                        .text_xs()
                        .text_color(rgb(0x9aa3b2))
                        .child("Details"),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(0x1d222b))
                        .text_xs()
                        .text_color(rgb(0x9aa3b2))
                        .child("Search"),
                ),
        )
}

fn message_row(message: &Message, index: usize) -> impl IntoElement {
    let mut meta = div()
        .flex()
        .flex_row()
        .gap_2()
        .items_center()
        .text_xs()
        .text_color(rgb(0x8992a3))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if message.outgoing {
                    rgb(0x9fb2ff)
                } else {
                    rgb(0xdfe3ea)
                })
                .child(message.sender.clone()),
        )
        .child(div().child(message.time.clone()));

    if message.edited {
        meta = meta.child(div().text_color(rgb(0x7d8797)).child("edited"));
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
        rgb(0x2b3a63)
    } else {
        rgb(0x1d222b)
    };
    let bubble_border = if message.outgoing {
        rgb(0x3a4d7d)
    } else {
        rgb(0x2b313d)
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
                .bg(rgb(0x101319))
                .text_xs()
                .text_color(rgb(0xb3bdcd))
                .child(div().text_color(rgb(0x8da2ff)).child("▣"))
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
                    .bg(rgb(0x202631))
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

fn composer() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(rgb(0x272c36))
        .bg(rgb(0x14171d))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(rgb(0x1d222b))
                .border_1()
                .border_color(rgb(0x2b313d))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(0x7d8797))
                        .child("Message #synthetic-studio (read-only spike composer)"),
                )
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(0x303a4f))
                        .text_xs()
                        .text_color(rgb(0x8f9cb4))
                        .child("Send"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .text_xs()
                .text_color(rgb(0x6f7887))
                .child("Static composer · no IPC, sending, or interaction")
                .child("Dark surface only"),
        )
}

fn details_pane() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(DETAILS_WIDTH))
        .h_full()
        .bg(rgb(0x171a21))
        .border_l_1()
        .border_color(rgb(0x272c36))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .px_4()
                .py_5()
                .border_b_1()
                .border_color(rgb(0x272c36))
                .child(
                    div()
                        .size(px(56.))
                        .rounded_full()
                        .bg(rgb(0x2b3a63))
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
                        .text_color(rgb(0x8992a3))
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
                .text_color(rgb(0x9aa3b2))
                .child(section("About", "Offline GPUI rendering spike."))
                .child(section(
                    "Shared media",
                    "Attachment placeholders throughout.\nNo files are read or opened.",
                ))
                .child(section(
                    "Pinned",
                    "Transcript rows are uniform height and virtualized.",
                ))
                .child(section(
                    "Limits",
                    "No composer input, no sending, no daemon.",
                )),
        )
}

fn section(title: &'static str, body: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0xdfe3ea))
                .child(title),
        )
        .child(div().line_height(px(18.)).child(body))
}

fn run_window() {
    let model = Model::synthetic(MESSAGE_COUNT);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        cx.open_window(
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
                let model = model.clone();
                cx.new(|_| BloomSpike { model })
            },
        )
        .expect("failed to open GPUI window");
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
        assert!(parse_args(args(&["--rows=100001", "--measure"])).is_err());
        assert!(parse_args(args(&["--rows=abc", "--measure"])).is_err());
        assert!(parse_args(args(&["--rows=5"])).is_err());
        assert!(parse_args(args(&["--rows"])).is_err());
        assert!(parse_args(args(&["--wat"])).is_err());
        assert!(parse_args(args(&["--measure", "--measure"])).is_err());
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
}
