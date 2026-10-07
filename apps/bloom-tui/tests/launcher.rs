//! Integration tests for the installed `bloom` launcher.
//!
//! Stub `bloomd`/`bloom-tui` scripts stand in for the real binaries so the
//! process lifecycle (start, wait, reuse, stop) is exercised without an engine.
#![cfg(unix)]

use bloom_tui::launcher::run_app;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A daemon stub that binds the requested socket and then sleeps until killed.
fn daemon_stub(directory: &Path, pid_file: &Path) -> PathBuf {
    let path = directory.join("bloomd");
    write_script(
        &path,
        &format!(
            "#!/usr/bin/env python3\n\
             import os, socket, time\n\
             open({pid:?}, 'w').write(str(os.getpid()))\n\
             target = os.environ['BLOOM_SOCKET_PATH']\n\
             try:\n    os.unlink(target)\nexcept FileNotFoundError:\n    pass\n\
             server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n\
             server.bind(target)\n\
             server.listen(5)\n\
             time.sleep(60)\n",
            pid = pid_file.to_string_lossy(),
        ),
    );
    path
}

/// A TUI stub that records the socket it was handed and exits successfully.
fn tui_stub(directory: &Path, probe: &Path) -> PathBuf {
    let path = directory.join("bloom-tui");
    write_script(
        &path,
        &format!(
            "#!/usr/bin/env python3\n\
             import os\n\
             open({probe:?}, 'w').write(os.environ.get('BLOOM_SOCKET_PATH', ''))\n\
             raise SystemExit(0)\n",
            probe = probe.to_string_lossy(),
        ),
    );
    path
}

fn process_is_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn starts_a_daemon_runs_the_tui_and_stops_its_own_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("bloomd.sock");
    let log = directory.path().join("private/bloomd.log");
    let pid_file = directory.path().join("daemon.pid");
    let probe = directory.path().join("tui.probe");
    let daemon = daemon_stub(directory.path(), &pid_file);
    let tui = tui_stub(directory.path(), &probe);

    let code = run_app(Some(&daemon), &tui, &socket, &log);
    assert_eq!(code, ExitCode::SUCCESS);

    assert_eq!(
        fs::read_to_string(&probe).unwrap(),
        socket.to_string_lossy(),
        "the TUI must receive the shared socket path"
    );
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !process_is_alive(pid),
        "the launcher must stop the daemon it started"
    );
}

#[test]
fn reuses_an_already_listening_daemon_instead_of_starting_one() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("bloomd.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let probe = directory.path().join("tui.probe");
    let tui = tui_stub(directory.path(), &probe);

    // A daemon is already answering, so the (nonexistent) bloomd is never used.
    let code = run_app(
        Some(Path::new("/nonexistent/bloomd")),
        &tui,
        &socket,
        &directory.path().join("bloomd.log"),
    );
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        probe.exists(),
        "the TUI still runs against the existing daemon"
    );

    // The pre-existing listener is untouched: dropping it must succeed cleanly.
    drop(listener);
}

#[test]
fn a_daemon_that_exits_during_startup_is_reported_without_running_the_tui() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("bloomd.sock");
    let probe = directory.path().join("tui.probe");
    let daemon = directory.path().join("bloomd");
    write_script(&daemon, "#!/bin/sh\nexit 7\n");
    let tui = tui_stub(directory.path(), &probe);

    let code = run_app(
        Some(&daemon),
        &tui,
        &socket,
        &directory.path().join("bloomd.log"),
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(
        !probe.exists(),
        "the TUI must not open when the daemon never came up"
    );
}
