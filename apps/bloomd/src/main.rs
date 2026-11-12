//! bloomd: the Bloom daemon.
//!
//! Owns the encrypted store and the local IPC socket, supervises the engine sidecar
//! child process, persists engine events before fanning them out, and serves clients.

use bloom_secrets::OsKeyring;
use bloomd::{
    config::DaemonConfig,
    engine::{engine_slot, Engine, EVENT_CAPACITY},
    generation::{Outcome, RecoveryLoop},
    ipc::{serve_with_subscriptions, Listener, SubscriptionHub},
    readiness::Readiness,
    recovery::RecoveryPolicy,
    service::Service,
    store::EncryptedStore,
    supervisor::Supervisor,
};
use std::{
    fmt,
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

const USAGE: &str = "\
Usage: bloomd [OPTIONS]

Options:
      --demo   Run the isolated synthetic demo daemon
  -h, --help   Print this help and exit
";

/// Arguments accepted by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DaemonArgs {
    demo: bool,
}

/// Result of parsing the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgOutcome {
    Run(DaemonArgs),
    Help,
}

/// Why the command line was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ArgError {
    Unknown(String),
    Duplicate(&'static str),
    Conflict(&'static str),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(argument) => write!(f, "unknown argument {argument:?}"),
            Self::Duplicate(argument) => write!(f, "duplicate argument {argument}"),
            Self::Conflict(argument) => write!(f, "conflicting arguments: {argument}"),
        }
    }
}

/// Parse the daemon command line without touching configuration or discovery.
fn parse_args<I, S>(args: I) -> Result<ArgOutcome, ArgError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut demo = false;
    let mut help = false;
    for argument in args {
        let argument = argument.into();
        match argument.as_str() {
            "--demo" => {
                if demo {
                    return Err(ArgError::Duplicate("--demo"));
                }
                if help {
                    return Err(ArgError::Conflict("--help with --demo"));
                }
                demo = true;
            }
            "-h" | "--help" => {
                if help {
                    return Err(ArgError::Duplicate("--help"));
                }
                if demo {
                    return Err(ArgError::Conflict("--help with --demo"));
                }
                help = true;
            }
            other => return Err(ArgError::Unknown(other.to_owned())),
        }
    }
    if help {
        Ok(ArgOutcome::Help)
    } else {
        Ok(ArgOutcome::Run(DaemonArgs { demo }))
    }
}

/// Parsed demo request handed to the fixed dispatch seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DemoContext {
    /// Always true: the seam is only called for a demo request.
    requested: bool,
}

impl DemoContext {
    fn requested() -> Self {
        Self { requested: true }
    }
}

/// Where startup goes after the demo dispatch seam.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DemoDispatch {
    /// Continue with normal startup.
    ProceedNormal,
    /// Demo startup handled the process; exit with this code.
    Exit(ExitCode),
}

/// Call the demo dispatch seam when the command line requested demo mode.
fn demo_dispatch(args: DaemonArgs) -> DemoDispatch {
    if args.demo {
        run_demo_daemon(DemoContext::requested())
    } else {
        DemoDispatch::ProceedNormal
    }
}

/// Fixed demo dispatch seam.
///
/// The signature and [`DemoDispatch`] shape are fixed so dispatch stays here.
/// The body calls the U2-owned demo module, which owns root validation, demo
/// startup, and cleanup. A demo request never falls through to normal startup.
fn run_demo_daemon(context: DemoContext) -> DemoDispatch {
    debug_assert!(context.requested);
    match bloomd::demo::run() {
        Ok(()) => DemoDispatch::Exit(ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("bloomd: demo startup refused: {error}");
            DemoDispatch::Exit(ExitCode::from(1))
        }
    }
}

