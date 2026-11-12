//! The installed `bloom` command: start the daemon if needed, then open the TUI.
//!
//! `bloom --doctor` runs read-only diagnostics instead and never starts anything.
//! `bloom --demo` runs a fully isolated synthetic session and never probes or
//! reuses the normal daemon socket.

#[cfg(unix)]
fn usage() -> &'static str {
    "usage: bloom [--demo | --doctor | --help]\n\
     \n\
     With no arguments, start the daemon if needed and open the TUI.\n\
     --demo    run an isolated synthetic session under a fresh private root\n\
     --doctor  print read-only diagnostics and exit\n\
     --help    print this help and exit\n"
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        return bloom_tui::launcher::run();
    };
    if let Some(unexpected) = args.next() {
        eprintln!(
            "bloom: {} takes no other arguments (got {})",
            first.to_string_lossy(),
            unexpected.to_string_lossy()
        );
        return std::process::ExitCode::from(2);
    }
    match first.to_str() {
        // Never fall through to a full client start when the operator asked for
        // diagnostics or help; reject extra arguments instead.
        Some("--doctor") => bloom_tui::launcher::doctor(),
        Some("--demo") => bloom_tui::launcher::run_demo(),
        Some("--help") | Some("-h") => {
            print!("{}", usage());
            std::process::ExitCode::SUCCESS
        }
        _ => {
            eprintln!("bloom: unknown argument {}", first.to_string_lossy());
            eprint!("{}", usage());
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("bloom: this build only supports Unix-socket daemons");
    std::process::ExitCode::from(1)
}
