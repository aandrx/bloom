//! The installed `bloom` entry point.
//!
//! `cargo install` places the `bloom`, `bloomd`, and `bloom-tui` binaries side by
//! side, so the launcher can discover companions without a checkout or shell script:
//! it starts the daemon when nothing is listening on the socket, runs the TUI, and
//! stops only the daemon it started. A daemon that was already running is reused
//! and left alone, so several TUIs can share one owner of the engine state.

use crate::config::{Config, StoredReadPosition};
use bloom_ipc::demo::{DemoPaths, DemoRootError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// How long a freshly spawned daemon gets to start listening.
pub const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a daemon we started gets to stop before it is killed.
/// Covers daemon forwarding drain (10s), persistence (10s), and cleanup margin.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const TUI_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(100);
/// Flatpak application id of the engine provider, as used by the daemon.
const FLATPAK_APP: &str = "app.openbubbles.OpenBubbles";
/// Sidecar entry point inside the repository, used for development builds.
const REPO_SCRIPT: &str = "../../ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart";
/// Pinned Dart SDK, relative to the checkout root.
const REPO_DART: &str = ".dart-sdk/bin/dart";

/// The socket path, resolved the same way the daemon and TUI resolve it.
pub fn socket_path() -> PathBuf {
    bloom_ipc::socket_path_from_env()
}

/// True only when a process is listening: a stale socket file from a crashed
/// daemon must not be mistaken for a live one.
pub fn daemon_answers(socket: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let Ok(metadata) = fs::symlink_metadata(socket) else {
        return false;
    };
    if !metadata.file_type().is_socket() {
        return false;
    }
    nonblocking_connect(socket)
}

/// Locate a companion binary next to this executable first, then on `PATH`.
///
/// `cargo install` puts every binary of a package in the same directory, so the
/// sibling lookup avoids depending on the current working directory. The daemon
/// still needs the separately configured Dart sidecar and external engine.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().unwrap_or_default();
    let path_env = std::env::var_os("PATH");
    find_companion(&executable, path_env.as_deref(), name).map(|found| found.path)
}

/// Testable companion lookup: the sibling of `executable` first, then `PATH`.
pub fn find_companion(executable: &Path, path_env: Option<&OsStr>, name: &str) -> Option<Resolved> {
    let file_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Some(directory) = executable.parent() {
        let sibling = directory.join(&file_name);
        if is_executable(&sibling) {
            return Some(Resolved {
                path: sibling,
                source: "next to the launcher",
            });
        }
    }
    let path_env = path_env.filter(|value| !value.is_empty())?;
    std::env::split_paths(path_env)
        .map(|directory| directory.join(&file_name))
        .find(|candidate| is_executable(candidate))
        .map(|path| Resolved {
            path,
            source: "PATH",
        })
}

/// Where the daemon's stdout/stderr go when `bloom` starts it.
pub fn daemon_log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("bloom").join("bloomd.log")
}

/// Run the app: ensure a daemon, run the TUI, and clean up only our own daemon.
pub fn run() -> ExitCode {
    install_termination_handlers();
    let socket = socket_path();
    let bloomd = find_binary("bloomd");
    let Some(tui) = find_binary("bloom-tui") else {
        eprintln!("bloom: could not find bloom-tui next to bloom or on PATH");
        return ExitCode::from(1);
    };
    let mut started_daemon = match ensure_daemon(bloomd.as_deref(), &socket, &daemon_log_path()) {
        Ok(child) => child,
        Err(message) => {
            eprintln!("bloom: {message}");
            return ExitCode::from(if termination_signal() != 0 {
                128 + termination_signal() as u8
            } else {
                1
            });
        }
    };
    if termination_signal() != 0 {
        if let Some(child) = started_daemon.as_mut() {
            stop_daemon(child);
        }
        return ExitCode::from(128 + termination_signal() as u8);
    }
    run_tui(&tui, &socket, started_daemon.as_mut())
}

/// The testable core: everything is passed in, so a fixture can supply stub
/// binaries and a temporary socket. It never installs process signal handlers.
pub fn run_app(bloomd: Option<&Path>, tui: &Path, socket: &Path, log: &Path) -> ExitCode {
    let mut started_daemon = match ensure_daemon(bloomd, socket, log) {
        Ok(child) => child,
        Err(message) => {
            eprintln!("bloom: {message}");
            return ExitCode::from(1);
        }
    };
    run_tui(tui, socket, started_daemon.as_mut())
}

/// Version of the synthetic-only session marker written by the launcher.
pub const DEMO_SESSION_VERSION: u32 = 1;
/// Marker kind, so a stray JSON file cannot pass as a demo session.
pub const DEMO_SESSION_KIND: &str = "bloom-demo-session";
/// Restrained header label shown while a demo session is open.
pub const DEMO_INDICATOR: &str = "Demo";
/// The fixed relative socket path inside a demo root.
const DEMO_SOCKET_RELATIVE: &str = "runtime/bloomd.sock";
/// The fixed relative store path inside a demo root.
const DEMO_STORE_RELATIVE: &str = "data/store.db";
/// Top-level directories the launcher creates inside a fresh demo root.
const DEMO_LAYOUT: [&str; 6] = [
    "runtime",
    "data",
    "config/bloom",
    "state/bloom",
    "cache/attachments",
    "home",
];

/// The synthetic-only identity marker stored at `demo-session.json`.
///
/// The launcher writes it when it creates the root. Seeded read positions are
/// handed to the TUI through this marker: the demo daemon may replace it with
/// the seeded positions before the TUI starts, and the launcher copies them into
/// the demo TUI config. Nothing here is read in normal mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemoSessionMarker {
    pub version: u32,
    pub kind: String,
    pub synthetic: bool,
    pub root: String,
    pub socket: String,
    pub store: String,
    pub created_at: i64,
    #[serde(default)]
    pub read_positions: HashMap<String, StoredReadPosition>,
}

