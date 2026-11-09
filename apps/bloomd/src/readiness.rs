//! Revisioned daemon readiness for the required startup metadata refresh.
//!
//! One required refresh runs per engine generation. The gate is open only after a
//! complete result for the current generation and operation. A stale generation,
//! stale operation, late result, or disconnect leaves it closed. This module owns
//! only state and publication; it never starts, retries, or cancels a refresh.

use crate::ipc::SubscriptionHub;
use bloom_ipc::{
    Event, EventKind, ReadinessError, ReadinessOutcome, ReadinessPhase, ReadinessSnapshot,
};
use std::sync::{Arc, Mutex};

/// Identifies one accepted readiness operation. A result carrying any other
/// generation or operation is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadinessToken {
    pub generation: u64,
    pub operation: u64,
}

/// Terminal result of one required refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Complete(u32),
    Limited(u32),
    Failed,
    Unavailable,
    Timeout,
}

#[derive(Clone)]
pub struct Readiness {
    state: Arc<Mutex<ReadinessSnapshot>>,
    hub: SubscriptionHub,
}

impl Readiness {
    fn with_snapshot(snapshot: ReadinessSnapshot, hub: SubscriptionHub) -> Self {
        Self {
            state: Arc::new(Mutex::new(snapshot)),
            hub,
        }
    }

    /// Production initial state: no generation has completed a refresh yet.
    pub fn new_starting(hub: SubscriptionHub) -> Self {
        Self::with_snapshot(
            ReadinessSnapshot {
                generation: 0,
                operation: 0,
                revision: 0,
                phase: ReadinessPhase::Starting,
                outcome: None,
                observed_records: 0,
                can_send: false,
                engine_ready: false,
                bounded_error: None,
            },
            hub,
        )
    }

    /// Test convenience for services that are unrelated to startup readiness.
    /// Production wiring uses [`Readiness::new_starting`] and the explicit sync.
    pub fn new_ready(generation: u64, hub: SubscriptionHub) -> Self {
        Self::with_snapshot(
            ReadinessSnapshot {
                generation,
                operation: 0,
                revision: 0,
                phase: ReadinessPhase::Ready,
                outcome: Some(ReadinessOutcome::Complete),
                observed_records: 0,
                can_send: true,
                engine_ready: true,
                bounded_error: None,
            },
            hub,
        )
    }

    pub fn snapshot(&self) -> ReadinessSnapshot {
        self.state.lock().unwrap().clone()
    }

    pub fn can_send(&self) -> bool {
        self.state.lock().unwrap().can_send
    }

    pub fn current_generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    /// Begin a required refresh for `generation`. A lower generation is stale and
    /// is ignored. A greater generation resets engine readiness until the caller
    /// reports a successful start. Returns `None` for a stale generation.
    pub fn begin(&self, generation: u64) -> Option<ReadinessToken> {
        let mut token = None;
        self.mutate(|state| {
            if generation < state.generation {
                return false;
            }
            if generation > state.generation {
                state.generation = generation;
                state.engine_ready = false;
            }
            state.operation = state.operation.saturating_add(1);
            state.phase = ReadinessPhase::Refreshing;
            state.outcome = None;
            state.observed_records = 0;
            state.can_send = false;
            state.bounded_error = None;
            token = Some(ReadinessToken {
                generation: state.generation,
                operation: state.operation,
            });
            true
        });
        token
    }

    /// Invalidate the current generation before its transport drains. The
    /// operation advances, so an in-flight result for the old operation is stale.
    pub fn begin_recovery(&self) {
        self.mutate(|state| {
            state.operation = state.operation.saturating_add(1);
            state.phase = ReadinessPhase::Recovering;
            state.outcome = None;
            state.observed_records = 0;
            state.can_send = false;
            state.engine_ready = false;
            state.bounded_error = None;
            true
        });
    }

