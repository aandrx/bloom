//! The installed `bloom` command: start the daemon if needed, then open the TUI.
//!
//! `bloom --doctor` runs read-only diagnostics instead and never starts anything.

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|argument| argument == "--doctor") {
        // Never fall through to a full client start when the operator asked for
        // diagnostics; reject extra arguments instead.
        if let Some(unexpected) = args.next() {
            eprintln!(
                "bloom: --doctor takes no other arguments (got {})",
                unexpected.to_string_lossy()
            );
            return std::process::ExitCode::from(2);
        }
        return bloom_tui::launcher::doctor();
    }
    bloom_tui::launcher::run()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bloom: this build only supports Unix-socket daemons");
    std::process::ExitCode::from(1)
}
