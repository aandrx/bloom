//! Client-side startup checklist.
//!
//! The daemon owns authoritative metadata readiness. The client independently
//! waits for its own post-refresh snapshot and selected draft restoration before
//! offering Open Bloom. This module is pure state so the gate can be tested
//! without a terminal, daemon, or timing sleeps.

use crate::daemon::ConnectionState;
use bloom_ipc::{ReadinessError, ReadinessOutcome, ReadinessPhase, ReadinessSnapshot};

pub const CHECK_LABELS: [&str; 5] = [
    "Daemon connected",
    "Engine ready",
    "Metadata refresh",
    "Persistence acknowledged",
    "Local view and draft",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupFailure {
    Disconnected,
    Unavailable,
    Timeout,
    Limited,
    Failed,
    Protocol,
    Internal,
    Client,
}

impl StartupFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::Disconnected => "The daemon connection was lost. Retry to reconnect.",
            Self::Unavailable => "The daemon has no usable engine connection.",
            Self::Timeout => "The initial metadata refresh timed out.",
            Self::Limited => "The initial metadata refresh was limited; some chats may be missing.",
            Self::Failed => "The initial metadata refresh failed.",
            Self::Protocol => "The daemon returned an unreadable readiness state.",
            Self::Internal => "Bloom could not prepare the local view.",
            Self::Client => "The local view or draft could not be prepared.",
        }
    }
}

/// One client startup session. `opened` is only set after an explicit Open Bloom
/// confirmation; it never follows automatically from a readiness packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupState {
    readiness: ReadinessSnapshot,
    connected: bool,
    snapshot_ready: bool,
    draft_ready: bool,
    opened: bool,
    failure: Option<StartupFailure>,
    retrying: bool,
}

impl Default for StartupState {
    /// Existing unit tests and non-startup surfaces use an already-open app.
    /// Production wiring replaces this with [`StartupState::loading`].
    fn default() -> Self {
        Self::ready_for_tests()
    }
}