    /// Mark the engine for `token` as started. Metadata readiness stays separate.
    pub fn mark_engine_ready(&self, token: ReadinessToken) {
        self.mutate(|state| {
            if state.generation != token.generation || state.operation != token.operation {
                return false;
            }
            if state.engine_ready {
                return false;
            }
            state.engine_ready = true;
            if state.phase == ReadinessPhase::Ready
                && state.outcome == Some(ReadinessOutcome::Complete)
            {
                state.can_send = true;
            }
            true
        });
    }

    /// Record an engine session that is active for the current generation. This
    /// is independent of whether the required metadata refresh has completed.
    pub fn mark_engine_ready_current(&self) {
        self.mutate(|state| {
            if state.engine_ready {
                return false;
            }
            state.engine_ready = true;
            if state.phase == ReadinessPhase::Ready
                && state.outcome == Some(ReadinessOutcome::Complete)
            {
                state.can_send = true;
            }
            true
        });
    }

    /// Apply a terminal result. Returns whether the token was current.
    pub fn finish(&self, token: ReadinessToken, finish: Finish) -> bool {
        self.mutate(|state| {
            if state.generation != token.generation || state.operation != token.operation {
                return false;
            }
            match finish {
                Finish::Complete(count) => {
                    state.phase = ReadinessPhase::Ready;
                    state.outcome = Some(ReadinessOutcome::Complete);
                    state.observed_records = count;
                    // Engine readiness is reported independently by the session
                    // event. Metadata completion alone cannot open the gate.
                    state.can_send = state.engine_ready;
                    state.bounded_error = None;
                }
                Finish::Limited(count) => {
                    state.phase = ReadinessPhase::Limited;
                    state.outcome = Some(ReadinessOutcome::Limited);
                    state.observed_records = count;
                    state.can_send = false;
                    state.bounded_error = Some(ReadinessError::Limited);
                }
                Finish::Failed => {
                    state.phase = ReadinessPhase::Failed;
                    state.outcome = Some(ReadinessOutcome::Failed);
                    state.observed_records = 0;
                    state.can_send = false;
                    state.bounded_error = Some(ReadinessError::Failed);
                }
                Finish::Unavailable => {
                    state.phase = ReadinessPhase::Failed;
                    state.outcome = Some(ReadinessOutcome::Unavailable);
                    state.observed_records = 0;
                    state.can_send = false;
                    state.bounded_error = Some(ReadinessError::Unavailable);
                }
                Finish::Timeout => {
                    state.phase = ReadinessPhase::Failed;
                    state.outcome = Some(ReadinessOutcome::Failed);
                    state.observed_records = 0;
                    state.can_send = false;
                    state.bounded_error = Some(ReadinessError::Timeout);
                }
            }
            true
        })
        .is_some()
    }

    /// A lost session or channel closes the gate immediately and invalidates any
    /// in-flight result. It never queues or retries a send.
    pub fn session_lost(&self) {
        self.mutate(|state| {
            if !state.can_send
                && !state.engine_ready
                && state.bounded_error == Some(ReadinessError::Disconnected)
            {
                return false;
            }
            state.operation = state.operation.saturating_add(1);
            state.phase = ReadinessPhase::Failed;
            state.outcome = None;
            state.can_send = false;
            state.engine_ready = false;
            state.bounded_error = Some(ReadinessError::Disconnected);
            true
        });
    }

    /// Reserve a same-generation reconnect refresh. Only a disconnected session
    /// with no refresh already in progress qualifies; this coalesces duplicate
    /// Active events into one operation.
    pub fn begin_reconnect(&self) -> bool {
        self.mutate(|state| {
            if state.bounded_error != Some(ReadinessError::Disconnected)
                || state.phase == ReadinessPhase::Refreshing
            {
                return false;
            }
            state.operation = state.operation.saturating_add(1);
            state.phase = ReadinessPhase::Refreshing;
            state.outcome = None;
            state.can_send = false;
            state.bounded_error = None;
            true
        })
        .is_some()
    }

