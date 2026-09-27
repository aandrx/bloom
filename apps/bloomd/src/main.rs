//! bloomd: the Bloom daemon.
//!
//! Owns the encrypted store and the local IPC socket, supervises the engine sidecar
//! child process, persists engine events before fanning them out, and serves clients.

use bloom_secrets::OsKeyring;
use bloomd::{
    config::DaemonConfig,
    engine::{Engine, EVENT_CAPACITY},
    ipc::{serve_with_subscriptions, Listener, SubscriptionHub},
    service::Service,
    store::EncryptedStore,
    supervisor::Supervisor,
};
use std::{
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

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
    // Only ask the engine for a full conversation listing when this store has none.
    let store_is_empty = {
        let guard = store.lock().unwrap();
        bloomd::store::list_conversations(&guard).map_or(true, |rows| rows.is_empty())
    };
    let mut engine_config = config.engine.clone();
    engine_config.sync_chats = store_is_empty;

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
    let engine = Arc::new(Engine::new(engine_config));

    let (sink, mut events) = mpsc::channel(EVENT_CAPACITY);
    let mut supervisor = Supervisor::new(engine.clone(), sink);
    if let Err(error) = supervisor.start().await {
        eprintln!("bloomd: engine unavailable: {error}");
        return ExitCode::from(3);
    }

    let service = Arc::new(Service::new(store, engine.clone(), hub.clone()));
    let persistence = {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let Err(error) = service.persist(&event) {
                    eprintln!("bloomd: dropping event that failed to persist: {error}");
                }
            }
        })
    };

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
    let _ = supervisor.shutdown(SHUTDOWN_DRAIN).await;
    drop(service);
    persistence.abort();
    eprintln!("bloomd: clean shutdown");
    ExitCode::SUCCESS
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