/// Why a demo session was refused before the TUI started.
#[derive(Debug)]
pub enum DemoContextError {
    /// `BLOOM_DEMO_ROOT` is missing or the root failed structural validation.
    Root(DemoRootError),
    /// The session marker is absent, so this is not a launcher-created session.
    MissingMarker,
    /// The marker is unreadable or not valid JSON.
    Marker(String),
    /// The marker does not describe this root.
    MarkerMismatch(&'static str),
    /// A filesystem operation on the root failed.
    Io(String),
}

impl std::fmt::Display for DemoContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root(error) => write!(f, "{error}"),
            Self::MissingMarker => write!(f, "demo session marker is missing"),
            Self::Marker(reason) => write!(f, "demo session marker is invalid: {reason}"),
            Self::MarkerMismatch(reason) => {
                write!(f, "demo session marker does not match the root: {reason}")
            }
            Self::Io(error) => write!(f, "demo session filesystem error: {error}"),
        }
    }
}

impl std::error::Error for DemoContextError {}

/// A validated demo session: fixed paths plus the seeded read positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoContext {
    pub paths: DemoPaths,
    pub read_positions: HashMap<String, StoredReadPosition>,
}

impl DemoContext {
    /// Resolve the demo session named by `BLOOM_DEMO_ROOT`.
    ///
    /// Normal mode never calls this, so an inherited `BLOOM_DEMO_ROOT` is inert
    /// unless the operator asked for `--demo`.
    pub fn from_env() -> Result<Self, DemoContextError> {
        let paths = DemoPaths::from_env().map_err(DemoContextError::Root)?;
        Self::from_paths(paths)
    }

    /// Validate an explicit root and read its session marker.
    pub fn from_root(root: &Path) -> Result<Self, DemoContextError> {
        let paths = DemoPaths::validate(root).map_err(DemoContextError::Root)?;
        Self::from_paths(paths)
    }

    fn from_paths(paths: DemoPaths) -> Result<Self, DemoContextError> {
        let marker = read_marker(&paths)?;
        validate_marker(&paths, &marker)?;
        Ok(Self {
            paths,
            read_positions: marker.read_positions,
        })
    }

    /// The demo socket. The normal socket is never probed or reused.
    pub fn socket_path(&self) -> &Path {
        &self.paths.socket_path
    }

    /// The demo preferences file, written by the launcher before the TUI starts.
    pub fn config_path(&self) -> &Path {
        &self.paths.tui_config_path
    }

    /// The demo state directory, used so diagnostics never touch the real one.
    pub fn state_dir(&self) -> PathBuf {
        self.paths.root.join("state")
    }
}

fn read_marker(paths: &DemoPaths) -> Result<DemoSessionMarker, DemoContextError> {
    let bytes = match fs::read(&paths.session_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DemoContextError::MissingMarker)
        }
        Err(error) => return Err(DemoContextError::Io(error.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|error| DemoContextError::Marker(error.to_string()))
}

fn validate_marker(paths: &DemoPaths, marker: &DemoSessionMarker) -> Result<(), DemoContextError> {
    if marker.version != DEMO_SESSION_VERSION {
        return Err(DemoContextError::MarkerMismatch("unsupported version"));
    }
    if marker.kind != DEMO_SESSION_KIND {
        return Err(DemoContextError::MarkerMismatch("unexpected kind"));
    }
    if !marker.synthetic {
        return Err(DemoContextError::MarkerMismatch("not marked synthetic"));
    }
    if marker.socket != DEMO_SOCKET_RELATIVE {
        return Err(DemoContextError::MarkerMismatch("unexpected socket path"));
    }
    if marker.store != DEMO_STORE_RELATIVE {
        return Err(DemoContextError::MarkerMismatch("unexpected store path"));
    }
    let actual_root = fs::canonicalize(&paths.root).map_err(DemoContextError::io)?;
    // A marker naming a nonexistent root still fails the comparison rather than
    // becoming a different error, so every mismatch reports the same reason.
    let marker_root =
        fs::canonicalize(&marker.root).unwrap_or_else(|_| PathBuf::from(&marker.root));
    if marker_root != actual_root {
        return Err(DemoContextError::MarkerMismatch("unexpected root"));
    }
    Ok(())
}

