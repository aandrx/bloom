//! Demo-mode daemon startup and isolation guards.
//!
//! A demo session runs the real IPC and subscription loop against a synthetic
//! store inside one fresh private root under `/tmp`. This module is the only
//! demo startup path: it validates the root through [`bloom_ipc::demo`], refuses
//! a root that is not fresh, opens only `data/store.db` with the public demo key,
//! binds only `runtime/bloomd.sock`, and removes the root when the daemon exits.
//!
//! Nothing here reads real configuration or discovery: no `DaemonConfig`, no
//! engine, no supervisor, no recovery loop, no Dart/Flatpak lookup, no
//! registration, and no `OsKeyring`. Every `BLOOM_*` variable except
//! `BLOOM_DEMO_ROOT` (and inherited `HOME`/XDG destinations) is ignored by never
//! being consulted.
//!
//! The synthetic data seeder lives in the sibling `seeder` module, which is a
//! later unit; this module deliberately does not declare it yet.

use crate::{
    ipc::{serve_with_subscriptions, Listener, ListenerError, SubscriptionHub},
    service::Service,
    store::{EncryptedStore, StoreError},
};
use bloom_ipc::demo::{DemoPaths, DemoRootError};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Run the demo daemon for the root named by `BLOOM_DEMO_ROOT`.
///
/// The dispatch seam in `main.rs` is synchronous, so the async daemon runs on a
/// dedicated thread with its own current-thread runtime. That keeps the seam
/// shape fixed and avoids starting a runtime inside the process runtime.
pub fn run() -> Result<(), DemoError> {
    let paths = DemoPaths::from_env().map_err(DemoError::Root)?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| DemoError::Runtime(error.to_string()))?;
        runtime.block_on(serve(paths))
    })
    .join()
    .map_err(|_| DemoError::Runtime("demo daemon thread panicked".to_owned()))?
}

/// Serve the demo session until a termination signal arrives.
async fn serve(paths: DemoPaths) -> Result<(), DemoError> {
    // Refusal comes first: a pre-existing daemon artifact means this root is not
    // fresh. The database is never opened or inspected to decide this.
    ensure_fresh(&paths)?;
    prepare_directories(&paths)?;

    let store = EncryptedStore::open_demo(&paths.store_path).map_err(DemoError::Store)?;
    let listener = Listener::bind(&paths.socket_path).map_err(DemoError::Listener)?;
    let hub = SubscriptionHub::with_default_capacity();
    let service = Arc::new(Service::demo(Arc::new(Mutex::new(store)), hub.clone()));

    // From here on the session owns the root, so every exit path (signal, error,
    // or panic) removes it. A killed process leaves the root behind; the next
    // launch uses a fresh random root and never adopts it.
    let _cleanup = RootCleanup::arm(paths.root.clone());

    eprintln!(
        "bloomd: demo session root={} socket={}",
        paths.root.display(),
        paths.socket_path.display()
    );

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
                eprintln!("bloomd: demo termination signal received ({signal})");
                break;
            }
        }
    }

    drop(listener);
    Ok(())
}

/// Refuse a root that already holds a daemon artifact.
///
/// The launcher creates the layout, but only the daemon creates the store and
/// the socket. Their presence means a previous session crashed or was reused;
/// the root is refused rather than repaired, adopted, or inspected.
fn ensure_fresh(paths: &DemoPaths) -> Result<(), DemoError> {
    for (artifact, path) in [
        ("data/store.db", &paths.store_path),
        ("runtime/bloomd.sock", &paths.socket_path),
    ] {
        // `symlink_metadata` also catches a dangling link squatting on the path.
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(DemoError::StaleRoot { artifact });
        }
    }
    Ok(())
}

/// Create the two directories the daemon owns, privately.
fn prepare_directories(paths: &DemoPaths) -> Result<(), DemoError> {
    for directory in [paths.store_path.parent(), paths.socket_path.parent()] {
        let Some(directory) = directory else {
            return Err(DemoError::Layout("demo artifact has no parent directory"));
        };
        create_private_dir(directory)?;
    }
    Ok(())
}

fn create_private_dir(path: &std::path::Path) -> Result<(), DemoError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(DemoError::Layout("demo directory is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .map_err(|error| DemoError::Io(error.to_string()))
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir(path).map_err(|error| DemoError::Io(error.to_string()))
            }
        }
        Err(error) => Err(DemoError::Io(error.to_string())),
    }
}

/// Removes the session root on drop. Best effort, and never follows a symlink
/// for the root itself (validation already rejected a symlinked root).
struct RootCleanup {
    root: PathBuf,
}

impl RootCleanup {
    fn arm(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Drop for RootCleanup {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("bloomd: demo cleanup failed: {error}"),
        }
    }
}

/// Name the signal that ended the run. The launcher stops the daemon with
/// SIGTERM, so Ctrl+C alone would miss the normal shutdown path.
#[cfg(unix)]
async fn termination_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
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

/// Why demo startup was refused or stopped.
#[derive(Debug)]
pub enum DemoError {
    /// `BLOOM_DEMO_ROOT` is missing or the root failed validation.
    Root(DemoRootError),
    /// The root already contains a daemon artifact, so it is not fresh.
    StaleRoot { artifact: &'static str },
    /// The fixed demo layout could not be prepared.
    Layout(&'static str),
    /// The synthetic store could not be opened.
    Store(StoreError),
    /// The demo socket could not be bound.
    Listener(ListenerError),
    /// A filesystem operation on the root failed.
    Io(String),
    /// The demo runtime could not be started or stopped cleanly.
    Runtime(String),
}

impl fmt::Display for DemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(error) => write!(f, "demo context rejected: {error}"),
            Self::StaleRoot { artifact } => {
                write!(f, "demo root is not fresh: {artifact} already exists")
            }
            Self::Layout(reason) => write!(f, "demo layout rejected: {reason}"),
            Self::Store(error) => write!(f, "cannot open demo store: {error}"),
            Self::Listener(error) => write!(f, "cannot bind demo socket: {error}"),
            Self::Io(error) => write!(f, "demo filesystem operation failed: {error}"),
            Self::Runtime(error) => write!(f, "demo runtime failed: {error}"),
        }
    }
}

impl std::error::Error for DemoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Listener(error) => Some(error),
            Self::StaleRoot { .. } | Self::Layout(_) | Self::Io(_) | Self::Runtime(_) => None,
        }
    }
}