impl StartupState {
    fn empty_readiness() -> ReadinessSnapshot {
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
        }
    }

    pub fn loading() -> Self {
        Self {
            readiness: Self::empty_readiness(),
            connected: false,
            snapshot_ready: false,
            draft_ready: false,
            opened: false,
            failure: None,
            retrying: false,
        }
    }

    pub fn ready_for_tests() -> Self {
        Self {
            readiness: ReadinessSnapshot {
                generation: 1,
                operation: 1,
                revision: 1,
                phase: ReadinessPhase::Ready,
                outcome: Some(ReadinessOutcome::Complete),
                observed_records: 0,
                can_send: true,
                engine_ready: true,
                bounded_error: None,
            },
            connected: true,
            snapshot_ready: true,
            draft_ready: true,
            opened: true,
            failure: None,
            retrying: false,
        }
    }

    pub fn opened(&self) -> bool {
        self.opened
    }

    pub fn overlay_visible(&self) -> bool {
        !self.opened
    }

    pub fn retrying(&self) -> bool {
        self.retrying
    }

    pub fn failure(&self) -> Option<StartupFailure> {
        self.failure
    }

    pub fn observed_records(&self) -> u32 {
        self.readiness.observed_records
    }

    pub fn checks(&self) -> [bool; 5] {
        let complete = self.metadata_complete();
        [
            self.connected,
            self.readiness.engine_ready,
            complete,
            complete,
            self.snapshot_ready && self.draft_ready,
        ]
    }

    pub fn completed_checks(&self) -> usize {
        self.checks().into_iter().filter(|done| *done).count()
    }

    pub fn can_confirm(&self) -> bool {
        self.failure.is_none()
            && self.connected
            && self.readiness.can_send
            && self.readiness.engine_ready
            && self.metadata_complete()
            && self.snapshot_ready
            && self.draft_ready
    }

    pub fn can_send(&self) -> bool {
        self.opened && self.can_confirm()
    }

    pub fn is_loading(&self) -> bool {
        self.failure.is_none()
            && matches!(
                self.readiness.phase,
                ReadinessPhase::Starting | ReadinessPhase::Refreshing | ReadinessPhase::Recovering
            )
    }

    pub fn confirm_open(&mut self) -> bool {
        if self.opened || !self.can_confirm() {
            return false;
        }
        self.opened = true;
        true
    }

    pub fn observe_connection(&mut self, state: ConnectionState) {
        let connected = state == ConnectionState::Connected;
        match state {
            ConnectionState::Connected => {
                self.connected = true;
                if self.failure == Some(StartupFailure::Disconnected) {
                    self.failure = None;
                }
            }
            ConnectionState::Connecting => {
                self.connected = false;
                // Connecting is a normal in-progress state, not a terminal error.
                // Do not erase a snapshot that may have arrived through another
                // channel just before this status update.
                if self.failure == Some(StartupFailure::Disconnected) {
                    self.failure = None;
                }
            }
            ConnectionState::Disconnected => {
                self.connected = false;
                self.opened = false;
                self.snapshot_ready = false;
                self.draft_ready = false;
                // A restarted daemon starts a new revision sequence. Keep the
                // reconnect gate closed until its fresh readiness packet arrives.
                self.readiness = Self::empty_readiness();
                self.failure = Some(StartupFailure::Disconnected);
            }
        }
        self.connected = connected;
    }

    pub fn observe_readiness(&mut self, snapshot: ReadinessSnapshot) {
        if snapshot.revision < self.readiness.revision {
            return;
        }
        self.readiness = snapshot;
        if matches!(
            self.readiness.phase,
            ReadinessPhase::Starting | ReadinessPhase::Refreshing | ReadinessPhase::Recovering
        ) {
            self.failure = None;
        } else if let Some(failure) = self.failure_from_readiness() {
            self.failure = Some(failure);
            self.opened = false;
        } else if self.metadata_complete() {
            self.failure = None;
            self.retrying = false;
        }
        // A manual sync or routine readiness transition must not eject an
        // already-open session. Admission is closed separately by can_send.
    }

    pub fn observe_snapshot(&mut self) {
        self.snapshot_ready = true;
    }

    pub fn observe_draft_ready(&mut self) {
        self.draft_ready = true;
    }

    /// A newly selected conversation has its own durable draft load. Only the
    /// initial startup gate needs to wait for that load again.
    pub fn reset_draft_readiness(&mut self) {
        if !self.opened {
            self.draft_ready = false;
        }
    }

    pub fn begin_retry(&mut self) {
        self.failure = None;
        self.retrying = true;
        self.opened = false;
        self.snapshot_ready = false;
        self.draft_ready = false;
    }

    pub fn fail_local(&mut self, failure: StartupFailure) {
        self.failure = Some(failure);
        self.opened = false;
    }

    fn metadata_complete(&self) -> bool {
        self.readiness.phase == ReadinessPhase::Ready
            && self.readiness.outcome == Some(ReadinessOutcome::Complete)
    }

    fn failure_from_readiness(&self) -> Option<StartupFailure> {
        if self.readiness.phase == ReadinessPhase::Limited
            || self.readiness.outcome == Some(ReadinessOutcome::Limited)
        {
            return Some(StartupFailure::Limited);
        }
        if self.readiness.phase != ReadinessPhase::Failed {
            return None;
        }
        Some(match self.readiness.bounded_error {
            Some(ReadinessError::Timeout) => StartupFailure::Timeout,
            Some(ReadinessError::Unavailable) => StartupFailure::Unavailable,
            Some(ReadinessError::Limited) => StartupFailure::Limited,
            Some(ReadinessError::Disconnected) => StartupFailure::Disconnected,
            Some(ReadinessError::Failed) | None => StartupFailure::Failed,
            Some(ReadinessError::Protocol) => StartupFailure::Protocol,
            Some(ReadinessError::Internal) => StartupFailure::Internal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(phase: ReadinessPhase, outcome: Option<ReadinessOutcome>) -> ReadinessSnapshot {
        ReadinessSnapshot {
            generation: 1,
            operation: 1,
            revision: 1,
            phase,
            outcome,
            observed_records: 3,
            can_send: phase == ReadinessPhase::Ready && outcome == Some(ReadinessOutcome::Complete),
            engine_ready: true,
            bounded_error: None,
        }
    }

    #[test]
    fn all_five_checks_are_required_before_confirmation() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        startup.observe_snapshot();
        assert!(!startup.can_confirm());
        assert_eq!(startup.completed_checks(), 4);
        startup.observe_draft_ready();
        assert!(startup.can_confirm());
        assert_eq!(startup.completed_checks(), 5);
        assert!(!startup.confirm_open() || startup.opened());
        assert!(startup.can_send());
    }

    #[test]
    fn empty_complete_is_valid_and_zero_records_is_reported() {
        let mut startup = StartupState::ready_for_tests();
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        assert_eq!(startup.observed_records(), 3);
        assert_eq!(startup.completed_checks(), 5);
        assert!(startup.can_confirm());
    }

    #[test]
    fn limited_and_timeout_fail_closed() {
        let mut limited = StartupState::loading();
        limited.observe_connection(ConnectionState::Connected);
        let mut state = snapshot(ReadinessPhase::Limited, Some(ReadinessOutcome::Limited));
        state.can_send = false;
        limited.observe_readiness(state);
        assert_eq!(limited.failure(), Some(StartupFailure::Limited));
        assert!(!limited.can_confirm());

        let mut timeout = StartupState::loading();
        timeout.observe_connection(ConnectionState::Connected);
        let mut state = snapshot(ReadinessPhase::Failed, Some(ReadinessOutcome::Failed));
        state.bounded_error = Some(ReadinessError::Timeout);
        timeout.observe_readiness(state);
        assert_eq!(timeout.failure(), Some(StartupFailure::Timeout));
        assert!(!timeout.can_confirm());
    }

    #[test]
    fn stale_revision_cannot_reopen_or_replace_newer_state() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        let mut newer = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        newer.revision = 9;
        startup.observe_readiness(newer);
        let mut older = snapshot(ReadinessPhase::Failed, Some(ReadinessOutcome::Failed));
        older.revision = 8;
        startup.observe_readiness(older);
        assert_eq!(startup.completed_checks(), 4);
        assert_eq!(startup.failure(), None);
    }

    #[test]
    fn disconnection_closes_admission_without_dropping_local_state() {
        let mut startup = StartupState::ready_for_tests();
        assert!(startup.can_send());
        startup.observe_connection(ConnectionState::Disconnected);
        assert!(!startup.can_send());
        assert_eq!(startup.failure(), Some(StartupFailure::Disconnected));
        assert_eq!(startup.completed_checks(), 0);
    }

    #[test]
    fn reconnect_accepts_a_lower_revision_from_a_restarted_daemon() {
        let mut startup = StartupState::ready_for_tests();
        let mut later = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        later.revision = 9;
        startup.observe_readiness(later);
        startup.observe_connection(ConnectionState::Disconnected);
        assert_eq!(startup.completed_checks(), 0);
        assert_eq!(startup.failure(), Some(StartupFailure::Disconnected));

        startup.observe_connection(ConnectionState::Connected);
        let mut restarted = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        restarted.revision = 1;
        startup.observe_readiness(restarted);
        startup.observe_snapshot();
        startup.observe_draft_ready();
        assert!(
            startup.can_confirm(),
            "new daemon revision must be accepted"
        );
    }

    #[test]
    fn manual_sync_closes_admission_without_reopening_the_checklist() {
        let mut startup = StartupState::ready_for_tests();
        let mut refreshing = snapshot(ReadinessPhase::Refreshing, None);
        refreshing.revision = 2;
        startup.observe_readiness(refreshing);
        assert!(startup.opened());
        assert!(!startup.can_send());
        let mut complete = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        complete.revision = 3;
        startup.observe_readiness(complete);
        assert!(startup.opened());
        assert!(startup.can_send());
    }

    #[test]
    fn connecting_is_loading_not_a_terminal_failure() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connecting);
        assert_eq!(startup.failure(), None);
        assert!(startup.is_loading());
    }

    #[test]
    fn retry_resets_local_completion_but_keeps_the_session_data() {
        let mut startup = StartupState::ready_for_tests();
        startup.begin_retry();
        assert!(startup.retrying());
        assert_eq!(startup.completed_checks(), 4);
        assert!(!startup.can_confirm());
        startup.observe_draft_ready();
        startup.observe_snapshot();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        assert!(startup.can_confirm());
    }
}