impl DemoContextError {
    fn io(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// A launcher-owned demo session root, created fresh for one launch.
#[derive(Debug)]
pub struct DemoSession {
    context: DemoContext,
}

impl DemoSession {
    /// Create a fresh private root under `/tmp` with the exact demo layout and
    /// the synthetic-only marker. An existing path is never reused.
    pub fn create() -> Result<Self, String> {
        let root = create_demo_root()?;
        for relative in DEMO_LAYOUT {
            create_private_tree(&root, relative)?;
        }
        let paths = DemoPaths::validate(&root).map_err(|error| error.to_string())?;
        let marker = DemoSessionMarker {
            version: DEMO_SESSION_VERSION,
            kind: DEMO_SESSION_KIND.to_owned(),
            synthetic: true,
            root: paths.root.to_string_lossy().into_owned(),
            socket: DEMO_SOCKET_RELATIVE.to_owned(),
            store: DEMO_STORE_RELATIVE.to_owned(),
            created_at: now_millis(),
            read_positions: HashMap::new(),
        };
        write_marker(&paths, &marker)?;
        Ok(Self {
            context: DemoContext {
                paths,
                read_positions: HashMap::new(),
            },
        })
    }

    pub fn context(&self) -> &DemoContext {
        &self.context
    }

    /// Re-read the marker after the daemon has seeded the session so seeded read
    /// positions reach the TUI. A malformed marker fails startup.
    pub fn refresh_read_positions(&mut self) -> Result<(), String> {
        let context =
            DemoContext::from_root(&self.context.paths.root).map_err(|error| error.to_string())?;
        self.context.read_positions = context.read_positions;
        Ok(())
    }

    /// Write the demo TUI preferences before the TUI starts. This is the only
    /// config the demo TUI loads; the real preferences file is never touched.
    pub fn write_tui_config(&self) -> Result<(), String> {
        write_demo_tui_config(&self.context)
    }

    /// Remove only the session root. The daemon may already have removed it, and
    /// a missing root is not an error.
    pub fn cleanup(&self) {
        match fs::remove_dir_all(&self.context.paths.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("bloom: demo cleanup failed: {error}"),
        }
    }
}

/// Write the demo TUI preferences for a validated context. The seeded read
/// positions land in `config/bloom/tui.json`; no real preferences file is read
/// or written.
pub fn write_demo_tui_config(context: &DemoContext) -> Result<(), String> {
    let config = Config {
        version: 1,
        read_positions: context.read_positions.clone(),
        ..Config::default()
    };
    config
        .save(&context.paths.tui_config_path)
        .map_err(|error| format!("could not write demo preferences: {error}"))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Create one fresh, private root. A collision is retried with a new name; an
/// existing root is never adopted.
fn create_demo_root() -> Result<PathBuf, String> {
    let base = Path::new("/tmp");
    for _ in 0..8 {
        let root = base.join(format!("bloom-demo-{}", random_hex()?));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => return Ok(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("could not create {}: {error}", root.display())),
            }
        }
        #[cfg(not(unix))]
        {
            match fs::create_dir(&root) {
                Ok(()) => return Ok(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("could not create {}: {error}", root.display())),
            }
        }
    }
    Err("could not allocate a fresh demo session root".into())
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("could not read randomness: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn create_private_tree(root: &Path, relative: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("{} is not a directory", current.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&current)
                        .map_err(|error| {
                            format!("could not create {}: {error}", current.display())
                        })?;
                }
                #[cfg(not(unix))]
                {
                    fs::create_dir(&current).map_err(|error| {
                        format!("could not create {}: {error}", current.display())
                    })?;
                }
            }
            Err(error) => return Err(format!("could not inspect {}: {error}", current.display())),
        }
    }
    Ok(())
}

fn write_marker(paths: &DemoPaths, marker: &DemoSessionMarker) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| format!("could not encode demo session marker: {error}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&paths.session_file)
        .map_err(|error| format!("could not write demo session marker: {error}"))?;
    use std::io::Write;
    file.write_all(&bytes)
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| format!("could not write demo session marker: {error}"))
}

/// Run an isolated demo session: fresh root, demo daemon, demo TUI, cleanup.
pub fn run_demo() -> ExitCode {
    install_termination_handlers();
    let Some(tui) = find_binary("bloom-tui") else {
        eprintln!("bloom: could not find bloom-tui next to bloom or on PATH");
        return ExitCode::from(1);
    };
    let Some(bloomd) = find_binary("bloomd") else {
        eprintln!("bloom: could not find bloomd next to bloom or on PATH");
        return ExitCode::from(1);
    };
    run_demo_app(&bloomd, &tui)
}

/// The testable demo core: it never installs process signal handlers and never
/// probes the normal socket.
pub fn run_demo_app(bloomd: &Path, tui: &Path) -> ExitCode {
    let mut session = match DemoSession::create() {
        Ok(session) => session,
        Err(message) => {
            eprintln!("bloom: {message}");
            return ExitCode::from(1);
        }
    };
    let paths = session.context().paths.clone();
    let mut daemon = match start_demo_daemon(bloomd, &paths) {
        Ok(child) => child,
        Err(message) => {
            eprintln!("bloom: {message}");
            session.cleanup();
            return ExitCode::from(1);
        }
    };
    if termination_signal() != 0 {
        stop_daemon(&mut daemon);
        session.cleanup();
        return ExitCode::from(128 + termination_signal() as u8);
    }
    // The daemon seeds before it listens, so the marker is complete now. Copy the
    // seeded read positions into the demo preferences before the TUI reads them.
    if let Err(message) = session.refresh_read_positions() {
        eprintln!("bloom: {message}");
        stop_daemon(&mut daemon);
        session.cleanup();
        return ExitCode::from(1);
    }
    if let Err(message) = session.write_tui_config() {
        eprintln!("bloom: {message}");
        stop_daemon(&mut daemon);
        session.cleanup();
        return ExitCode::from(1);
    }
    run_demo_tui(tui, &paths, &mut daemon, &session)
}

fn run_demo_tui(
    tui: &Path,
    paths: &DemoPaths,
    daemon: &mut Child,
    session: &DemoSession,
) -> ExitCode {
    let original_termios = capture_terminal();
    let mut tui_child = match Command::new(tui)
        .arg("--demo")
        .env(bloom_ipc::demo::DEMO_ROOT_ENV, &paths.root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            stop_daemon(daemon);
            session.cleanup();
            eprintln!("bloom: could not run the demo TUI: {error}");
            return ExitCode::from(1);
        }
    };
    let status = loop {
        if termination_signal() != 0 {
            terminate_tui(&mut tui_child);
            restore_terminal(original_termios);
            stop_daemon(daemon);
            session.cleanup();
            return ExitCode::from(128 + termination_signal() as u8);
        }
        match tui_child.try_wait() {
            Ok(Some(status)) => {
                restore_terminal(original_termios);
                break Ok(status);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                stop_child(&mut tui_child, TUI_STOP_TIMEOUT);
                restore_terminal(original_termios);
                break Err(error);
            }
        }
    };
    stop_daemon(daemon);
    session.cleanup();
    match status {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("bloom: could not run the demo TUI: {error}");
            ExitCode::from(1)
        }
    }
}

