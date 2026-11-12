//! Demo-mode isolation contract.
//!
//! These tests drive the real `bloomd --demo` binary against a hand-built
//! session root, so they exercise the actual startup path: root validation, the
//! fresh-root guard, the demo store key, the demo socket, the IPC loop, and
//! cleanup. They also check that demo startup never consults the real
//! environment, the keychain, or the engine.

use bloom_ipc::{
    client::{BloomClient, ClientError},
    DraftKey, DraftWriteOutcome, DurableDraft, ProtocolError,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

const TOP_LEVEL: &[&str] = &["runtime", "data", "config", "state", "cache", "home"];

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Build the launcher-owned demo layout inside a fresh private root.
fn demo_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    private_dir(root.path());
    for name in TOP_LEVEL {
        private_dir(&root.path().join(name));
    }
    std::fs::write(
        root.path().join("demo-session.json"),
        b"{\"synthetic\":true}",
    )
    .unwrap();
    root
}

fn socket_path(root: &Path) -> PathBuf {
    root.join("runtime/bloomd.sock")
}

fn store_path(root: &Path) -> PathBuf {
    root.join("data/store.db")
}

/// A spawned demo daemon that is always reaped, so a failing assertion cannot
/// leave a process behind.
struct DemoDaemon {
    child: Child,
    log: PathBuf,
}

