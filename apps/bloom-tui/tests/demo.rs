//! Demo-mode wiring tests for the TUI.
//!
//! These cover the launcher-owned demo context, the read-position handoff into
//! the demo preferences, the restrained header indicator, and the seeded unread
//! and draft surface. They never touch the real socket, store, or preferences.

use bloom_domain::{
    Conversation, ConversationId, ConversationKind, DeliveryState, Handle, HandleKind, Message,
    MessageId, MessagePart, Participant, TransportId,
};
use bloom_ipc::{DraftKey, DurableDraft};
use bloom_tui::{
    app::{Action, App, Theme},
    config::{Config, StoredReadPosition},
    launcher::{
        write_demo_tui_config, DemoContext, DemoContextError, DemoSessionMarker, DEMO_INDICATOR,
        DEMO_SESSION_KIND, DEMO_SESSION_VERSION,
    },
    ui,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use std::{
    collections::HashMap, fs, os::unix::fs::PermissionsExt, path::Path, path::PathBuf, sync::Mutex,
};

/// `BLOOM_DEMO_ROOT` is process-global, so the tests that set it take this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const LAYOUT: [&str; 6] = [
    "runtime",
    "data",
    "config/bloom",
    "state/bloom",
    "cache/attachments",
    "home",
];

fn layout(root: &Path) {
    for relative in LAYOUT {
        fs::create_dir_all(root.join(relative)).unwrap();
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
}

fn marker(root: &Path, read_positions: HashMap<String, StoredReadPosition>) -> DemoSessionMarker {
    DemoSessionMarker {
        version: DEMO_SESSION_VERSION,
        kind: DEMO_SESSION_KIND.to_owned(),
        synthetic: true,
        root: fs::canonicalize(root)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        socket: "runtime/bloomd.sock".into(),
        store: "data/store.db".into(),
        created_at: 1,
        read_positions,
    }
}

fn write_marker(root: &Path, marker: &DemoSessionMarker) {
    fs::write(
        root.join("demo-session.json"),
        serde_json::to_vec(marker).unwrap(),
    )
    .unwrap();
}

fn demo_root(read_positions: HashMap<String, StoredReadPosition>) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    layout(directory.path());
    write_marker(directory.path(), &marker(directory.path(), read_positions));
    directory
}

fn stored(created_at: i64, transport: &str, value: &str) -> StoredReadPosition {
    StoredReadPosition {
        created_at,
        transport: transport.to_owned(),
        value: value.to_owned(),
    }
}

#[test]
fn a_valid_marker_resolves_only_demo_paths() {
    let directory = demo_root(HashMap::from([(
        "openbubbles|chat-1".to_owned(),
        stored(10, "openbubbles", "m1"),
    )]));
    let context = DemoContext::from_root(directory.path()).unwrap();
    assert_eq!(
        context.socket_path(),
        directory.path().join("runtime/bloomd.sock")
    );
    assert_eq!(
        context.config_path(),
        directory.path().join("config/bloom/tui.json")
    );
    assert_eq!(context.state_dir(), directory.path().join("state"));
    assert_eq!(context.read_positions.len(), 1);
}

#[test]
fn a_root_without_a_marker_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    layout(directory.path());
    assert!(matches!(
        DemoContext::from_root(directory.path()),
        Err(DemoContextError::MissingMarker)
    ));
}

#[test]
fn a_marker_that_does_not_describe_the_root_is_refused() {
    let directory = demo_root(HashMap::new());
    let mut wrong_kind = marker(directory.path(), HashMap::new());
    wrong_kind.kind = "some-other-session".into();
    write_marker(directory.path(), &wrong_kind);
    assert!(matches!(
        DemoContext::from_root(directory.path()),
        Err(DemoContextError::MarkerMismatch(_))
    ));

    let mut wrong_root = marker(directory.path(), HashMap::new());
    wrong_root.root = "/tmp/not-this-root".into();
    write_marker(directory.path(), &wrong_root);
    assert!(matches!(
        DemoContext::from_root(directory.path()),
        Err(DemoContextError::MarkerMismatch(_))
    ));

    fs::write(directory.path().join("demo-session.json"), b"not json").unwrap();
    assert!(matches!(
        DemoContext::from_root(directory.path()),
        Err(DemoContextError::Marker(_))
    ));
}