/// Reuse a listening daemon, otherwise start one and wait for its socket.
fn ensure_daemon(
    bloomd: Option<&Path>,
    socket: &Path,
    log: &Path,
) -> Result<Option<Child>, String> {
    if daemon_answers(socket) {
        return Ok(None);
    }
    let Some(bloomd) = bloomd else {
        return Err("no daemon is listening and bloomd was not found".into());
    };
    start_daemon(bloomd, socket, log).map(Some)
}

/// Run the TUI to completion, then stop the daemon only if we started it.
fn run_tui(tui: &Path, socket: &Path, daemon: Option<&mut Child>) -> ExitCode {
    let original_termios = capture_terminal();
    let mut tui_child = match Command::new(tui)
        .env("BLOOM_SOCKET_PATH", socket)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            if let Some(child) = daemon {
                stop_daemon(child);
            }
            eprintln!("bloom: could not run the TUI: {error}");
            return ExitCode::from(1);
        }
    };
    let status = loop {
        if termination_signal() != 0 {
            terminate_tui(&mut tui_child);
            restore_terminal(original_termios);
            if let Some(child) = daemon {
                stop_daemon(child);
            }
            return ExitCode::from(128 + termination_signal() as u8);
        }
        match tui_child.try_wait() {
            Ok(Some(status)) => {
                restore_terminal(original_termios);
                break Ok(status);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                stop_child(&mut tui_child, TUI_STOP_TIMEOUT);
                restore_terminal(original_termios);
                break Err(error);
            }
        }
    };
    if let Some(child) = daemon {
        stop_daemon(child);
    }
    match status {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("bloom: could not run the TUI: {error}");
            ExitCode::from(1)
        }
    }
}

/// Open a private daemon log, refusing a symlink or a file we do not own.
fn open_private_log(log: &Path) -> Result<(fs::File, fs::File), String> {
    secure_log_parent(log)?;
    use std::os::unix::fs::OpenOptionsExt;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(log)
        .map_err(|error| format!("could not open {}: {error}", log.display()))?;
    use std::os::unix::fs::MetadataExt;
    let metadata = stdout
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", log.display()))?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(format!("refusing unsafe daemon log {}", log.display()));
    }
    use std::os::unix::fs::PermissionsExt;
    stdout
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure {}: {error}", log.display()))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("could not open {}: {error}", log.display()))?;
    Ok((stdout, stderr))
}

/// Start the daemon with its diagnostics in the log, then wait for the socket.
pub fn start_daemon(bloomd: &Path, socket: &Path, log: &Path) -> Result<Child, String> {
    let (stdout, stderr) = open_private_log(log)?;
    let mut command = Command::new(bloomd);
    command.env("BLOOM_SOCKET_PATH", socket);
    spawn_daemon(command, stdout, stderr, socket, log)
}

/// Start the demo daemon against the session root, then wait for the demo socket.
/// The normal socket is never consulted, and only `BLOOM_DEMO_ROOT` and `--demo`
/// are added to the inherited environment.
fn start_demo_daemon(bloomd: &Path, paths: &DemoPaths) -> Result<Child, String> {
    let (stdout, stderr) = open_private_log(&paths.daemon_log_path)?;
    let mut command = Command::new(bloomd);
    command
        .arg("--demo")
        .env(bloom_ipc::demo::DEMO_ROOT_ENV, &paths.root);
    spawn_daemon(
        command,
        stdout,
        stderr,
        &paths.socket_path,
        &paths.daemon_log_path,
    )
}

