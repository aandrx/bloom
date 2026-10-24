//! Runtime restart proof for the real engine adapter.
//!
//! A generated shell wrapper stands in for the Dart sidecar: it records its own
//! pid and then sleeps, holding stdout and stdin open. The test kills that child
//! and observes the real `Engine`, `Supervisor`, and `RecoveryLoop` replace it.
//! Nothing here uses the real engine, keychain, account, or network.

#![cfg(target_os = "linux")]

use bloomd::{
    engine::{engine_slot, Engine, EngineConfig},
    generation::{Outcome, RecoveryLoop},
    recovery::{GiveUpReason, RecoveryPolicy},
    supervisor::Supervisor,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const POLL: Duration = Duration::from_millis(10);
const PID_WAIT: Duration = Duration::from_secs(5);

/// Removes the temporary directory and kills any child still listed in the pid
/// file. Runs on success, on assertion failure, and on panic unwinding.
struct Harness {
    dir: PathBuf,
    pid_file: PathBuf,
    config: EngineConfig,
}

impl Harness {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bloomd-engine-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("stale harness directory is removable");
        }
        fs::create_dir_all(&dir).expect("harness directory is creatable");

        let pid_file = dir.join("children.pid");
        let wrapper = dir.join("wrapper.sh");
        let script = dir.join("engine.dart");
        let library = dir.join("libengine.so");
        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).expect("state directory is creatable");
        fs::write(&script, "// synthetic engine script placeholder\n")
            .expect("dummy script is writable");
        fs::write(&library, "synthetic library\n").expect("dummy library is writable");
        // Block on stdin and exit when it closes, which is the sidecar's real
        // shutdown path, so a normal stop drains instead of hitting the kill
        // deadline. `kill -9` still simulates an unexpected death.
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> '{}'\nexec python3 -c 'import sys; sys.stdin.buffer.read()'\n",
            pid_file.display()
        );
        fs::write(&wrapper, body).expect("wrapper is writable");
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
            .expect("wrapper is executable");

        let config = EngineConfig {
            dart_executable: wrapper,
            script,
            state_dir,
            library,
            sync_chats: false,
        };
        Self {
            dir,
            pid_file,
            config,
        }
    }

    fn pids(&self) -> Vec<String> {
        fs::read_to_string(&self.pid_file)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn wait_for_pids(&self, count: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + PID_WAIT;
        loop {
            let pids = self.pids();
            if pids.len() >= count {
                return pids;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {count} child pid(s); saw {pids:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_for_distinct_pid(&self, previous: &str) -> String {
        let deadline = tokio::time::Instant::now() + PID_WAIT;
        loop {
            if let Some(pid) = self.pids().into_iter().find(|pid| pid != previous) {
                return pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for a replacement child; saw {:?}",
                self.pids()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_until_dead(&self, pid: &str, budget: Duration) {
        let deadline = tokio::time::Instant::now() + budget;
        while is_alive(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} was still alive after {budget:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for pid in self.pids() {
            kill(&pid, "-9");
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn kill(pid: &str, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn is_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Build the first generation and start it through a supervisor.
async fn start_first(
    harness: &Harness,
    sink: &mpsc::Sender<bloom_transport::TransportEvent>,
) -> (Arc<Engine>, Supervisor) {
    let engine = Arc::new(Engine::new(harness.config.clone()));
    let mut supervisor = Supervisor::new(engine.clone(), sink.clone());
    supervisor.start().await.expect("first generation starts");
    (engine, supervisor)
}

/// The replacement builder: a fresh engine and a supervisor over that engine.
fn builder(
    config: EngineConfig,
    sink: mpsc::Sender<bloom_transport::TransportEvent>,
) -> impl FnMut() -> (Supervisor, Arc<Engine>) {
    move || {
        let engine = Arc::new(Engine::new(config.clone()));
        let supervisor = Supervisor::new(engine.clone(), sink.clone());
        (supervisor, engine)
    }
}

fn keep_sink_drained(mut receiver: mpsc::Receiver<bloom_transport::TransportEvent>) {
    tokio::spawn(async move { while receiver.recv().await.is_some() {} });
}

#[tokio::test]
async fn engine_child_restarts_after_unexpected_death_and_cancels_cleanly() {
    let harness = Harness::new();
    let (sink, sink_rx) = mpsc::channel(4);
    keep_sink_drained(sink_rx);

    let (first_engine, supervisor) = start_first(&harness, &sink).await;
    let first_pid = harness.wait_for_pids(1).await[0].clone();
    assert!(is_alive(&first_pid), "the first child must be running");

    let slot = engine_slot(first_engine.clone());
    let policy =
        RecoveryPolicy::with_limits(Duration::from_millis(10), Duration::from_millis(40), 5);
    let mut loop_ = RecoveryLoop::new(
        slot.clone(),
        supervisor,
        policy,
        builder(harness.config.clone(), sink.clone()),
    )
    // Stay well past any generation lifetime in this test so the budget only
    // resets when a generation outlives the stability window.
    .with_stability(Duration::from_secs(30));
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let mut handle = tokio::spawn(async move { loop_.run(&mut cancel_rx).await });

    kill(&first_pid, "-9");
    let second_pid = harness.wait_for_distinct_pid(&first_pid).await;

    assert!(
        !handle.is_finished(),
        "the recovery loop returned instead of restarting"
    );
    let restarted = slot.read().expect("slot lock is not poisoned").clone();
    assert!(
        !Arc::ptr_eq(&first_engine, &restarted),
        "the slot still holds the dead generation's engine"
    );

    // Exactly one live child: the replacement, never the killed generation.
    assert_eq!(
        harness.pids().len(),
        2,
        "exactly two children may ever be recorded: the dead generation and its replacement"
    );
    assert!(
        is_alive(&second_pid),
        "the replacement child must be running"
    );
    assert!(!is_alive(&first_pid), "the killed child must be gone");

    cancel_tx.send(true).expect("cancel receiver is alive");
    let outcome = tokio::time::timeout(Duration::from_secs(20), &mut handle)
        .await
        .expect("the loop cancels within the budget")
        .expect("the recovery task does not panic");
    match outcome {
        Outcome::Cancelled { report } => {
            assert_eq!(report.discarded_events, 0);
            assert!(!report.drain_incomplete, "cancellation must drain cleanly");
        }
        other => panic!("expected cancellation, got {other:?}"),
    }
    harness.wait_until_dead(&second_pid, PID_WAIT).await;
}

#[tokio::test]
async fn repeated_deaths_exhaust_the_attempt_budget() {
    let harness = Harness::new();
    let (sink, sink_rx) = mpsc::channel(4);
    keep_sink_drained(sink_rx);

    let (first_engine, supervisor) = start_first(&harness, &sink).await;
    harness.wait_for_pids(1).await;

    let slot = engine_slot(first_engine.clone());
    let policy =
        RecoveryPolicy::with_limits(Duration::from_millis(5), Duration::from_millis(10), 2);
    let mut loop_ = RecoveryLoop::new(
        slot,
        supervisor,
        policy,
        builder(harness.config.clone(), sink.clone()),
    )
    // Stay well past any generation lifetime in this test so the budget only
    // resets when a generation outlives the stability window.
    .with_stability(Duration::from_secs(30));
    let (_cancel_tx, mut cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(async move { loop_.run(&mut cancel_rx).await });

    let mut killed = 0usize;
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let pids = harness.pids();
            if let Some(pid) = pids.get(killed) {
                kill(pid, "-9");
                killed += 1;
                continue;
            }
            if handle.is_finished() {
                break handle.await.expect("the recovery task does not panic");
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("a crash loop must give up rather than restart forever");

    assert_eq!(
        outcome,
        Outcome::GiveUp(GiveUpReason::AttemptsExhausted),
        "the policy must stop after the bounded attempt budget"
    );
    assert_eq!(
        killed, 3,
        "one first child plus two replacements were killed"
    );
    for pid in harness.pids() {
        harness.wait_until_dead(&pid, PID_WAIT).await;
    }
}
