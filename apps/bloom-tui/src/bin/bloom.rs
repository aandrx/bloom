//! The installed `bloom` command: start the daemon if needed, then open the TUI.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    bloom_tui::launcher::run()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bloom: this build only supports Unix-socket daemons");
    std::process::ExitCode::from(1)
}
