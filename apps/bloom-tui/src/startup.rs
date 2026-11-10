//! Client-side startup checklist.
//!
//! The daemon owns authoritative metadata readiness. The client independently
//! waits for its own post-refresh snapshot and selected draft restoration before
//! offering Open Bloom. This module is pure state so the gate can be tested
//! without a terminal, daemon, or timing sleeps.

use crate::daemon::ConnectionState;
use bloom_ipc::{
    client::ClientError, DraftKey, ReadinessError, ReadinessOutcome, ReadinessPhase,
    ReadinessSnapshot,
};

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

/// The readiness packet a bootstrap read is allowed to complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadinessIdentity {
    pub generation: u64,
    pub operation: u64,
    pub revision: u64,
}

impl ReadinessIdentity {
    fn from_snapshot(snapshot: &ReadinessSnapshot) -> Self {
        Self {
            generation: snapshot.generation,
            operation: snapshot.operation,
            revision: snapshot.revision,
        }
    }
}

/// A client-local ticket for the one post-readiness startup read. It never travels
/// over IPC: it only ties a local snapshot reply to the bootstrap attempt and
/// readiness identity that authorized it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapTicket {
    pub attempt: u64,
    pub readiness: ReadinessIdentity,
}

/// One client startup session. `opened` is only set after an explicit Open Bloom
/// confirmation; it never follows automatically from a readiness packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupState {
    readiness: ReadinessSnapshot,
    connected: bool,
    snapshot_ready: bool,
    draft_ready: bool,
    draft_owner: Option<DraftKey>,
    opened: bool,
    failure: Option<StartupFailure>,
    retrying: bool,
    /// Monotonic local attempt. Retry, disconnect, and a changed readiness
    /// identity all advance it, so an older read can never complete a newer
    /// bootstrap.
    bootstrap_attempt: u64,
    /// The readiness identity the current attempt is allowed to observe.
    bootstrap_readiness: Option<ReadinessIdentity>,
    /// A ticketed read that main has not taken yet.
    bootstrap_pending: Option<BootstrapTicket>,
    /// A ticketed read that main has launched and whose reply is outstanding.
    bootstrap_in_flight: Option<BootstrapTicket>,
    /// A rejected ticketed read gets at most one replacement.
    bootstrap_replacement_used: bool,
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
            draft_owner: None,
            opened: false,
            failure: None,
            retrying: false,
            bootstrap_attempt: 0,
            bootstrap_readiness: None,
            bootstrap_pending: None,
            bootstrap_in_flight: None,
            bootstrap_replacement_used: false,
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
            draft_owner: None,
            opened: true,
            failure: None,
            retrying: false,
            bootstrap_attempt: 1,
            bootstrap_readiness: Some(ReadinessIdentity {
                generation: 1,
                operation: 1,
                revision: 1,
            }),
            bootstrap_pending: None,
            bootstrap_in_flight: None,
            bootstrap_replacement_used: false,
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

    pub fn snapshot_ready(&self) -> bool {
        self.snapshot_ready
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
                self.draft_owner = None;
                self.invalidate_bootstrap();
                self.bootstrap_readiness = None;
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
        let identity = ReadinessIdentity::from_snapshot(&snapshot);
        if self.bootstrap_readiness != Some(identity) {
            // A new readiness identity invalidates every older ticket. An open
            // session keeps its already-accepted local view so a routine manual
            // sync does not eject it; a closed checklist must read again.
            self.invalidate_bootstrap();
            self.bootstrap_readiness = Some(identity);
            if !self.opened {
                self.snapshot_ready = false;
            }
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
        self.schedule_bootstrap();
        // A manual sync or routine readiness transition must not eject an
        // already-open session. Admission is closed separately by can_send.
    }

    /// Test-only compatibility for surfaces that predate the bootstrap ticket.
    /// Production acknowledges startup only through [`Self::accept_bootstrap`].
    #[doc(hidden)]
    pub fn observe_snapshot(&mut self) {
        self.snapshot_ready = true;
        self.bootstrap_pending = None;
        self.bootstrap_in_flight = None;
    }

    pub fn observe_draft_ready(&mut self, owner: Option<DraftKey>) {
        self.draft_ready = true;
        self.draft_owner = owner;
    }

    /// Clear a draft completion that belonged to a different owner. An already
    /// loaded draft for the current owner still counts.
    pub fn observe_draft_owner(&mut self, owner: Option<&DraftKey>) {
        if self.draft_ready && self.draft_owner.as_ref() != owner {
            self.draft_ready = false;
            self.draft_owner = None;
        }
    }

    /// A newly selected conversation has its own durable draft load. Only the
    /// initial startup gate needs to wait for that load again.
    pub fn reset_draft_readiness(&mut self) {
        if !self.opened {
            self.draft_ready = false;
            self.draft_owner = None;
        }
    }

    pub fn begin_retry(&mut self) {
        self.invalidate_bootstrap();
        self.bootstrap_readiness = None;
        self.failure = None;
        self.retrying = true;
        self.opened = false;
        self.snapshot_ready = false;
        self.draft_ready = false;
        self.draft_owner = None;
    }

    pub fn fail_local(&mut self, failure: StartupFailure) {
        self.invalidate_bootstrap();
        self.failure = Some(failure);
        // A failure is terminal for this attempt. Leaving the retry flag set
        // would keep the overlay spinning instead of offering Retry.
        self.retrying = false;
        self.opened = false;
    }

    /// Apply the readiness fetch a startup retry performed. Success behaves
    /// exactly like an observed readiness packet. A failed fetch is a bounded
    /// local failure: without a readiness packet no bootstrap read is ever
    /// scheduled, so ignoring the error would wedge the checklist. The failure
    /// also drops any pending or in-flight ticket from the retry.
    pub fn observe_retry_readiness(
        &mut self,
        result: Result<ReadinessSnapshot, ClientError>,
    ) -> bool {
        match result {
            Ok(snapshot) => {
                self.observe_readiness(snapshot);
                true
            }
            Err(_) => {
                self.fail_local(StartupFailure::Client);
                false
            }
        }
    }

    /// Take the scheduled read, if any. Main owns launching it.
    pub fn take_pending_bootstrap(&mut self) -> Option<BootstrapTicket> {
        let ticket = self.bootstrap_pending.take()?;
        self.bootstrap_in_flight = Some(ticket);
        Some(ticket)
    }

    /// A ticketed read passed the revision gate and was applied. It may complete
    /// startup only when it belongs to the current attempt and readiness identity.
    pub fn accept_bootstrap(&mut self, ticket: BootstrapTicket) -> bool {
        if self.snapshot_ready
            || self.bootstrap_attempt != ticket.attempt
            || self.bootstrap_readiness != Some(ticket.readiness)
            || self.bootstrap_in_flight != Some(ticket)
        {
            return false;
        }
        self.bootstrap_in_flight = None;
        self.snapshot_ready = true;
        true
    }

    /// A ticketed read was rejected by the revision gate. Keep startup incomplete
    /// and schedule at most one replacement for the same attempt; a rejected
    /// replacement becomes a visible bounded failure instead of a silent stall.
    pub fn reject_bootstrap(&mut self, ticket: BootstrapTicket) -> bool {
        if self.bootstrap_in_flight != Some(ticket) {
            return false;
        }
        self.bootstrap_in_flight = None;
        if self.snapshot_ready {
            return false;
        }
        if self.bootstrap_replacement_used {
            // The replacement was rejected too. Stopping silently would leave
            // snapshot_ready false with no failure, no ticket, and no Retry, so
            // surface a bounded failure that the overlay can act on.
            self.fail_local(StartupFailure::Client);
            return false;
        }
        self.bootstrap_replacement_used = true;
        self.bootstrap_pending = Some(ticket);
        true
    }

    /// A current read failure is a bounded local startup failure. A stale reply
    /// from an older attempt is inert.
    pub fn fail_bootstrap(&mut self, ticket: BootstrapTicket) -> bool {
        if self.bootstrap_in_flight != Some(ticket) {
            return false;
        }
        self.bootstrap_in_flight = None;
        self.fail_local(StartupFailure::Client);
        true
    }

    fn invalidate_bootstrap(&mut self) {
        self.bootstrap_attempt = self.bootstrap_attempt.wrapping_add(1);
        self.bootstrap_pending = None;
        self.bootstrap_in_flight = None;
        self.bootstrap_replacement_used = false;
    }

    fn schedule_bootstrap(&mut self) {
        if self.opened
            || self.snapshot_ready
            || self.failure.is_some()
            || self.bootstrap_replacement_used
            || self.bootstrap_pending.is_some()
            || self.bootstrap_in_flight.is_some()
            || !self.metadata_complete()
        {
            return;
        }
        let Some(readiness) = self.bootstrap_readiness else {
            return;
        };
        self.bootstrap_pending = Some(BootstrapTicket {
            attempt: self.bootstrap_attempt,
            readiness,
        });
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

    fn complete_bootstrap(startup: &mut StartupState) {
        let ticket = startup
            .take_pending_bootstrap()
            .expect("a bootstrap read is scheduled");
        assert!(startup.accept_bootstrap(ticket));
    }

    #[test]
    fn all_five_checks_are_required_before_confirmation() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        assert!(!startup.can_confirm());
        assert_eq!(startup.completed_checks(), 4);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
        assert_eq!(startup.completed_checks(), 5);
        assert!(!startup.confirm_open() || startup.opened());
        assert!(startup.can_send());
    }

    #[test]
    fn untracked_snapshot_cannot_complete_startup() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(ReadinessPhase::Refreshing, None));
        // A snapshot read that began before the refresh finished arrives first.
        startup.observe_snapshot();
        let mut ready = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        ready.revision = 2;
        startup.observe_readiness(ready);
        assert!(!startup.snapshot_ready());
        assert!(!startup.can_confirm());
        assert!(!startup.checks()[4]);
    }

    #[test]
    fn ticketed_snapshot_matching_readiness_completes_startup() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let ticket = startup
            .take_pending_bootstrap()
            .expect("ready readiness schedules one read");
        assert!(startup.accept_bootstrap(ticket));
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }

    #[test]
    fn duplicate_readiness_does_not_schedule_a_second_read() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        let ready = snapshot(ReadinessPhase::Ready, Some(ReadinessOutcome::Complete));
        startup.observe_readiness(ready.clone());
        let ticket = startup.take_pending_bootstrap().expect("first read");
        startup.observe_readiness(ready.clone());
        assert!(
            startup.take_pending_bootstrap().is_none(),
            "a replayed Ready must not launch another read"
        );
        assert!(startup.accept_bootstrap(ticket));
        startup.observe_readiness(ready);
        assert!(startup.take_pending_bootstrap().is_none());
    }

    #[test]
    fn rejected_ticket_schedules_at_most_one_replacement() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let ticket = startup.take_pending_bootstrap().expect("first read");
        assert!(startup.reject_bootstrap(ticket));
        assert!(!startup.snapshot_ready());
        let replacement = startup.take_pending_bootstrap().expect("one replacement");
        assert!(!startup.reject_bootstrap(replacement));
        assert!(startup.take_pending_bootstrap().is_none());
        assert!(!startup.can_confirm());
    }

    #[test]
    fn rejected_replacement_is_a_visible_bounded_failure_with_retry() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let ticket = startup.take_pending_bootstrap().expect("first read");
        assert!(startup.reject_bootstrap(ticket));
        let replacement = startup.take_pending_bootstrap().expect("one replacement");
        assert!(!startup.reject_bootstrap(replacement));

        // Exhaustion must be visible instead of a silent wedge.
        assert_eq!(startup.failure(), Some(StartupFailure::Client));
        assert!(!startup.snapshot_ready());
        assert!(!startup.retrying());
        assert!(startup.bootstrap_pending.is_none());
        assert!(startup.bootstrap_in_flight.is_none());
        assert!(!startup.is_loading());

        // The offered Retry can actually schedule a fresh read.
        startup.begin_retry();
        assert_eq!(startup.failure(), None);
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }

    #[test]
    fn failed_retry_readiness_surfaces_a_bounded_failure_and_leaves_no_ticket() {
        let mut startup = StartupState::ready_for_tests();
        startup.begin_retry();
        assert!(startup.retrying());

        assert!(!startup.observe_retry_readiness(Err(ClientError::Timeout)));
        assert_eq!(startup.failure(), Some(StartupFailure::Client));
        assert!(!startup.retrying(), "a failure must stop the spinner");
        assert!(!startup.is_loading());
        assert!(startup.bootstrap_pending.is_none());
        assert!(startup.bootstrap_in_flight.is_none());
        assert!(!startup.can_confirm());

        // Retry after the failed fetch still reaches confirmation.
        startup.begin_retry();
        assert_eq!(startup.failure(), None);
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }

    #[test]
    fn successful_retry_readiness_schedules_the_bootstrap_read() {
        let mut startup = StartupState::ready_for_tests();
        startup.begin_retry();
        assert!(startup.observe_retry_readiness(Ok(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete)
        ))));
        assert_eq!(startup.failure(), None);
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }

    #[test]
    fn retry_invalidates_an_outstanding_ticket() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let old = startup.take_pending_bootstrap().expect("first read");
        startup.begin_retry();
        assert!(!startup.accept_bootstrap(old));
        assert!(!startup.snapshot_ready());

        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }

    #[test]
    fn recovery_invalidates_an_outstanding_ticket() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let old = startup.take_pending_bootstrap().expect("first read");
        let mut recovering = snapshot(ReadinessPhase::Recovering, None);
        recovering.revision = 2;
        startup.observe_readiness(recovering);
        assert!(!startup.accept_bootstrap(old));
        assert!(!startup.snapshot_ready());
        assert!(!startup.can_confirm());
    }

    #[test]
    fn current_bootstrap_failure_is_local_and_stale_failure_is_inert() {
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let old = startup.take_pending_bootstrap().expect("first read");
        startup.begin_retry();
        assert!(!startup.fail_bootstrap(old));
        assert_eq!(startup.failure(), None);

        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        let current = startup.take_pending_bootstrap().expect("retry read");
        assert!(startup.fail_bootstrap(current));
        assert_eq!(startup.failure(), Some(StartupFailure::Client));
        assert!(!startup.can_confirm());
    }

    #[test]
    fn draft_completion_is_owner_specific() {
        let first = DraftKey::Conversation {
            owner: bloom_domain::ConversationId::new(
                bloom_domain::TransportId::new("test").unwrap(),
                "first",
            )
            .unwrap(),
        };
        let second = DraftKey::Conversation {
            owner: bloom_domain::ConversationId::new(
                bloom_domain::TransportId::new("test").unwrap(),
                "second",
            )
            .unwrap(),
        };
        let mut startup = StartupState::loading();
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(Some(first.clone()));
        assert!(startup.can_confirm());
        startup.observe_draft_owner(Some(&second));
        assert!(!startup.can_confirm(), "an old owner's load cannot count");
        startup.observe_draft_ready(Some(second.clone()));
        assert!(startup.can_confirm());
        startup.observe_draft_owner(Some(&second));
        assert!(startup.can_confirm(), "same owner stays complete");
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
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
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
        startup.observe_connection(ConnectionState::Connected);
        startup.observe_readiness(snapshot(
            ReadinessPhase::Ready,
            Some(ReadinessOutcome::Complete),
        ));
        complete_bootstrap(&mut startup);
        startup.observe_draft_ready(None);
        assert!(startup.can_confirm());
    }
}
