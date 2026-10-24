//! Pure, deterministic retry policy for a future engine restart. It never
//! spawns, sleeps, reads clocks, logs, or mutates global state; callers own the
//! waiting.
//!
//! The policy applies only after a successful first start, so a spawn failure
//! before any success is terminal (today's fatal initial start). It decides only
//! whether and when to start a new generation, never whether to resend, replay,
//! or confirm a message. `GiveUp` is terminal for that sequence and leaves the
//! daemon serving with a typed unavailable state.

use std::time::Duration;

/// Base delay before the first retry.
pub const BASE_DELAY: Duration = Duration::from_millis(300);
/// Upper bound on any single retry delay.
pub const MAX_DELAY: Duration = Duration::from_secs(8);
/// Maximum retries after a successful first start.
pub const MAX_ATTEMPTS: u32 = 6;

/// Why an engine generation stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The child process could not be spawned.
    SpawnFailed,
    /// The child exited, optionally carrying its exit code.
    ChildExited(Option<i32>),
    /// A stream or read failed.
    StreamFailed,
}

/// Why the policy stopped retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiveUpReason {
    /// Spawn failed before any successful start; the initial start stays fatal.
    InitialStartFailed,
    /// The bounded attempt budget is exhausted.
    AttemptsExhausted,
}

/// Bounded exponential backoff with an attempt budget.
#[derive(Debug)]
pub struct RecoveryPolicy {
    started_once: bool,
    attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    max_attempts: u32,
    next_delay: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self::with_limits(BASE_DELAY, MAX_DELAY, MAX_ATTEMPTS)
    }
}

impl RecoveryPolicy {
    /// Build a policy with explicit limits, used by tests that cannot wait for
    /// the production schedule.
    ///
    /// `base` must be non-zero and `max` at least `base`; both are clamped here
    /// rather than rejected so a zero or inverted schedule still terminates.
    pub fn with_limits(base: Duration, max: Duration, attempts: u32) -> Self {
        let base_delay = base.max(Duration::from_millis(1));
        let max_delay = max.max(base_delay);
        Self {
            started_once: false,
            attempts: 0,
            base_delay,
            max_delay,
            max_attempts: attempts,
            next_delay: base_delay,
        }
    }

    /// Record a failure; return the delay before retrying or a terminal reason.
    pub fn record_failure(&mut self, kind: FailureKind) -> Result<Duration, GiveUpReason> {
        if !self.started_once && kind == FailureKind::SpawnFailed {
            return Err(GiveUpReason::InitialStartFailed);
        }
        if self.attempts >= self.max_attempts {
            return Err(GiveUpReason::AttemptsExhausted);
        }
        let delay = self.next_delay.min(self.max_delay);
        self.attempts += 1;
        self.next_delay = delay.saturating_mul(2);
        Ok(delay)
    }

    /// Record a successful start, resetting for a fresh sequence.
    pub fn record_success(&mut self) {
        self.started_once = true;
        self.attempts = 0;
        self.next_delay = self.base_delay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(kind: FailureKind) -> Vec<Duration> {
        let mut policy = RecoveryPolicy::default();
        policy.record_success();
        (0..MAX_ATTEMPTS)
            .map(|_| policy.record_failure(kind).expect("unexpected give-up"))
            .collect()
    }

    #[test]
    fn delay_sequence_is_non_decreasing_and_capped() {
        let seq = schedule(FailureKind::ChildExited(Some(1)));
        assert_eq!(seq.len() as u32, MAX_ATTEMPTS);
        assert_eq!(seq[0], BASE_DELAY);
        assert!(seq.windows(2).all(|w| w[0] <= w[1]));
        assert!(seq.iter().all(|d| *d <= MAX_DELAY));
        // The last step must be clamped rather than naturally equal to the cap.
        assert_eq!(seq[4] * 2, Duration::from_millis(9_600));
        assert_eq!(*seq.last().unwrap(), MAX_DELAY);
        assert!(seq[4] * 2 > MAX_DELAY, "the cap must actually clamp");
    }

    #[test]
    fn attempt_limit_gives_up_exactly_once_then_stays_given_up() {
        let mut policy = RecoveryPolicy::default();
        policy.record_success();
        for _ in 0..MAX_ATTEMPTS {
            assert!(policy.record_failure(FailureKind::StreamFailed).is_ok());
        }
        for _ in 0..3 {
            let step = policy.record_failure(FailureKind::StreamFailed);
            assert_eq!(step, Err(GiveUpReason::AttemptsExhausted));
        }
    }

    #[test]
    fn reset_after_success_starts_fresh_sequence() {
        let mut policy = RecoveryPolicy::default();
        policy.record_success();
        for _ in 0..MAX_ATTEMPTS {
            assert!(policy
                .record_failure(FailureKind::ChildExited(None))
                .is_ok());
        }
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Err(GiveUpReason::AttemptsExhausted)
        );

        policy.record_success();
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Ok(BASE_DELAY)
        );
        // A stale attempt counter would make this give up before the budget.
        for _ in 1..MAX_ATTEMPTS {
            assert!(
                policy
                    .record_failure(FailureKind::ChildExited(None))
                    .is_ok(),
                "success must restore the full attempt budget"
            );
        }
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Err(GiveUpReason::AttemptsExhausted)
        );
    }

    #[test]
    fn failure_kinds_do_not_change_delay_schedule() {
        let expected = schedule(FailureKind::ChildExited(Some(7)));
        let kinds = [
            FailureKind::SpawnFailed,
            FailureKind::ChildExited(None),
            FailureKind::ChildExited(Some(1)),
            FailureKind::StreamFailed,
        ];
        for kind in kinds {
            assert_eq!(schedule(kind), expected);
        }
    }

    #[test]
    fn spawn_failure_before_first_start_gives_up() {
        let mut policy = RecoveryPolicy::default();
        for _ in 0..3 {
            let step = policy.record_failure(FailureKind::SpawnFailed);
            assert_eq!(step, Err(GiveUpReason::InitialStartFailed));
        }
    }

    #[test]
    fn explicit_limits_bound_delay_and_attempts() {
        let mut policy =
            RecoveryPolicy::with_limits(Duration::from_millis(2), Duration::from_millis(5), 2);
        policy.record_success();
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Ok(Duration::from_millis(2))
        );
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Ok(Duration::from_millis(4))
        );
        assert_eq!(
            policy.record_failure(FailureKind::ChildExited(None)),
            Err(GiveUpReason::AttemptsExhausted)
        );
    }

    #[test]
    fn limits_are_clamped_to_a_terminating_schedule() {
        let mut zero = RecoveryPolicy::with_limits(Duration::ZERO, Duration::ZERO, 1);
        zero.record_success();
        assert_eq!(
            zero.record_failure(FailureKind::StreamFailed),
            Ok(Duration::from_millis(1))
        );

        let mut inverted =
            RecoveryPolicy::with_limits(Duration::from_millis(8), Duration::from_millis(2), 1);
        inverted.record_success();
        assert_eq!(
            inverted.record_failure(FailureKind::StreamFailed),
            Ok(Duration::from_millis(8))
        );
    }
}