    /// Record a replacement that never became usable. No token is available:
    /// recovery itself owns this terminal state.
    pub fn recovery_failed(&self) {
        self.mutate(|state| {
            // Invalidate any in-flight startup result for the failed generation.
            state.operation = state.operation.saturating_add(1);
            state.phase = ReadinessPhase::Failed;
            state.outcome = Some(ReadinessOutcome::Unavailable);
            state.observed_records = 0;
            state.can_send = false;
            state.engine_ready = false;
            state.bounded_error = Some(ReadinessError::Unavailable);
            true
        });
    }

    /// Record shutdown without opening the gate.
    pub fn stopped(&self) {
        self.mutate(|state| {
            if state.phase == ReadinessPhase::Stopped && !state.can_send && !state.engine_ready {
                return false;
            }
            state.phase = ReadinessPhase::Stopped;
            state.outcome = None;
            state.can_send = false;
            state.engine_ready = false;
            true
        });
    }

    fn mutate(
        &self,
        change: impl FnOnce(&mut ReadinessSnapshot) -> bool,
    ) -> Option<ReadinessSnapshot> {
        let mut state = self.state.lock().unwrap();
        if !change(&mut state) {
            return None;
        }
        state.revision = state.revision.saturating_add(1);
        let snapshot = state.clone();
        // Publish while the state lock is held so two concurrent changes cannot
        // reach subscribers out of revision order. The hub has no callback into
        // readiness, so this is not an async or re-entrant lock path.
        self.hub.publish(Event {
            event: EventKind::Readiness {
                snapshot: snapshot.clone(),
            },
        });
        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readiness() -> Readiness {
        Readiness::new_starting(SubscriptionHub::new(8))
    }

    fn started(readiness: &Readiness, generation: u64) -> ReadinessToken {
        let token = readiness.begin(generation).expect("generation is current");
        readiness.mark_engine_ready(token);
        token
    }

    #[test]
    fn empty_complete_opens_the_gate() {
        let readiness = readiness();
        let token = started(&readiness, 1);
        assert!(readiness.finish(token, Finish::Complete(0)));
        let snapshot = readiness.snapshot();
        assert_eq!(snapshot.phase, ReadinessPhase::Ready);
        assert_eq!(snapshot.outcome, Some(ReadinessOutcome::Complete));
        assert_eq!(snapshot.observed_records, 0);
        assert!(snapshot.can_send);
        assert!(snapshot.engine_ready);
        assert_eq!(snapshot.bounded_error, None);
    }

    #[test]
    fn timeout_keeps_the_gate_closed_with_a_bounded_reason() {
        let hub = SubscriptionHub::new(4);
        let readiness = Readiness::new_ready(1, hub);
        let token = readiness.begin(1).unwrap();
        readiness.mark_engine_ready(token);
        assert!(readiness.finish(token, Finish::Timeout));
        let snapshot = readiness.snapshot();
        assert!(!snapshot.can_send);
        assert_eq!(snapshot.phase, ReadinessPhase::Failed);
        assert_eq!(snapshot.outcome, Some(ReadinessOutcome::Failed));
        assert_eq!(snapshot.bounded_error, Some(ReadinessError::Timeout));
    }

    #[test]
    fn reconnect_refresh_only_starts_once_until_it_finishes() {
        let hub = SubscriptionHub::new(4);
        let readiness = Readiness::new_ready(1, hub);
        readiness.session_lost();
        assert!(readiness.begin_reconnect());
        assert!(
            !readiness.begin_reconnect(),
            "duplicate Active events must coalesce into one reconnect refresh"
        );
    }

    #[test]
    fn metadata_completion_does_not_imply_engine_readiness() {
        let readiness = readiness();
        let token = readiness.begin(1).expect("current generation");
        assert!(readiness.finish(token, Finish::Complete(0)));
        let snapshot = readiness.snapshot();
        assert_eq!(snapshot.phase, ReadinessPhase::Ready);
        assert!(!snapshot.engine_ready);
        assert!(!snapshot.can_send);
        readiness.mark_engine_ready_current();
        let ready = readiness.snapshot();
        assert!(ready.engine_ready);
        assert!(ready.can_send);
    }

    #[test]
    fn limited_failed_and_unavailable_stay_closed() {
        for (finish, phase, outcome, error) in [
            (
                Finish::Limited(9),
                ReadinessPhase::Limited,
                ReadinessOutcome::Limited,
                ReadinessError::Limited,
            ),
            (
                Finish::Failed,
                ReadinessPhase::Failed,
                ReadinessOutcome::Failed,
                ReadinessError::Failed,
            ),
            (
                Finish::Unavailable,
                ReadinessPhase::Failed,
                ReadinessOutcome::Unavailable,
                ReadinessError::Unavailable,
            ),
        ] {
            let readiness = readiness();
            let token = started(&readiness, 1);
            assert!(readiness.finish(token, finish));
            let snapshot = readiness.snapshot();
            assert_eq!(snapshot.phase, phase);
            assert_eq!(snapshot.outcome, Some(outcome));
            assert!(!snapshot.can_send);
            assert_eq!(snapshot.bounded_error, Some(error));
        }
    }

    #[test]
    fn stale_generation_and_operation_are_ignored() {
        let readiness = readiness();
        let first = started(&readiness, 1);
        let second = started(&readiness, 2);
        assert!(!readiness.finish(first, Finish::Complete(0)));
        assert!(!readiness.can_send());
        assert_eq!(readiness.snapshot().generation, 2);

        let third = readiness
            .begin(2)
            .expect("same generation is a new operation");
        assert!(!readiness.finish(second, Finish::Complete(0)));
        assert!(!readiness.can_send());
        assert!(readiness.finish(third, Finish::Complete(0)));
        assert!(readiness.can_send());
    }

    #[test]
    fn recovery_invalidates_before_the_new_generation_starts() {
        let readiness = readiness();
        let old = started(&readiness, 1);
        readiness.begin_recovery();
        assert!(!readiness.finish(old, Finish::Complete(0)));
        let recovering = readiness.snapshot();
        assert_eq!(recovering.phase, ReadinessPhase::Recovering);
        assert!(!recovering.can_send);
        assert!(!recovering.engine_ready);

        let new = started(&readiness, 2);
        assert!(readiness.finish(new, Finish::Complete(0)));
        let ready = readiness.snapshot();
        assert_eq!(ready.generation, 2);
        assert!(ready.can_send);
    }

    #[test]
    fn failed_replacement_invalidates_its_startup_result() {
        let readiness = readiness();
        let failed = started(&readiness, 1);
        readiness.recovery_failed();
        assert!(!readiness.can_send());
        assert!(!readiness.finish(failed, Finish::Complete(0)));
        let snapshot = readiness.snapshot();
        assert_eq!(snapshot.phase, ReadinessPhase::Failed);
        assert_eq!(snapshot.outcome, Some(ReadinessOutcome::Unavailable));
    }

    #[test]
    fn session_loss_closes_the_gate_and_invalidates_the_result() {
        let readiness = readiness();
        let token = started(&readiness, 1);
        assert!(readiness.finish(token, Finish::Complete(0)));
        readiness.session_lost();
        let snapshot = readiness.snapshot();
        assert!(!snapshot.can_send);
        assert!(!snapshot.engine_ready);
        assert_eq!(snapshot.bounded_error, Some(ReadinessError::Disconnected));
        assert!(!readiness.finish(token, Finish::Complete(0)));
    }

    #[test]
    fn revisions_only_increase() {
        let readiness = readiness();
        let token = started(&readiness, 1);
        let before = readiness.snapshot().revision;
        assert!(readiness.finish(token, Finish::Complete(0)));
        assert!(readiness.snapshot().revision > before);
    }
}
