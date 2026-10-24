//! Supervised engine restart, deliberately not wired into the daemon yet.
//!
//! This module owns the restart sequence only: watch for a generation ending,
//! back off, build a replacement, start it, and publish the new engine into the
//! shared slot. It never sends, replays, or confirms a message, and it never
//! holds the engine slot lock across an await.
//!
//! Initial start failure stays fatal at the call site; this loop only runs after
//! a successful first start supplied by its constructor.

use crate::engine::{Engine, EngineSlot};
use crate::recovery::{FailureKind, GiveUpReason, RecoveryPolicy};
use crate::supervisor::{ShutdownReport, Supervisor};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

/// Time allowed for a generation's transport to stop during cancellation.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Terminal result of one run of [`RecoveryLoop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Cancellation was requested; the transport was stopped and its report kept.
    Cancelled { report: ShutdownReport },
    /// The policy stopped retrying.
    GiveUp(GiveUpReason),
}

/// Repeatedly replaces a finished generation, bounded by [`RecoveryPolicy`].
///
/// The loop owns exactly one current supervisor at a time. A replacement is
/// built only after the previous forwarder has finished, and the previous
/// transport is stopped before the new engine is published.
pub struct RecoveryLoop<B> {
    slot: EngineSlot,
    supervisor: Option<Supervisor>,
    policy: RecoveryPolicy,
    build: B,
}