fn spawn_daemon(
    mut command: Command,
    stdout: fs::File,
    stderr: fs::File,
    socket: &Path,
    log: &Path,
) -> Result<Child, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| format!("could not start {program}: {error}"))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if termination_signal() != 0 {
            stop_daemon(&mut child);
            return Err("startup interrupted by termination signal".into());
        }
        if daemon_answers(socket) {
            return Ok(child);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "the daemon exited during startup ({status}); see {}",
                    log.display()
                ));
            }
            Ok(None) => {}
            Err(error) => {
                stop_daemon(&mut child);
                return Err(format!(
                    "could not check the daemon: {error}; see {}",
                    log.display()
                ));
            }
        }
        if Instant::now() >= deadline {
            stop_daemon(&mut child);
            return Err(format!(
                "the daemon never started listening; see {}",
                log.display()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Ask a daemon we started to stop, then kill it if it does not.
pub fn stop_daemon(child: &mut Child) {
    stop_child(child, STOP_TIMEOUT)
}

fn stop_child(child: &mut Child, timeout: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

static TERMINATION_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn record_termination(signal: libc::c_int) {
    TERMINATION_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

fn termination_signal() -> i32 {
    TERMINATION_SIGNAL.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(unix)]
fn nonblocking_connect(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path_c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return false;
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path_c.as_bytes().len() >= address.sun_path.len() {
        unsafe {
            libc::close(fd);
        }
        return false;
    }
    for (dst, src) in address.sun_path.iter_mut().zip(path_c.as_bytes_with_nul()) {
        *dst = *src as libc::c_char;
    }
    let length = (std::mem::size_of_val(&address.sun_family) + path_c.as_bytes_with_nul().len())
        as libc::socklen_t;
    let result =
        unsafe { libc::connect(fd, &address as *const _ as *const libc::sockaddr, length) };
    unsafe {
        libc::close(fd);
    }
    result == 0
}

/// A path with a plain-text description of how it was resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: &'static str,
}

impl Resolved {
    fn detail(&self) -> String {
        format!("{} ({})", self.path.display(), self.source)
    }
}

/// One line of `bloom --doctor` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub required: bool,
    pub ok: bool,
    pub detail: String,
}

impl DoctorCheck {
    fn pass(name: impl Into<String>, required: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: impl Into<String>, required: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required,
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Everything the doctor needs, passed explicitly so tests need no real install.
#[derive(Clone, Debug)]
pub struct DoctorInput {
    pub executable: PathBuf,
    pub path_env: Option<OsString>,
    pub engine_script: Option<OsString>,
    pub engine_library: Option<OsString>,
    pub engine_state: Option<OsString>,
    pub dart: Option<OsString>,
    pub socket: PathBuf,
    pub socket_source: bloom_ipc::SocketSource,
    pub store_env: Option<OsString>,
    pub data_home: Option<OsString>,
    pub home: Option<PathBuf>,
    pub checkout: Option<PathBuf>,
    pub repo_script: PathBuf,
    pub flatpak_library: Option<PathBuf>,
}

/// Sidecar entry point, resolved by the same rule as the daemon: environment,
/// installed prefix, legacy sibling, then the checkout. The prefix derives from
/// `bloomd`'s path so the doctor agrees with the daemon even if the launcher
/// lives elsewhere.
pub fn resolve_sidecar(
    bloomd_exe: &Path,
    script_env: Option<&OsStr>,
    repo_script: &Path,
) -> Option<Resolved> {
    // The daemon resolves its own executable through /proc/self/exe, so a
    // symlinked install must be canonicalised here or the two prefixes differ.
    let resolved_exe =
        std::fs::canonicalize(bloomd_exe).unwrap_or_else(|_| bloomd_exe.to_path_buf());
    bloom_ipc::sidecar_script_with_source(&resolved_exe, script_env, repo_script).map(
        |(path, source)| Resolved {
            path,
            source: source.label(),
        },
    )
}

/// Engine library: the environment override, then the installed Flatpak payload.
pub fn resolve_library(
    library_env: Option<&OsStr>,
    flatpak_library: Option<&Path>,
) -> Option<Resolved> {
    if let Some(value) = library_env.filter(|value| !value.is_empty()) {
        return Some(Resolved {
            path: PathBuf::from(value),
            source: "BLOOM_ENGINE_LIBRARY",
        });
    }
    flatpak_library
        .filter(|path| path.is_file())
        .map(|path| Resolved {
            path: path.to_path_buf(),
            source: "installed Flatpak",
        })
}

/// Engine state directory: the environment override, then the Flatpak data dir.
pub fn resolve_state(state_env: Option<&OsStr>, home: Option<&Path>) -> Option<Resolved> {
    if let Some(value) = state_env.filter(|value| !value.is_empty()) {
        return Some(Resolved {
            path: PathBuf::from(value),
            source: "BLOOM_ENGINE_STATE",
        });
    }
    let candidate = home?
        .join(".var/app")
        .join(FLATPAK_APP)
        .join("data/bluebubbles");
    candidate.is_dir().then_some(Resolved {
        path: candidate,
        source: "default Flatpak state",
    })
}

/// Dart executable: the environment override, `PATH`, the checkout's pinned SDK,
/// then a known user-local SDK.
pub fn resolve_dart(
    dart_env: Option<&OsStr>,
    path_env: Option<&OsStr>,
    home: Option<&Path>,
    checkout: Option<&Path>,
) -> Option<Resolved> {
    if let Some(value) = dart_env.filter(|value| !value.is_empty()) {
        return Some(Resolved {
            path: PathBuf::from(value),
            source: "BLOOM_DART_EXECUTABLE",
        });
    }
    if let Some(path_env) = path_env.filter(|value| !value.is_empty()) {
        if let Some(path) = std::env::split_paths(path_env)
            .map(|directory| directory.join("dart"))
            .find(|candidate| candidate.is_file())
        {
            return Some(Resolved {
                path,
                source: "PATH",
            });
        }
    }
    if let Some(checkout) = checkout {
        let candidate = checkout.join(REPO_DART);
        if candidate.is_file() {
            return Some(Resolved {
                path: candidate,
                source: "checkout Dart SDK",
            });
        }
    }
    let home = home?;
    for relative in [
        ".cache/dart-sdk/dart-sdk/bin/dart",
        ".local/share/bloom/dart/bin/dart",
    ] {
        let candidate = home.join(relative);
        if candidate.is_file() {
            return Some(Resolved {
                path: candidate,
                source: "local Dart SDK",
            });
        }
    }
    None
}

fn socket_source_label(source: bloom_ipc::SocketSource) -> &'static str {
    match source {
        bloom_ipc::SocketSource::ExplicitPath => "BLOOM_SOCKET_PATH",
        bloom_ipc::SocketSource::LegacyAlias => "BLOOM_SOCKET (legacy alias)",
        bloom_ipc::SocketSource::Default => "default runtime path",
    }
}

/// Store path: the environment override, then `XDG_DATA_HOME` or `HOME`.
pub fn resolve_store(
    store_env: Option<&OsStr>,
    data_home: Option<&OsStr>,
    home: Option<&Path>,
) -> Resolved {
    if let Some(value) = store_env.filter(|value| !value.is_empty()) {
        return Resolved {
            path: PathBuf::from(value),
            source: "BLOOM_STORE_PATH",
        };
    }
    if let Some(value) = data_home.filter(|value| !value.is_empty()) {
        return Resolved {
            path: PathBuf::from(value).join("bloom/store.db"),
            source: "XDG_DATA_HOME",
        };
    }
    if let Some(home) = home {
        return Resolved {
            path: home.join(".local/share/bloom/store.db"),
            source: "HOME",
        };
    }
    Resolved {
        path: PathBuf::from("bloom/store.db"),
        source: "fallback",
    }
}

fn is_readable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) && fs::File::open(path).is_ok()
}

/// Resolve the engine library the same way the daemon does, read-only.
fn flatpak_engine_library() -> Option<PathBuf> {
    let output = Command::new("flatpak")
        .args(["info", "--show-location", FLATPAK_APP])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let candidate =
        PathBuf::from(root.trim()).join("files/bluebubbles/lib/librust_lib_bluebubbles.so");
    candidate.is_file().then_some(candidate)
}

impl DoctorInput {
    /// Read the process environment; only paths and executables are inspected.
    pub fn from_process() -> Self {
        let (socket, socket_source) = bloom_ipc::socket_path_from_env_with_source();
        Self {
            executable: std::env::current_exe().unwrap_or_default(),
            path_env: std::env::var_os("PATH"),
            engine_script: std::env::var_os("BLOOM_ENGINE_SCRIPT"),
            engine_library: std::env::var_os("BLOOM_ENGINE_LIBRARY"),
            engine_state: std::env::var_os("BLOOM_ENGINE_STATE"),
            dart: std::env::var_os("BLOOM_DART_EXECUTABLE"),
            socket,
            socket_source,
            store_env: std::env::var_os("BLOOM_STORE_PATH"),
            data_home: std::env::var_os("XDG_DATA_HOME"),
            home: std::env::var_os("HOME").map(PathBuf::from),
            checkout: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")),
            repo_script: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(REPO_SCRIPT),
            flatpak_library: flatpak_engine_library(),
        }
    }
}

/// Build the doctor checks. Nothing here starts or executes the engine, sidecar,
/// or daemon; the only process call is a read-only `flatpak info` lookup.
pub fn doctor_checks(input: &DoctorInput) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    let mut bloomd = None;
    for name in ["bloom-tui", "bloomd"] {
        let label = format!("companion {name}");
        match find_companion(&input.executable, input.path_env.as_deref(), name) {
            Some(found) => {
                if name == "bloomd" {
                    bloomd = Some(found.path.clone());
                }
                checks.push(DoctorCheck::pass(label, true, found.detail()));
            }
            None => checks.push(DoctorCheck::fail(
                label,
                true,
                "not found next to the launcher or on PATH",
            )),
        }
    }

    // The daemon resolves its prefix from its own executable, so the sidecar
    // check uses the discovered `bloomd` path rather than the launcher's.
    let bloomd = bloomd.unwrap_or_else(|| input.executable.clone());
    let sidecar = resolve_sidecar(&bloomd, input.engine_script.as_deref(), &input.repo_script);
    checks.push(match sidecar {
        Some(found) if is_readable_file(&found.path) => {
            DoctorCheck::pass("sidecar script", true, found.detail())
        }
        Some(found) => DoctorCheck::fail(
            "sidecar script",
            true,
            format!("{} ({}): not a readable file", found.path.display(), found.source),
        ),
        None => DoctorCheck::fail(
            "sidecar script",
            true,
            "not found via BLOOM_ENGINE_SCRIPT, the installed prefix, next to the daemon, or in the repository checkout",
        ),
    });

    let library = resolve_library(
        input.engine_library.as_deref(),
        input.flatpak_library.as_deref(),
    );
    checks.push(match library {
        Some(found) if is_readable_file(&found.path) => {
            DoctorCheck::pass("engine library", true, found.detail())
        }
        Some(found) => DoctorCheck::fail(
            "engine library",
            true,
            format!(
                "{} ({}): not a readable file",
                found.path.display(),
                found.source
            ),
        ),
        None => DoctorCheck::fail(
            "engine library",
            true,
            "not found via BLOOM_ENGINE_LIBRARY or the installed Flatpak",
        ),
    });

    let state = resolve_state(input.engine_state.as_deref(), input.home.as_deref());
    checks.push(match state {
        Some(found) if found.path.is_dir() => {
            DoctorCheck::pass("engine state", true, found.detail())
        }
        Some(found) => DoctorCheck::fail(
            "engine state",
            true,
            format!(
                "{} ({}): not a directory",
                found.path.display(),
                found.source
            ),
        ),
        None => DoctorCheck::fail(
            "engine state",
            true,
            "not found via BLOOM_ENGINE_STATE or the default Flatpak location",
        ),
    });

    let dart = resolve_dart(
        input.dart.as_deref(),
        input.path_env.as_deref(),
        input.home.as_deref(),
        input.checkout.as_deref(),
    );
    checks.push(match dart {
        Some(found) if is_executable(&found.path) => {
            DoctorCheck::pass("dart executable", true, found.detail())
        }
        Some(found) => DoctorCheck::fail(
            "dart executable",
            true,
            format!(
                "{} ({}): not an executable file",
                found.path.display(),
                found.source
            ),
        ),
        None => DoctorCheck::fail(
            "dart executable",
            true,
            "not found via BLOOM_DART_EXECUTABLE, PATH, or a local Dart SDK",
        ),
    });

    let socket = &input.socket;
    let listening = daemon_answers(socket);
    let daemon_detail = if listening {
        format!("a daemon is already listening at {}", socket.display())
    } else {
        format!(
            "not listening at {}; bloom would start bloomd",
            socket.display()
        )
    };
    checks.push(DoctorCheck::pass(
        "socket path",
        false,
        format!(
            "{} ({})",
            socket.display(),
            socket_source_label(input.socket_source)
        ),
    ));
    checks.push(DoctorCheck::pass("daemon", false, daemon_detail));

    let store = resolve_store(
        input.store_env.as_deref(),
        input.data_home.as_deref(),
        input.home.as_deref(),
    );
    checks.push(DoctorCheck::pass("store path", false, store.detail()));

    checks
}

/// Render one line per check. Only check names, paths, and sources are printed.
pub fn doctor_report(checks: &[DoctorCheck]) -> String {
    checks
        .iter()
        .map(|check| {
            format!(
                "{:<4} {}: {}\n",
                if check.ok { "ok" } else { "FAIL" },
                check.name,
                check.detail
            )
        })
        .collect()
}

/// Non-zero when any required check fails.
pub fn doctor_exit_code(checks: &[DoctorCheck]) -> u8 {
    u8::from(checks.iter().any(|check| check.required && !check.ok))
}

/// Run the diagnostics and print them, without starting the daemon or engine.
pub fn doctor() -> ExitCode {
    let checks = doctor_checks(&DoctorInput::from_process());
    print!("{}", doctor_report(&checks));
    ExitCode::from(doctor_exit_code(&checks))
}

fn secure_log_parent(log: &Path) -> Result<(), String> {
    let Some(parent) = log.parent() else {
        return Err("daemon log has no parent directory".into());
    };
    let mut missing = Vec::new();
    let mut current = parent;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current.parent().ok_or("invalid daemon log directory")?;
    }
    for dir in missing.into_iter().rev() {
        fs::create_dir(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("could not secure {}: {e}", dir.display()))?;
    }
    let metadata = fs::symlink_metadata(parent)
        .map_err(|e| format!("could not inspect {}: {e}", parent.display()))?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing unsafe daemon log directory {}",
            parent.display()
        ));
    }
    Ok(())
}