/// Name the signal that ended the run, so the log says how the daemon stopped.
///
/// The launcher stops the daemon with SIGTERM on exit, so handling only Ctrl+C
/// would take the ungraceful path on every normal shutdown: no drain, and no
/// record of why it stopped.
#[cfg(unix)]
async fn termination_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        // If the SIGTERM stream cannot be installed, Ctrl+C still stops the daemon.
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return "SIGINT";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = terminate.recv() => "SIGTERM",
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "SIGINT"
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(ArgOutcome::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(ArgOutcome::Run(args)) => args,
        Err(error) => {
            eprintln!("bloomd: {error}");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    // Demo dispatch precedes real configuration and discovery. A demo request
    // must never fall through to normal startup.
    match demo_dispatch(args) {
        DemoDispatch::Exit(code) => return code,
        DemoDispatch::ProceedNormal => {
            if args.demo {
                eprintln!("bloomd: demo startup declined; refusing normal startup");
                return ExitCode::from(1);
            }
        }
    }

    let config = match DaemonConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("bloomd: {error}");
            return ExitCode::from(2);
        }
    };

    if let Some(parent) = config.socket_path.parent() {
        if let Err(error) = create_private_dir(parent) {
            eprintln!("bloomd: cannot prepare socket directory: {error}");
            return ExitCode::from(2);
        }
    }

    let store = match EncryptedStore::open_or_create(&config.store_path, &OsKeyring) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(error) => {
            eprintln!("bloomd: cannot open encrypted store: {error}");
            return ExitCode::from(2);
        }
    };
    // The daemon owns the required startup refresh explicitly. The sidecar must
    // not run its own unsolicited listing, even when this store is empty.
    let mut engine_config = config.engine.clone();
    engine_config.sync_chats = false;

    eprintln!(
        "bloomd: starting pid={} socket={}",
        std::process::id(),
        config.socket_path.display()
    );
    // Claim the socket before touching the engine. The sidecar owns the user's real
    // registration state, so a second daemon must be refused here rather than after
    // it has already started an engine and risked two owners of that state.
    let listener = match Listener::bind(&config.socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("bloomd: cannot bind socket: {error}");
            return ExitCode::from(2);
        }
    };

    let hub = SubscriptionHub::with_default_capacity();
    let readiness = Readiness::new_starting(hub.clone());
    // Establish the initial generation before the engine can emit Active. The
    // first session event then marks engine readiness without racing the sync
    // operation created below.
    let _ = readiness.begin(1);
    let engine = Arc::new(Engine::new(engine_config.clone()));
    // The service reads the current engine from this slot, so the recovery loop can
    // replace its contents without rebuilding the service.
    let slot = engine_slot(engine.clone());

    let (sink, events) = mpsc::channel(EVENT_CAPACITY);
    let mut supervisor = Supervisor::new(engine.clone(), sink.clone());
    if let Err(error) = supervisor.start().await {
        eprintln!("bloomd: engine unavailable: {error}");
        return ExitCode::from(3);
    }

    let service = Arc::new(Service::with_engine_slot_and_readiness(
        store,
        slot.clone(),
        hub.clone(),
        readiness.clone(),
    ));
    let generations = Arc::new(AtomicU64::new(1));

    // Replacements are built over the same sink and engine config, so the new
    // supervisor always wraps the engine it returns. The generation advances and
    // readiness is invalidated before the replacement is exposed.
    let builder = {
        let service = service.clone();
        let generations = generations.clone();
        let engine_config = engine_config.clone();
        let sink = sink.clone();
        move || {
            let generation = generations.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = service.readiness().begin(generation);
            let engine = Arc::new(Engine::with_generation(engine_config.clone(), generation));
            let supervisor = Supervisor::new(engine.clone(), sink.clone());
            (supervisor, engine)
        }
    };
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let mut recovery =
        RecoveryLoop::new(slot.clone(), supervisor, RecoveryPolicy::default(), builder)
            .with_recovery_hook({
                let readiness = readiness.clone();
                move || readiness.begin_recovery()
            })
            .with_started_hook({
                let service = service.clone();
                let generations = generations.clone();
                move || {
                    let generation = generations.load(Ordering::SeqCst);
                    let service = service.clone();
                    tokio::spawn(async move {
                        service.run_sync_for_generation(generation).await;
                    });
                }
            })
            .with_failed_hook({
                let readiness = readiness.clone();
                move || readiness.recovery_failed()
            });
    let recovery_task = tokio::spawn(async move { recovery.run(&mut cancel_rx).await });

    let persistence = tokio::spawn(service.clone().persist_events(events));
    // The initial required refresh starts only after the supervisor is running and
    // the persistence receiver exists. An empty listing is a valid complete result.
    {
        let service = service.clone();
        tokio::spawn(async move {
            service.run_sync_for_generation(1).await;
        });
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(connection) => {
                    let service = service.clone();
                    let hub = hub.clone();
                    tokio::spawn(async move {
                        serve_with_subscriptions(connection, &*service, &hub).await;
                    });
                }
                Err(_) => continue,
            },
            signal = termination_signal() => {
                eprintln!("bloomd: termination signal received ({signal})");
                break;
            }
        }
    }

    drop(listener);
    // Leave no stale socket behind for the next start to trip over.
    let _ = std::fs::remove_file(&config.socket_path);
    let _ = cancel_tx.send(true);
    // The loop stops the current generation and drops its builder, so this is the
    // last sender and persistence observes end-of-stream. If the loop overruns its
    // budget, abort it so it cannot publish another generation after the service is
    // dropped, and so its captured sink closes for persistence.
    let mut recovery_task = recovery_task;
    let recovery_result = match tokio::time::timeout(SHUTDOWN_DRAIN, &mut recovery_task).await {
        Ok(joined) => Ok(joined),
        Err(elapsed) => {
            recovery_task.abort();
            let _ = recovery_task.await;
            Err(elapsed)
        }
    };
    drop(service);
    let persistence_result = tokio::time::timeout(SHUTDOWN_DRAIN, persistence).await;

    let mut clean = true;
    if let Some(message) = RecoveryShutdown::classify(recovery_result).incomplete_message() {
        eprintln!("bloomd: shutdown incomplete: {message}");
        clean = false;
    }
    match persistence_result {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            eprintln!("bloomd: shutdown incomplete: persistence task failed");
            clean = false;
        }
        Err(_) => {
            eprintln!("bloomd: shutdown incomplete: persistence drain timed out");
            clean = false;
        }
    }
    if clean {
        eprintln!("bloomd: clean shutdown");
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
    }
}