impl<B> RecoveryLoop<B>
where
    B: FnMut() -> (Supervisor, Arc<Engine>),
{
    /// Build a loop around an already-started first generation.
    ///
    /// `build` is invoked only when a replacement is needed. It must construct
    /// its `Supervisor` over the same engine it returns, as production wiring
    /// does with `Supervisor::new(engine.clone(), sink)`: the loop stops only the
    /// supervisor's transport, so a mismatched pair would leave the published
    /// engine running. The constructor marks the supplied generation as a
    /// successful start so a later death is recoverable rather than treated as a
    /// fatal initial start failure.
    pub fn new(slot: EngineSlot, supervisor: Supervisor, policy: RecoveryPolicy, build: B) -> Self {
        let mut policy = policy;
        policy.record_success();
        Self {
            slot,
            supervisor: Some(supervisor),
            policy,
            build,
        }
    }

    /// Wait for the current generation to finish, then replace it until the
    /// policy gives up or `cancel` becomes true.
    pub async fn run(&mut self, cancel: &mut watch::Receiver<bool>) -> Outcome {
        loop {
            if let Some(supervisor) = self.supervisor.as_mut() {
                let finished = tokio::select! {
                    biased;
                    () = wait_cancelled(cancel) => false,
                    _ = supervisor.wait_finished() => true,
                };
                if !finished {
                    return self.cancel_now().await;
                }
            }

            // Finish the old generation explicitly before any replacement is
            // built, so at most one generation is live at a time.
            let _ = self.stop_current().await;

            let delay = match self.policy.record_failure(FailureKind::ChildExited(None)) {
                Ok(delay) => delay,
                Err(reason) => {
                    self.stop_current().await;
                    return Outcome::GiveUp(reason);
                }
            };

            if !wait_delay(delay, cancel).await {
                return self.cancel_now().await;
            }

            let (mut supervisor, engine) = (self.build)();
            match supervisor.start().await {
                Ok(()) => {
                    *self.slot.write().expect("engine slot lock is not poisoned") =
                        Arc::clone(&engine);
                    self.policy.record_success();
                    self.supervisor = Some(supervisor);
                }
                Err(_) => {
                    // The replacement never became usable. Stop whatever it
                    // opened, keep no live supervisor, and let the policy bound
                    // the next attempt.
                    let _ = supervisor.shutdown(DRAIN_TIMEOUT).await;
                    self.supervisor = None;
                }
            }
        }
    }

    async fn cancel_now(&mut self) -> Outcome {
        let report = self.stop_current().await;
        Outcome::Cancelled { report }
    }

    async fn stop_current(&mut self) -> ShutdownReport {
        match self.supervisor.take() {
            Some(mut supervisor) => {
                supervisor
                    .shutdown(DRAIN_TIMEOUT)
                    .await
                    .unwrap_or(ShutdownReport {
                        discarded_events: 0,
                        drain_incomplete: true,
                    })
            }
            None => ShutdownReport {
                discarded_events: 0,
                drain_incomplete: false,
            },
        }
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Returns true when the delay elapsed, false when cancellation interrupted it.
async fn wait_delay(delay: Duration, cancel: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        biased;
        () = wait_cancelled(cancel) => false,
        () = tokio::time::sleep(delay) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{engine_slot, EngineConfig};
    use async_trait::async_trait;
    use bloom_domain::{
        Capabilities, Conversation, ConversationId, Message, MessageId, OutgoingBody, TransportId,
    };
    use bloom_transport::{Transport, TransportError, TransportEvent};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    struct ScriptedTransport {
        id: TransportId,
        capabilities: Capabilities,
        events: Mutex<Option<mpsc::Sender<TransportEvent>>>,
        stop_count: Arc<AtomicUsize>,
        send_count: Arc<AtomicUsize>,
        fail_start: Arc<std::sync::atomic::AtomicBool>,
        log: Arc<Mutex<Vec<String>>>,
        label: String,
    }

    impl ScriptedTransport {
        fn new(label: &str, log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                id: TransportId::new(label).unwrap(),
                capabilities: Capabilities::default(),
                events: Mutex::new(None),
                stop_count: Arc::new(AtomicUsize::new(0)),
                send_count: Arc::new(AtomicUsize::new(0)),
                fail_start: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                log,
                label: label.to_string(),
            }
        }

        fn close(&self) {
            self.log
                .lock()
                .unwrap()
                .push(format!("closed:{}", self.label));
            self.events.lock().unwrap().take();
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        fn id(&self) -> TransportId {
            self.id.clone()
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }
        async fn take_events(
            &self,
        ) -> bloom_transport::TransportResult<mpsc::Receiver<TransportEvent>> {
            let mut events = self.events.lock().unwrap();
            if events.is_some() {
                return Err(TransportError::Internal);
            }
            let (sender, receiver) = mpsc::channel(8);
            *events = Some(sender);
            Ok(receiver)
        }
        async fn start(&self) -> bloom_transport::TransportResult<()> {
            if self.fail_start.load(Ordering::SeqCst) {
                return Err(TransportError::Internal);
            }
            if self.events.lock().unwrap().is_none() {
                return Err(TransportError::Internal);
            }
            Ok(())
        }
        async fn stop(&self) -> bloom_transport::TransportResult<()> {
            self.stop_count.fetch_add(1, Ordering::SeqCst);
            self.log
                .lock()
                .unwrap()
                .push(format!("stopped:{}", self.label));
            self.close();
            Ok(())
        }
        async fn conversations(&self) -> bloom_transport::TransportResult<Vec<Conversation>> {
            Ok(vec![])
        }
        async fn history(
            &self,
            _: &ConversationId,
            _: u16,
        ) -> bloom_transport::TransportResult<Vec<Message>> {
            Ok(vec![])
        }
        async fn send(
            &self,
            _: &ConversationId,
            _: OutgoingBody,
        ) -> bloom_transport::TransportResult<MessageId> {
            self.send_count.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::TransportUnavailable)
        }
    }

    struct Parts {
        log: Arc<Mutex<Vec<String>>>,
        generations: AtomicUsize,
        builder_calls: AtomicUsize,
        fail_start: Arc<std::sync::atomic::AtomicBool>,
        transports: Mutex<Vec<Arc<ScriptedTransport>>>,
        engines: Mutex<Vec<Arc<Engine>>>,
    }

    impl Parts {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                log: Arc::new(Mutex::new(Vec::new())),
                generations: AtomicUsize::new(0),
                builder_calls: AtomicUsize::new(0),
                fail_start: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                transports: Mutex::new(Vec::new()),
                engines: Mutex::new(Vec::new()),
            })
        }

        fn make_generation(&self) -> (Arc<ScriptedTransport>, Supervisor, Arc<Engine>) {
            let index = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
            let label = format!("gen{index}");
            self.log.lock().unwrap().push(format!("build:{label}"));
            let transport = Arc::new(ScriptedTransport::new(&label, Arc::clone(&self.log)));
            transport
                .fail_start
                .store(self.fail_start.load(Ordering::SeqCst), Ordering::SeqCst);
            self.transports.lock().unwrap().push(Arc::clone(&transport));
            let (sink, _receiver) = mpsc::channel(8);
            let supervisor = Supervisor::new(transport.clone(), sink);
            let engine = Arc::new(Engine::new(EngineConfig {
                dart_executable: PathBuf::from("dart"),
                script: PathBuf::from("engine.dart"),
                state_dir: PathBuf::from("state"),
                library: PathBuf::from("library"),
                sync_chats: false,
            }));
            self.engines.lock().unwrap().push(Arc::clone(&engine));
            (transport, supervisor, engine)
        }

        fn make_replacement(&self) -> (Supervisor, Arc<Engine>) {
            self.builder_calls.fetch_add(1, Ordering::SeqCst);
            let (_, supervisor, engine) = self.make_generation();
            (supervisor, engine)
        }

        fn stop_total(&self) -> usize {
            self.transports
                .lock()
                .unwrap()
                .iter()
                .map(|t| t.stop_count.load(Ordering::SeqCst))
                .sum()
        }

        fn send_total(&self) -> usize {
            self.transports
                .lock()
                .unwrap()
                .iter()
                .map(|t| t.send_count.load(Ordering::SeqCst))
                .sum()
        }
    }

    async fn eventually(label: &str, mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
    }

    fn tiny_policy(attempts: u32) -> RecoveryPolicy {
        RecoveryPolicy::with_limits(Duration::from_millis(1), Duration::from_millis(4), attempts)
    }

    async fn start_loop(
        parts: Arc<Parts>,
        policy: RecoveryPolicy,
    ) -> (
        EngineSlot,
        Arc<ScriptedTransport>,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<Outcome>,
    ) {
        let (first_transport, mut supervisor, engine) = parts.make_generation();
        supervisor.start().await.unwrap();
        let slot = engine_slot(engine);
        let builder = {
            let parts = Arc::clone(&parts);
            move || parts.make_replacement()
        };
        let mut recovery = RecoveryLoop::new(slot.clone(), supervisor, policy, builder);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let handle = tokio::spawn(async move { recovery.run(&mut cancel_rx).await });
        (slot, first_transport, cancel_tx, handle)
    }

    #[tokio::test]
    async fn one_death_rebuilds_once_and_swaps_the_slot() {
        let parts = Parts::new();
        let (slot, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(3)).await;

        first.close();
        eventually("one replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 1
        })
        .await;

        let engines = parts.engines.lock().unwrap().clone();
        assert_eq!(engines.len(), 2);
        assert_eq!(parts.generations.load(Ordering::SeqCst), 2);
        assert!(
            Arc::ptr_eq(&slot.read().unwrap().clone(), &engines[1]),
            "the slot must point at the replacement engine"
        );

        cancel_tx.send(true).unwrap();
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn two_deaths_rebuild_twice() {
        let parts = Parts::new();
        let (slot, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(5)).await;

        first.close();
        eventually("first replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 1
        })
        .await;

        let second = parts.transports.lock().unwrap()[1].clone();
        second.close();
        eventually("second replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 2
        })
        .await;

        let engines = parts.engines.lock().unwrap().clone();
        assert_eq!(engines.len(), 3);
        assert_eq!(parts.generations.load(Ordering::SeqCst), 3);
        assert!(
            Arc::ptr_eq(&slot.read().unwrap().clone(), &engines[2]),
            "the slot must point at the newest engine"
        );

        cancel_tx.send(true).unwrap();
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn builder_runs_only_after_the_previous_generation_finished() {
        let parts = Parts::new();
        let (_, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(3)).await;

        first.close();
        eventually("one replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 1
        })
        .await;

        let log = parts.log.lock().unwrap().clone();
        let closed = log
            .iter()
            .position(|entry| entry == "closed:gen1")
            .expect("the first generation closed");
        let build = log
            .iter()
            .position(|entry| entry == "build:gen2")
            .expect("the replacement was built");
        let stopped = log
            .iter()
            .position(|entry| entry == "stopped:gen1")
            .expect("the first transport stopped");
        assert!(closed < stopped, "the old close must precede its stop");
        assert!(
            stopped < build,
            "the old transport must stop before a replacement is built"
        );

        cancel_tx.send(true).unwrap();
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn open_generation_is_not_stopped_or_replaced() {
        let parts = Parts::new();
        let (_, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(3)).await;

        // Several policy delays pass while the transport is still open. A loop
        // that did not await completion would stop and rebuild in this window.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(parts.builder_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            parts.stop_total(),
            0,
            "an open generation must not be stopped"
        );
        assert!(
            first.events.lock().unwrap().is_some(),
            "the generation is still open"
        );

        first.close();
        eventually("one replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(parts.stop_total(), 1);

        cancel_tx.send(true).unwrap();
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
    }

    #[tokio::test]
    async fn give_up_returns_the_reason_and_stops_building() {
        let parts = Parts::new();
        let (_, first, _cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(1)).await;

        parts.fail_start.store(true, Ordering::SeqCst);
        first.close();
        let outcome = handle.await.unwrap();
        assert_eq!(outcome, Outcome::GiveUp(GiveUpReason::AttemptsExhausted));
        assert_eq!(parts.builder_calls.load(Ordering::SeqCst), 1);
        assert_eq!(parts.generations.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_during_a_long_delay_returns_promptly_and_stops_once() {
        let parts = Parts::new();
        let policy =
            RecoveryPolicy::with_limits(Duration::from_millis(400), Duration::from_millis(400), 3);
        let (_, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), policy).await;

        first.close();
        // Holding a sender clone would keep the channel open, so observe the
        // closure in the fake's log instead of awaiting `Sender::closed`.
        eventually("the first generation to close", || {
            parts
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|entry| entry == "closed:gen1")
        })
        .await;
        assert_eq!(
            parts.builder_calls.load(Ordering::SeqCst),
            0,
            "the loop should still be inside the delay"
        );

        let started = tokio::time::Instant::now();
        cancel_tx.send(true).unwrap();
        let outcome = tokio::time::timeout(Duration::from_millis(150), handle)
            .await
            .expect("cancellation should interrupt the delay")
            .unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(
            parts.stop_total(),
            1,
            "the transport must stop exactly once"
        );
    }

    #[tokio::test]
    async fn loop_never_sends_a_message() {
        let parts = Parts::new();
        let (_, first, cancel_tx, handle) = start_loop(Arc::clone(&parts), tiny_policy(5)).await;

        first.close();
        eventually("first replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 1
        })
        .await;
        parts.transports.lock().unwrap()[1].close();
        eventually("second replacement", || {
            parts.builder_calls.load(Ordering::SeqCst) == 2
        })
        .await;

        assert_eq!(parts.send_total(), 0, "the recovery loop sent a message");

        cancel_tx.send(true).unwrap();
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
        assert_eq!(parts.send_total(), 0);
    }
}