fn terminate_tui(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    stop_child(child, TUI_STOP_TIMEOUT);
}

fn install_termination_handlers() {
    #[cfg(unix)]
    unsafe {
        TERMINATION_SIGNAL.store(0, std::sync::atomic::Ordering::SeqCst);
        libc::signal(
            libc::SIGTERM,
            record_termination as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            record_termination as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            record_termination as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(unix)]
fn capture_terminal() -> Option<libc::termios> {
    let fd = std::io::stdin().as_raw_fd();
    let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
    (unsafe { libc::tcgetattr(fd, &mut term) } == 0).then_some(term)
}
#[cfg(not(unix))]
fn capture_terminal() -> Option<()> {
    None
}

#[cfg(unix)]
fn restore_terminal(original: Option<libc::termios>) {
    if let Some(term) = original {
        let fd = std::io::stdin().as_raw_fd();
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &term);
        }
        if unsafe { libc::isatty(std::io::stdout().as_raw_fd()) } == 1 {
            use std::io::Write;
            let _ = std::io::stdout().write_all(
                b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l\x1b[?25h",
            );
            let _ = std::io::stdout().flush();
        }
    }
}
#[cfg(not(unix))]
fn restore_terminal(_: Option<()>) {}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_socket_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!daemon_answers(&directory.path().join("absent.sock")));
    }

    #[test]
    fn a_listening_socket_answers() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("bloomd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(daemon_answers(&socket));
        drop(listener);
        assert!(!daemon_answers(&socket));
    }

    #[test]
    fn companion_binaries_are_found_next_to_this_executable() {
        // `bloom-tui` is a sibling of the test binary in the Cargo target dir.
        let found = find_binary("bloom-tui");
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("bloom-tui")));
        if let Some(sibling) = sibling.filter(|path| path.is_file()) {
            assert_eq!(found.as_deref(), Some(sibling.as_path()));
        }
    }

    #[test]
    fn the_daemon_log_lives_under_a_bloom_directory() {
        assert!(daemon_log_path().ends_with("bloom/bloomd.log"));
    }

    fn write_executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A synthetic prefix with every required piece present.
    fn complete_input(prefix: &Path) -> DoctorInput {
        for name in ["bloom", "bloom-tui", "bloomd"] {
            write_executable(&prefix.join(name));
        }
        let sidecar = prefix.join("bloom-engine/bloom_engine.dart");
        fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        fs::write(&sidecar, "// sidecar\n").unwrap();
        let library = prefix.join("librust_lib_bluebubbles.so");
        fs::write(&library, "engine library").unwrap();
        let state = prefix.join("state");
        fs::create_dir(&state).unwrap();
        let dart = prefix.join("dart");
        write_executable(&dart);
        DoctorInput {
            executable: prefix.join("bloom"),
            path_env: None,
            engine_script: None,
            engine_library: Some(library.into_os_string()),
            engine_state: Some(state.into_os_string()),
            dart: Some(dart.into_os_string()),
            socket: prefix.join("absent.sock"),
            socket_source: bloom_ipc::SocketSource::Default,
            store_env: None,
            data_home: None,
            home: Some(prefix.to_path_buf()),
            checkout: None,
            repo_script: prefix.join("repo/bloom_engine.dart"),
            flatpak_library: None,
        }
    }

    fn check_named<'a>(checks: &'a [DoctorCheck], name: &str) -> &'a DoctorCheck {
        checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("no check named {name}"))
    }

    #[test]
    fn socket_source_labels_match_each_source() {
        use bloom_ipc::SocketSource;
        assert_eq!(
            socket_source_label(SocketSource::ExplicitPath),
            "BLOOM_SOCKET_PATH"
        );
        assert_eq!(
            socket_source_label(SocketSource::LegacyAlias),
            "BLOOM_SOCKET (legacy alias)"
        );
        assert_eq!(
            socket_source_label(SocketSource::Default),
            "default runtime path"
        );
    }

    #[test]
    fn all_present_prefix_passes() {
        let directory = tempfile::tempdir().unwrap();
        let input = complete_input(directory.path());
        let checks = doctor_checks(&input);
        assert!(
            checks.iter().all(|check| check.ok),
            "unexpected failure: {}",
            doctor_report(&checks)
        );
        let daemon = check_named(&checks, "daemon");
        assert!(daemon.detail.contains("absent.sock"));
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    #[test]
    fn missing_companion_fails_with_a_named_check() {
        let directory = tempfile::tempdir().unwrap();
        let input = complete_input(directory.path());
        fs::remove_file(directory.path().join("bloomd")).unwrap();
        let checks = doctor_checks(&input);
        let companion = check_named(&checks, "companion bloomd");
        assert!(!companion.ok);
        assert!(companion.required);
        assert!(doctor_report(&checks).contains("FAIL companion bloomd"));
        assert_eq!(doctor_exit_code(&checks), 1);
    }

    #[test]
    fn doctor_prefix_sidecar_matches_the_shared_resolver_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = complete_input(directory.path());
        // Move the binaries into `bin/` so the install matches the canonical
        // prefix layout; the legacy sibling stays to prove prefix precedence.
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["bloom", "bloom-tui", "bloomd"] {
            fs::rename(directory.path().join(name), bin.join(name)).unwrap();
        }
        input.executable = bin.join("bloom");
        let prefix_sidecar = directory
            .path()
            .join("share/bloom/engine/bin/bloom_engine.dart");
        fs::create_dir_all(prefix_sidecar.parent().unwrap()).unwrap();
        fs::write(&prefix_sidecar, "// sidecar\n").unwrap();

        let checks = doctor_checks(&input);
        let sidecar = check_named(&checks, "sidecar script");
        let (expected, source) =
            bloom_ipc::sidecar_script_with_source(&bin.join("bloomd"), None, &input.repo_script)
                .unwrap();
        assert_eq!(expected, prefix_sidecar);
        assert!(sidecar.ok);
        assert!(
            sidecar.detail.contains(source.label()),
            "doctor detail {:?} must name {:?}",
            sidecar.detail,
            source.label()
        );
        assert_eq!(
            sidecar.detail,
            format!("{} ({})", expected.display(), source.label())
        );
    }

    #[test]
    fn unreadable_sidecar_fails() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = complete_input(directory.path());
        let unreadable = directory.path().join("not-a-file");
        fs::create_dir(&unreadable).unwrap();
        input.engine_script = Some(unreadable.into_os_string());
        let checks = doctor_checks(&input);
        let sidecar = check_named(&checks, "sidecar script");
        assert!(!sidecar.ok);
        assert!(sidecar.detail.contains("not a readable file"));
        assert_eq!(doctor_exit_code(&checks), 1);
    }

    #[test]
    fn missing_engine_state_or_library_fails() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = complete_input(directory.path());
        let absent = directory.path().join("absent");
        input.engine_state = Some(absent.clone().into_os_string());
        input.engine_library = Some(absent.into_os_string());
        let checks = doctor_checks(&input);
        assert!(!check_named(&checks, "engine state").ok);
        assert!(!check_named(&checks, "engine library").ok);
        assert_eq!(doctor_exit_code(&checks), 1);
    }

    #[test]
    fn missing_dart_is_reported_and_does_not_pass() {
        let directory = tempfile::tempdir().unwrap();
        let mut input = complete_input(directory.path());
        input.dart = None;
        input.path_env = Some(OsString::from(directory.path().join("empty-bin")));
        let checks = doctor_checks(&input);
        let dart = check_named(&checks, "dart executable");
        assert!(!dart.ok);
        assert!(doctor_report(&checks).contains("FAIL dart executable"));
        assert_eq!(doctor_exit_code(&checks), 1);
    }

    #[test]
    fn checkout_dart_sdk_wins_over_the_home_cache() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        let checkout_dart = checkout.join(REPO_DART);
        fs::create_dir_all(checkout_dart.parent().unwrap()).unwrap();
        write_executable(&checkout_dart);

        let home = directory.path().join("home");
        let cached = home.join(".cache/dart-sdk/dart-sdk/bin/dart");
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        write_executable(&cached);

        let resolved = resolve_dart(None, None, Some(&home), Some(&checkout)).unwrap();
        assert_eq!(resolved.path, checkout_dart);
        assert_eq!(resolved.source, "checkout Dart SDK");
    }

    #[test]
    fn home_cache_is_the_fallback_when_the_checkout_has_no_sdk() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        fs::create_dir(&checkout).unwrap();

        let home = directory.path().join("home");
        let cached = home.join(".cache/dart-sdk/dart-sdk/bin/dart");
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        write_executable(&cached);

        let resolved = resolve_dart(None, None, Some(&home), Some(&checkout)).unwrap();
        assert_eq!(resolved.path, cached);
        assert_eq!(resolved.source, "local Dart SDK");
    }

    #[test]
    fn exit_status_maps_required_checks_only() {
        let ok = DoctorCheck::pass("required", true, "present");
        let optional_fail = DoctorCheck::fail("optional", false, "absent");
        let required_fail = DoctorCheck::fail("required", true, "absent");
        assert_eq!(doctor_exit_code(std::slice::from_ref(&ok)), 0);
        assert_eq!(doctor_exit_code(&[ok, optional_fail]), 0);
        assert_eq!(doctor_exit_code(&[required_fail]), 1);
    }
}
