//! The installed `bloom` entry point.
//!
//! `cargo install` places the `bloom`, `bloomd`, and `bloom-tui` binaries side by
//! side, so the launcher can discover companions without a checkout or shell script:
//! it starts the daemon when nothing is listening on the socket, runs the TUI, and
//! stops only the daemon it started. A daemon that was already running is reused
//! and left alone, so several TUIs can share one owner of the engine state.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
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

/// Start the daemon with its diagnostics in the log, then wait for the socket.
pub fn start_daemon(bloomd: &Path, socket: &Path, log: &Path) -> Result<Child, String> {
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

    let mut child = Command::new(bloomd)
        .env("BLOOM_SOCKET_PATH", socket)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| format!("could not start {}: {error}", bloomd.display()))?;

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
    pub repo_script: PathBuf,
    pub flatpak_library: Option<PathBuf>,
}

/// Sidecar entry point: environment, then beside the binary, then the checkout.
pub fn resolve_sidecar(
    executable: &Path,
    script_env: Option<&OsStr>,
    repo_script: &Path,
) -> Option<Resolved> {
    if let Some(value) = script_env.filter(|value| !value.is_empty()) {
        return Some(Resolved {
            path: PathBuf::from(value),
            source: "BLOOM_ENGINE_SCRIPT",
        });
    }
    // A missing executable parent must not skip the checkout fallback, or the
    // doctor reports a false failure that the daemon would not hit.
    if let Some(parent) = executable.parent() {
        let sibling = parent.join("bloom-engine/bloom_engine.dart");
        if sibling.is_file() {
            return Some(Resolved {
                path: sibling,
                source: "next to the launcher",
            });
        }
    }
    repo_script.is_file().then(|| Resolved {
        path: repo_script.to_path_buf(),
        source: "repository checkout",
    })
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

/// Dart executable: the environment override, `PATH`, then a known local SDK.
pub fn resolve_dart(
    dart_env: Option<&OsStr>,
    path_env: Option<&OsStr>,
    home: Option<&Path>,
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
            repo_script: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(REPO_SCRIPT),
            flatpak_library: flatpak_engine_library(),
        }
    }
}

/// Build the doctor checks. Nothing here starts or executes the engine, sidecar,
/// or daemon; the only process call is a read-only `flatpak info` lookup.
pub fn doctor_checks(input: &DoctorInput) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    for name in ["bloom-tui", "bloomd"] {
        let label = format!("companion {name}");
        match find_companion(&input.executable, input.path_env.as_deref(), name) {
            Some(found) => checks.push(DoctorCheck::pass(label, true, found.detail())),
            None => checks.push(DoctorCheck::fail(
                label,
                true,
                "not found next to the launcher or on PATH",
            )),
        }
    }

    let sidecar = resolve_sidecar(
        &input.executable,
        input.engine_script.as_deref(),
        &input.repo_script,
    );
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
            "not found via BLOOM_ENGINE_SCRIPT, next to the launcher, or in the repository checkout",
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
    fn exit_status_maps_required_checks_only() {
        let ok = DoctorCheck::pass("required", true, "present");
        let optional_fail = DoctorCheck::fail("optional", false, "absent");
        let required_fail = DoctorCheck::fail("required", true, "absent");
        assert_eq!(doctor_exit_code(std::slice::from_ref(&ok)), 0);
        assert_eq!(doctor_exit_code(&[ok, optional_fail]), 0);
        assert_eq!(doctor_exit_code(&[required_fail]), 1);
    }
}