/// Terminal recovery-loop result reduced to what the clean/unclean decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryShutdown {
    /// The loop stopped the current generation and reported its drain.
    Cancelled {
        discarded_events: usize,
        drain_incomplete: bool,
    },
    /// The retry policy stopped trying.
    GiveUp,
    /// The task did not finish inside the shutdown budget.
    TimedOut,
    /// The task panicked or was cancelled.
    Failed,
}

impl RecoveryShutdown {
    fn classify(
        result: Result<Result<Outcome, tokio::task::JoinError>, tokio::time::error::Elapsed>,
    ) -> Self {
        match result {
            Ok(Ok(Outcome::Cancelled { report })) => Self::Cancelled {
                discarded_events: report.discarded_events,
                drain_incomplete: report.drain_incomplete,
            },
            Ok(Ok(Outcome::GiveUp(_))) => Self::GiveUp,
            Ok(Err(_)) => Self::Failed,
            Err(_) => Self::TimedOut,
        }
    }

    /// Static, path-free reason when this result makes shutdown unclean.
    fn incomplete_message(self) -> Option<&'static str> {
        match self {
            Self::Cancelled {
                discarded_events: 0,
                drain_incomplete: false,
            } => None,
            Self::Cancelled { .. } => Some("engine event forwarding did not fully drain"),
            Self::GiveUp => Some("engine restart policy gave up"),
            Self::TimedOut => Some("engine recovery loop did not stop in time"),
            Self::Failed => Some("engine producer failed to stop"),
        }
    }
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bloomd::{generation::Outcome, recovery::GiveUpReason, supervisor::ShutdownReport};

    fn cancelled(discarded_events: usize, drain_incomplete: bool) -> RecoveryShutdown {
        RecoveryShutdown::classify(Ok(Ok(Outcome::Cancelled {
            report: ShutdownReport {
                discarded_events,
                drain_incomplete,
            },
        })))
    }

    #[test]
    fn clean_cancelled_is_clean() {
        assert_eq!(cancelled(0, false).incomplete_message(), None);
    }

    #[test]
    fn cancelled_with_discarded_events_is_unclean() {
        assert_eq!(
            cancelled(3, false).incomplete_message(),
            Some("engine event forwarding did not fully drain")
        );
    }

    #[test]
    fn cancelled_with_incomplete_drain_is_unclean() {
        assert_eq!(
            cancelled(0, true).incomplete_message(),
            Some("engine event forwarding did not fully drain")
        );
    }

    #[test]
    fn give_up_is_unclean() {
        let result =
            RecoveryShutdown::classify(Ok(Ok(Outcome::GiveUp(GiveUpReason::AttemptsExhausted))));
        assert_eq!(
            result.incomplete_message(),
            Some("engine restart policy gave up")
        );
    }

    #[tokio::test]
    async fn timeout_waiting_for_the_task_is_unclean() {
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        assert_eq!(
            RecoveryShutdown::classify(Err(elapsed)).incomplete_message(),
            Some("engine recovery loop did not stop in time")
        );
    }

    #[test]
    fn help_is_safe_and_exits_zero() {
        assert_eq!(parse_args(["--help"]), Ok(ArgOutcome::Help));
        assert_eq!(parse_args(["-h"]), Ok(ArgOutcome::Help));
        assert!(USAGE.contains("Usage: bloomd"));
    }

    #[test]
    fn unknown_argument_is_rejected() {
        assert_eq!(
            parse_args(["--nope"]),
            Err(ArgError::Unknown("--nope".to_owned()))
        );
        assert_eq!(
            parse_args(["positional"]),
            Err(ArgError::Unknown("positional".to_owned()))
        );
    }

    #[test]
    fn duplicate_and_conflicting_arguments_are_rejected() {
        assert_eq!(
            parse_args(["--demo", "--demo"]),
            Err(ArgError::Duplicate("--demo"))
        );
        assert_eq!(
            parse_args(["--help", "--help"]),
            Err(ArgError::Duplicate("--help"))
        );
        assert_eq!(
            parse_args(["--demo", "--help"]),
            Err(ArgError::Conflict("--help with --demo"))
        );
        assert_eq!(
            parse_args(["--help", "--demo"]),
            Err(ArgError::Conflict("--help with --demo"))
        );
    }

    #[test]
    fn demo_flag_reaches_the_stub_hook() {
        let args = parse_args(["--demo"]).unwrap();
        assert_eq!(args, ArgOutcome::Run(DaemonArgs { demo: true }));
        let ArgOutcome::Run(args) = args else {
            panic!("expected runnable arguments");
        };
        assert_eq!(demo_dispatch(args), DemoDispatch::Exit(ExitCode::from(1)));
    }

    #[test]
    fn stub_hook_fails_without_touching_state() {
        // The stub body only prints and returns; it does not read configuration,
        // discovery, the store, the keychain, a socket, or the environment.
        assert_eq!(
            run_demo_daemon(DemoContext::requested()),
            DemoDispatch::Exit(ExitCode::from(1))
        );
        // Calling it again is deterministic and still touches nothing.
        assert_eq!(
            run_demo_daemon(DemoContext::requested()),
            DemoDispatch::Exit(ExitCode::from(1))
        );
    }

    #[test]
    fn normal_mode_does_not_dispatch_demo() {
        let args = parse_args(std::iter::empty::<&str>()).unwrap();
        assert_eq!(args, ArgOutcome::Run(DaemonArgs { demo: false }));
        let ArgOutcome::Run(args) = args else {
            panic!("expected runnable arguments");
        };
        assert_eq!(demo_dispatch(args), DemoDispatch::ProceedNormal);
    }

    #[tokio::test]
    async fn failed_task_is_unclean() {
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        let error = handle.await.unwrap_err();
        assert_eq!(
            RecoveryShutdown::classify(Ok(Err(error))).incomplete_message(),
            Some("engine producer failed to stop")
        );
    }
}