impl DemoDaemon {
    fn spawn(root: &Path, env: &[(&str, &str)], log_dir: &Path) -> Self {
        let log = log_dir.join("bloomd-demo.log");
        let stderr = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bloomd"));
        command
            .arg("--demo")
            .env_clear()
            .env("BLOOM_DEMO_ROOT", root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command.spawn().unwrap();
        Self { child, log }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn wait_for_socket(&mut self, path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "demo daemon exited before serving ({status}): {}",
                    self.log()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("demo socket did not appear: {}", self.log());
    }

    async fn wait_for_exit(&mut self) -> ExitStatus {
        for _ in 0..400 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("demo daemon did not exit: {}", self.log());
    }

    fn terminate(&mut self) {
        // SAFETY: `kill` with SIGTERM has no preconditions on a live child pid.
        let result = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        assert_eq!(result, 0, "SIGTERM delivery failed");
    }
}

impl Drop for DemoDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the demo daemon to completion and return its status and stderr.
fn run_to_exit(root: Option<&Path>, env: &[(&str, &str)]) -> (ExitStatus, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bloomd"));
    command
        .arg("--demo")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(root) = root {
        command.env("BLOOM_DEMO_ROOT", root);
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn demo_startup_opens_only_root_internal_paths() {
    let root = demo_root();
    let logs = tempfile::tempdir().unwrap();
    let mut daemon = DemoDaemon::spawn(root.path(), &[], logs.path());
    daemon.wait_for_socket(&socket_path(root.path())).await;

    // The store and socket live inside the root, nowhere else.
    assert!(store_path(root.path()).is_file(), "demo store missing");
    assert!(
        socket_path(root.path()).exists(),
        "demo socket missing inside root"
    );
    assert!(
        !daemon.log().contains("demo not implemented yet"),
        "the U1 stub must be gone: {}",
        daemon.log()
    );

    let client = BloomClient::new(socket_path(root.path()));
    assert!(client.list_conversations().await.unwrap().is_empty());
    assert!(client
        .search_messages("anything", 10)
        .await
        .unwrap()
        .is_empty());
    assert!(client.readiness().await.unwrap().can_send);

    // Drafts are store-backed and work in demo.
    let key = DraftKey::NewChat {
        transport: bloom_domain::TransportId::new("openbubbles").unwrap(),
        address: "demo@example.invalid".into(),
    };
    let draft = DurableDraft {
        body: "parked demo draft".into(),
        cursor: 4,
        pasted_body: false,
        uncertain: false,
    };
    assert_eq!(
        client.draft_save(key.clone(), draft, 1).await.unwrap(),
        DraftWriteOutcome::Applied
    );
    assert_eq!(
        client.draft_load(key).await.unwrap().draft.unwrap().body,
        "parked demo draft"
    );

    // Engine-facing operations are disabled in demo.
    let conversation = bloom_domain::ConversationId::new(
        bloom_domain::TransportId::new("openbubbles").unwrap(),
        "demo-chat",
    )
    .unwrap();
    let body = bloom_domain::OutgoingBody {
        parts: vec![bloom_domain::MessagePart::Text("hello".into())],
    };
    assert_eq!(
        client.send(conversation, body).await.unwrap_err(),
        ClientError::Server(ProtocolError::Unsupported)
    );
    assert_eq!(
        client
            .fetch_attachment("0123456789abcdef0123456789abcdef".into())
            .await
            .unwrap_err(),
        ClientError::Server(ProtocolError::Unsupported)
    );

    daemon.terminate();
    let status = daemon.wait_for_exit().await;
    assert!(status.success(), "demo shutdown failed: {}", daemon.log());
    // Cleanup removes only the session root.
    assert!(
        !root.path().exists(),
        "demo root must be removed after exit"
    );
}

#[tokio::test]
async fn demo_ignores_foreign_environment_and_sentinels() {
    let root = demo_root();
    let logs = tempfile::tempdir().unwrap();
    let sentinel = tempfile::tempdir().unwrap();
    let sentinel_socket = sentinel.path().join("real.sock");
    let sentinel_store = sentinel.path().join("real-store.db");
    let sentinel_home = sentinel.path().join("home");
    let sentinel_xdg = sentinel.path().join("xdg");

    let env = [
        ("BLOOM_SOCKET_PATH", sentinel_socket.to_str().unwrap()),
        ("BLOOM_SOCKET", sentinel_socket.to_str().unwrap()),
        ("BLOOM_STORE_PATH", sentinel_store.to_str().unwrap()),
        ("BLOOM_ENGINE_STATE", sentinel.path().to_str().unwrap()),
        ("BLOOM_ENGINE_LIBRARY", sentinel.path().to_str().unwrap()),
        ("BLOOM_ENGINE_SCRIPT", sentinel.path().to_str().unwrap()),
        ("BLOOM_DART_EXECUTABLE", sentinel.path().to_str().unwrap()),
        ("HOME", sentinel_home.to_str().unwrap()),
        ("XDG_DATA_HOME", sentinel_xdg.to_str().unwrap()),
        ("XDG_CONFIG_HOME", sentinel_xdg.to_str().unwrap()),
        ("XDG_STATE_HOME", sentinel_xdg.to_str().unwrap()),
        ("XDG_CACHE_HOME", sentinel_xdg.to_str().unwrap()),
        // A poisoned keyring environment: demo must never need one.
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus"),
        ("XDG_RUNTIME_DIR", "/nonexistent/runtime"),
    ];

    let mut daemon = DemoDaemon::spawn(root.path(), &env, logs.path());
    daemon.wait_for_socket(&socket_path(root.path())).await;

    assert!(store_path(root.path()).is_file());
    assert!(!sentinel_socket.exists(), "demo bound the real socket path");
    assert!(!sentinel_store.exists(), "demo wrote the real store path");
    assert!(!sentinel_home.exists(), "demo touched the inherited HOME");
    assert!(!sentinel_xdg.exists(), "demo touched an inherited XDG path");
    assert_eq!(
        std::fs::read_dir(sentinel.path()).unwrap().count(),
        0,
        "demo startup created entries in a sentinel directory"
    );

    let client = BloomClient::new(socket_path(root.path()));
    assert!(client.list_conversations().await.unwrap().is_empty());

    daemon.terminate();
    assert!(daemon.wait_for_exit().await.success());
    assert!(!root.path().exists());
}

#[test]
fn missing_malformed_or_unexpected_root_fails_without_fallback() {
    // Missing context: no BLOOM_DEMO_ROOT at all.
    let sentinel = tempfile::tempdir().unwrap();
    let sentinel_socket = sentinel.path().join("real.sock");
    let (status, stderr) = run_to_exit(
        None,
        &[("BLOOM_SOCKET_PATH", sentinel_socket.to_str().unwrap())],
    );
    assert!(!status.success(), "missing demo context must fail");
    assert!(
        stderr.contains("BLOOM_DEMO_ROOT"),
        "expected a typed demo context error, got: {stderr}"
    );
    assert!(
        !sentinel_socket.exists(),
        "demo fell back to normal startup"
    );

    // Malformed context: a relative root.
    let (status, stderr) = run_to_exit(Some(Path::new("relative/demo-root")), &[]);
    assert!(!status.success());
    assert!(
        stderr.contains("absolute"),
        "expected an absolute-path refusal, got: {stderr}"
    );

    // Unexpected top-level entry.
    let root = demo_root();
    std::fs::write(root.path().join("notes.txt"), b"not part of the layout").unwrap();
    let (status, stderr) = run_to_exit(Some(root.path()), &[]);
    assert!(!status.success());
    assert!(
        stderr.contains("unexpected top-level entry"),
        "expected a layout refusal, got: {stderr}"
    );
    assert!(
        root.path().exists(),
        "a refused root must not be removed or repaired"
    );
}

#[test]
fn stale_root_is_refused_and_never_reused() {
    let root = demo_root();
    // Simulate a crashed session: the daemon's own artifacts are still present.
    let stale_store = store_path(root.path());
    std::fs::write(&stale_store, b"crashed session database").unwrap();

    let (status, stderr) = run_to_exit(Some(root.path()), &[]);
    assert!(!status.success(), "a stale root must be refused");
    assert!(
        stderr.contains("not fresh"),
        "expected a fresh-root refusal, got: {stderr}"
    );
    assert!(
        root.path().exists(),
        "a stale root must be left for manual sweeping"
    );
    assert_eq!(
        std::fs::read(&stale_store).unwrap(),
        b"crashed session database",
        "the stale database must not be repaired or replaced"
    );
}

#[test]
fn normal_mode_never_consults_the_demo_root() {
    let root = demo_root();
    // Normal startup still requires real configuration, so it fails on the
    // engine state rather than entering demo mode.
    let mut command = Command::new(env!("CARGO_BIN_EXE_bloomd"));
    command
        .env_clear()
        .env("BLOOM_DEMO_ROOT", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("BLOOM_ENGINE_STATE"),
        "normal mode must still read real configuration, got: {stderr}"
    );
    assert!(
        !stderr.contains("demo"),
        "normal mode must not consult demo context, got: {stderr}"
    );
    assert!(
        root.path().exists(),
        "normal mode must not touch the demo root"
    );
    assert!(!socket_path(root.path()).exists());
}

#[test]
fn demo_source_has_no_keychain_or_engine_startup() {
    // Comments describe what is absent, so only code lines are inspected.
    fn code(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let demo = code(include_str!("../src/demo/mod.rs"));
    let service = code(include_str!("../src/service.rs"));
    let store = code(include_str!("../src/store/mod.rs"));

    for forbidden in [
        "OsKeyring",
        "open_or_create",
        "bloom_secrets",
        "Supervisor",
        "Engine::new",
        "DaemonConfig",
        "flatpak",
    ] {
        assert!(
            !demo.contains(forbidden),
            "demo startup must not reference {forbidden}"
        );
    }
    for forbidden in ["OsKeyring", "open_or_create"] {
        assert!(
            !service.contains(forbidden),
            "the service must not reference {forbidden}"
        );
    }
    assert!(demo.contains("open_demo"));
    assert!(store.contains("open_with_key"));
    assert!(store.contains("DEMO_STORE_KEY"));
    assert_eq!(bloomd::store::DEMO_STORE_KEY.len(), 32);
    assert_eq!(
        bloomd::store::DEMO_STORE_KEY,
        *b"bloom-demo-public-key-not-secret"
    );
}
