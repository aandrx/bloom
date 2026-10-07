//! The installed `bloom` entry point.
//!
//! `cargo install` places the `bloom`, `bloomd`, and `bloom-tui` binaries side by
//! side, so the launcher can discover companions without a checkout or shell script:
//! it starts the daemon when nothing is listening on the socket, runs the TUI, and
//! stops only the daemon it started. A daemon that was already running is reused
//! and left alone, so several TUIs can share one owner of the engine state.

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

/// The socket path, resolved the same way the daemon and TUI resolve it.
pub fn socket_path() -> PathBuf {
    std::env::var_os("BLOOM_SOCKET_PATH")
        .or_else(|| std::env::var_os("BLOOM_SOCKET"))
        .map(PathBuf::from)
        .unwrap_or_else(bloom_ipc::default_socket_path)
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
    let file_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            let sibling = directory.join(&file_name);
            if is_executable(&sibling) {
                return Some(sibling);
            }
        }
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(&file_name))
        .find(|candidate| is_executable(candidate))
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
}