#[test]
fn direct_demo_without_a_session_never_resolves_a_normal_context() {
    let _guard = ENV_LOCK.lock().unwrap();
    let previous = std::env::var_os(bloom_ipc::demo::DEMO_ROOT_ENV);
    std::env::remove_var(bloom_ipc::demo::DEMO_ROOT_ENV);
    assert!(matches!(
        DemoContext::from_env(),
        Err(DemoContextError::Root(
            bloom_ipc::demo::DemoRootError::Unset
        ))
    ));

    // A hand-set root that is not a launcher-created session also fails.
    let directory = tempfile::tempdir().unwrap();
    layout(directory.path());
    std::env::set_var(bloom_ipc::demo::DEMO_ROOT_ENV, directory.path());
    assert!(matches!(
        DemoContext::from_env(),
        Err(DemoContextError::MissingMarker)
    ));

    match previous {
        Some(value) => std::env::set_var(bloom_ipc::demo::DEMO_ROOT_ENV, value),
        None => std::env::remove_var(bloom_ipc::demo::DEMO_ROOT_ENV),
    }
}

#[test]
fn seeded_read_positions_reach_the_demo_tui_config() {
    let directory = demo_root(HashMap::from([(
        "openbubbles|chat-1".to_owned(),
        stored(10, "openbubbles", "m1"),
    )]));
    let context = DemoContext::from_root(directory.path()).unwrap();
    write_demo_tui_config(&context).unwrap();

    // The real preferences path is untouched: only the demo config exists.
    let (config, warning) = Config::load(context.config_path());
    assert_eq!(warning, None);
    assert_eq!(
        config.read_positions.get("openbubbles|chat-1"),
        Some(&stored(10, "openbubbles", "m1"))
    );
}

fn transport() -> TransportId {
    TransportId::new("openbubbles").unwrap()
}

fn conversation() -> Conversation {
    Conversation {
        id: ConversationId::new(transport(), "chat-1").unwrap(),
        remote_id: "iMessage;-;chat-1".into(),
        kind: ConversationKind::Direct,
        title: Some("Demo chat".into()),
        participants: vec![
            Participant {
                handle: Handle::new(HandleKind::Opaque, "me").unwrap(),
                display_name: Some("Me".into()),
                is_me: true,
            },
            Participant {
                handle: Handle::new(HandleKind::Opaque, "maya").unwrap(),
                display_name: Some("Maya".into()),
                is_me: false,
            },
        ],
        created_at: 1,
        last_message_at: Some(20),
        aliases: Vec::new(),
        preview: None,
    }
}

fn message(conversation: &Conversation, id: &str, at: i64, text: &str, mine: bool) -> Message {
    Message::new(
        MessageId::new(transport(), id).unwrap(),
        conversation.id.clone(),
        None,
        if mine {
            conversation.participants[0].handle.clone()
        } else {
            conversation.participants[1].handle.clone()
        },
        vec![MessagePart::Text(text.to_owned())],
        at,
        None,
        if mine {
            DeliveryState::Delivered
        } else {
            DeliveryState::Read
        },
        mine,
        None,
        Vec::new(),
        None,
    )
    .unwrap()
}

fn cached_frame(app: &App, width: u16, height: u16) -> (ratatui::buffer::Buffer, ui::HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut cache = ui::RenderCache::default();
    let mut hits = ui::HitMap::default();
    terminal
        .draw(|frame| hits = ui::render_with_cache(frame, app, &mut cache))
        .unwrap();
    (terminal.backend().buffer().clone(), hits)
}

fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn outside_transcript_contains(
    buffer: &ratatui::buffer::Buffer,
    hits: &ui::HitMap,
    needle: &str,
) -> bool {
    (0..buffer.area.height)
        .filter(|y| *y < hits.transcript.y || *y >= hits.transcript.y + hits.transcript.height)
        .any(|y| row_text(buffer, y).contains(needle))
}

fn transcript_text(buffer: &ratatui::buffer::Buffer, hits: &ui::HitMap) -> String {
    let area = hits.transcript;
    (area.y..area.y + area.height)
        .map(|y| {
            (area.x..area.x + area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("")
}

fn demo_app(theme: Theme) -> App {
    let conversation = conversation();
    let mut app = App {
        theme,
        system_theme: theme,
        session: DEMO_INDICATOR.to_owned(),
        ..Default::default()
    };
    app.set_conversations(vec![conversation.clone()]);
    app.upsert(
        conversation.id.clone(),
        message(&conversation, "m1", 10, "synthetic history", false),
    );
    app.select(Some(conversation.id.clone()));
    app
}

#[test]
fn the_demo_indicator_renders_outside_the_transcript() {
    let app = demo_app(Theme::Dark);
    for (width, height) in [(100, 24), (66, 18)] {
        let (buffer, hits) = cached_frame(&app, width, height);
        assert!(
            outside_transcript_contains(&buffer, &hits, DEMO_INDICATOR),
            "the demo indicator must sit outside the transcript at {width}x{height}"
        );
        assert!(
            !transcript_text(&buffer, &hits).contains(DEMO_INDICATOR),
            "the demo indicator must not occupy the transcript"
        );
    }
}

#[test]
fn the_demo_indicator_renders_in_both_themes() {
    for theme in [Theme::Dark, Theme::Light] {
        let app = demo_app(theme);
        let (buffer, hits) = cached_frame(&app, 100, 24);
        assert!(outside_transcript_contains(&buffer, &hits, DEMO_INDICATOR));
    }
}

#[test]
fn seeded_read_positions_and_a_parked_draft_are_visible() {
    // The demo's auto-selected chat sits at its newest message, so it is read.
    let directory = demo_root(HashMap::from([(
        "openbubbles|chat-1".to_owned(),
        stored(20, "openbubbles", "m2"),
    )]));
    let context = DemoContext::from_root(directory.path()).unwrap();
    write_demo_tui_config(&context).unwrap();
    let (config, _) = Config::load(context.config_path());

    let conversation = conversation();
    let mut app = App::default();
    app.set_conversations(vec![conversation.clone()]);
    app.upsert(
        conversation.id.clone(),
        message(&conversation, "m1", 10, "already seen", false),
    );
    app.upsert(
        conversation.id.clone(),
        message(&conversation, "m2", 20, "new since the marker", false),
    );
    app.select(Some(conversation.id.clone()));
    app.apply_read_positions(&config.read_positions);
    assert_eq!(
        app.unread_count(&conversation.id).map(|count| count.shown),
        Some(0),
        "the auto-selected chat must not be unread"
    );

    let loaded = app.apply_loaded_draft(
        DraftKey::Conversation {
            owner: conversation.id.clone(),
        },
        DurableDraft {
            body: "Parked demo draft".into(),
            cursor: 17,
            pasted_body: false,
            uncertain: false,
        },
    );
    assert!(loaded, "the parked demo draft must load into the composer");

    let (buffer, hits) = cached_frame(&app, 100, 24);
    let rendered: String = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !rendered.contains("· 1"),
        "the auto-selected chat must not show an unread badge:\n{rendered}"
    );
    assert!(
        rendered.contains("Parked demo draft"),
        "the seeded draft must be visible:\n{rendered}"
    );
    assert!(hits.transcript.width > 0);
}

#[test]
fn every_seeded_history_loads_and_only_unread_chats_show_badges() {
    let transport = transport();
    let mut app = App::default();
    let mut conversations = Vec::new();
    let mut read_positions = HashMap::new();
    for index in 0..5 {
        let id = ConversationId::new(transport.clone(), format!("chat-{index}")).unwrap();
        let mut chat = conversation();
        chat.id = id.clone();
        chat.title = Some(format!("Demo chat {index}"));
        app.upsert(
            id.clone(),
            message(&chat, &format!("m{index}-1"), 10, "already seen", false),
        );
        app.upsert(
            id.clone(),
            message(
                &chat,
                &format!("m{index}-2"),
                20,
                "new since the marker",
                false,
            ),
        );
        // The first chat is the one the bootstrap snapshot auto-selects, so its
        // marker sits at its newest message. The other four keep an older one.
        let (created_at, value) = if index == 0 {
            (20, format!("m{index}-2"))
        } else {
            (10, format!("m{index}-1"))
        };
        read_positions.insert(
            bloom_tui::app::conversation_key(&id),
            stored(created_at, "openbubbles", &value),
        );
        conversations.push(chat);
    }
    let selected = conversations[0].id.clone();
    app.set_conversations(conversations.clone());
    app.select(Some(selected.clone()));
    app.apply_read_positions(&read_positions);

    for chat in &conversations {
        let count = app.unread_count(&chat.id).unwrap_or_else(|| {
            panic!(
                "history for {} must be loaded so its badge can be computed",
                chat.id.value()
            )
        });
        if chat.id == selected {
            assert_eq!(count.shown, 0, "the auto-selected chat must be read");
        } else {
            assert_eq!(count.shown, 1, "each other seeded chat must stay unread");
        }
    }

    let (buffer, _hits) = cached_frame(&app, 100, 24);
    let rendered: String = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("· 1"),
        "the unread chats must show badges:\n{rendered}"
    );
}

#[test]
fn demo_diagnostics_resolve_inside_the_session_root() {
    let directory = demo_root(HashMap::new());
    let context = DemoContext::from_root(directory.path()).unwrap();
    let state_dir = context.state_dir();
    let resolved = bloom_tui::diagnostics::resolve_path(Some(&state_dir)).unwrap();
    assert_eq!(resolved, directory.path().join("state/bloom/tui.log"));
    assert!(resolved.starts_with(directory.path()));
}

#[test]
fn diagnostics_keep_both_platform_bases_inside_the_given_root() {
    let root = Path::new("/tmp/bloom-demo-root");
    // The XDG base used on Linux, and the explicit demo state directory.
    assert_eq!(
        bloom_tui::diagnostics::path_in(&root.join("state")),
        root.join("state/bloom/tui.log")
    );
    // The macOS HOME base. Demo mode never reaches it, but if it did with HOME
    // inside the session root the log would still stay under that root.
    assert_eq!(
        bloom_tui::diagnostics::path_in(&root.join("home/Library/Logs")),
        root.join("home/Library/Logs/bloom/tui.log")
    );
}

#[test]
fn normal_mode_diagnostics_keep_the_platform_base() {
    let resolved = bloom_tui::diagnostics::resolve_path(None);
    #[cfg(target_os = "macos")]
    {
        let expected = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Logs/bloom/tui.log"));
        assert_eq!(resolved, expected);
    }
    #[cfg(target_os = "windows")]
    {
        let expected =
            std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("bloom/tui.log"));
        assert_eq!(resolved, expected);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let expected = std::env::var_os("XDG_STATE_HOME")
            .map(|base| PathBuf::from(base).join("bloom/tui.log"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local/state/bloom/tui.log"))
            });
        assert_eq!(resolved, expected);
    }
}

#[test]
fn exit_and_quit_commands_work_in_a_demo_session() {
    for command in ["/exit", "/quit"] {
        let mut app = App {
            session: DEMO_INDICATOR.to_owned(),
            ..Default::default()
        };
        for character in command.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Quit,
            "{command} must quit a demo session"
        );
    }
}
