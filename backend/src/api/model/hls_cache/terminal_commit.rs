use super::{
    lease::HlsTerminalTailPreparation,
    session_store::{
        HlsCurrentProxySessionAccess, HlsSessionHandle, HlsSessionIncarnation, HlsSessionStore,
    },
    runtime_custom_tail::{
        HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason, HlsRuntimeCustomTailRevision,
    },
    terminal_tail::{
        HlsTerminalCommitMediaGuard, HlsTerminalTailCompatibility, HlsTerminalTailPlan,
    },
    HlsAccessLeaseId, HlsAccessLeaseStore, ProxySessionId,
};
#[cfg(test)]
use super::terminal_tail::HlsTerminalAssetIdentity;
use futures::FutureExt;
use log::{debug, error};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::RwLock;

pub(crate) const HLS_TERMINAL_COMMIT_RETRY_CAPACITY: usize = 256;
const HLS_TERMINAL_COMMIT_MAX_ATTEMPTS: u8 = 64;
const HLS_TERMINAL_COMMIT_INITIAL_BACKOFF_MS: u64 = 1;
const HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS: u64 = 50;
const HLS_TERMINAL_COMMIT_EXCLUSIVE_DEADLINE_GUARD_MS: u64 = 1;
const HLS_TERMINAL_COMMIT_MAX_WORKER_RESTARTS: u8 = 3;
const HLS_TERMINAL_COMMIT_LIVE_CLOCK: u64 = u64::MAX;

pub(super) struct HlsTerminalCommitClock {
    fixed_now_ms: AtomicU64,
}

impl Default for HlsTerminalCommitClock {
    fn default() -> Self { Self { fixed_now_ms: AtomicU64::new(HLS_TERMINAL_COMMIT_LIVE_CLOCK) } }
}

impl HlsTerminalCommitClock {
    pub(super) fn now_ms(&self) -> u64 {
        let fixed_now_ms = self.fixed_now_ms.load(Ordering::Acquire);
        if fixed_now_ms == HLS_TERMINAL_COMMIT_LIVE_CLOCK { current_time_millis() } else { fixed_now_ms }
    }

    #[cfg(test)]
    pub(super) fn set_fixed_now_ms(&self, now_ms: u64) {
        self.fixed_now_ms.store(now_ms, Ordering::Release);
    }

    pub(super) fn initial_attempt_now_ms(&self, supplied_now_ms: u64) -> u64 {
        #[cfg(test)]
        if self.fixed_now_ms.load(Ordering::Acquire) == HLS_TERMINAL_COMMIT_LIVE_CLOCK {
            return supplied_now_ms;
        }
        self.now_ms().max(supplied_now_ms)
    }

    fn advance_fixed_retry_clock(&self, scheduled_at_ms: u64) -> bool {
        let fixed_now_ms = self.fixed_now_ms.load(Ordering::Acquire);
        if fixed_now_ms != HLS_TERMINAL_COMMIT_LIVE_CLOCK {
            self.fixed_now_ms.fetch_max(scheduled_at_ms, Ordering::AcqRel);
            return true;
        }
        false
    }
}

/// Complete bounded backoff schedule plus one millisecond needed to keep the
/// final configured attempt strictly before an exclusive safe deadline.
pub(crate) const fn terminal_commit_retry_schedule_budget_ms() -> u64 {
    let mut attempts_completed = 1;
    let mut budget_ms = 0_u64;
    while attempts_completed < HLS_TERMINAL_COMMIT_MAX_ATTEMPTS {
        budget_ms = budget_ms.saturating_add(terminal_commit_retry_backoff_ms(attempts_completed));
        attempts_completed = attempts_completed.saturating_add(1);
    }
    budget_ms.saturating_add(HLS_TERMINAL_COMMIT_EXCLUSIVE_DEADLINE_GUARD_MS)
}

/// Bounded tail reserved for fail-closed handoff. One complete maximum
/// configured backoff interval plus the exclusive-deadline guard gives an
/// initially contended CAS real scheduling time for autonomous retries.
pub(crate) const fn terminal_commit_retry_handoff_budget_ms() -> u64 {
    HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS.saturating_add(HLS_TERMINAL_COMMIT_EXCLUSIVE_DEADLINE_GUARD_MS)
}

/// Machine-readable result of a generation-bound terminal publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum HlsTerminalCommitOutcome {
    Committed,
    AlreadyCommitted,
    SupersededGeneration,
    LeaseNoLongerEligible,
    RecoveryCommitted,
    CutoverNoLongerRequired,
    BundleNotReady,
    BundleIncompatible,
    SafeCommitDeadlineElapsed,
    RetryCapacityExceeded,
    RetryAttemptsExhausted,
    RetryWorkerUnavailable,
    LockBusy { retry_before_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsTerminalCommitAttempt {
    LockBusy,
    Completed(HlsTerminalCommitOutcome),
}

pub(super) type HlsTerminalCommitAttemptFn = fn(
    &Arc<RwLock<HlsAccessLeaseStore>>,
    &HlsTerminalCommitCommand,
    u64,
    u64,
) -> HlsTerminalCommitAttempt;

/// Immutable terminal mutation selected before entering the final CAS.
#[derive(Debug, Clone)]
pub(crate) enum HlsTerminalLeaseDecision {
    Tail(Arc<HlsTerminalTailPlan>),
    Unavailable(HlsTerminalTailCompatibility),
    UnavailableAfterOwnerFailure(HlsTerminalTailCompatibility),
}

/// Revalidates the configured terminal asset at the final CAS boundary and on
/// every autonomous retry without retaining configuration locks.
pub(crate) struct HlsTerminalAssetRevisionGuard {
    expected: HlsRuntimeCustomTailRevision,
    current: Arc<dyn Fn() -> HlsRuntimeCustomTailRevision + Send + Sync>,
}

/// One authoritative configured-asset snapshot taken at a terminal CAS boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsTerminalAssetRevisionValidation {
    Current,
    Changed { current: HlsRuntimeCustomTailRevision },
}

impl HlsTerminalAssetRevisionGuard {
    #[cfg(test)]
    pub(crate) fn new(
        expected: Option<HlsTerminalAssetIdentity>,
        current: impl Fn() -> Option<HlsTerminalAssetIdentity> + Send + Sync + 'static,
    ) -> Self {
        Self {
            expected: HlsRuntimeCustomTailRevision {
                reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
                asset: expected,
            },
            current: Arc::new(move || HlsRuntimeCustomTailRevision {
                reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
                asset: current(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_runtime_tail(
        expected: HlsRuntimeCustomTailAssetIdentity,
        current: impl Fn() -> Option<HlsRuntimeCustomTailAssetIdentity> + Send + Sync + 'static,
    ) -> Self {
        Self::for_optional_runtime_tail(expected.reason, Some(expected), current)
    }

    pub(crate) fn for_optional_runtime_tail(
        reason: HlsRuntimeCustomTailReason,
        expected: Option<HlsRuntimeCustomTailAssetIdentity>,
        current: impl Fn() -> Option<HlsRuntimeCustomTailAssetIdentity> + Send + Sync + 'static,
    ) -> Self {
        Self {
            expected: expected.map_or_else(
                || HlsRuntimeCustomTailRevision::missing(reason),
                HlsRuntimeCustomTailRevision::from_identity,
            ),
            current: Arc::new(move || {
                current().map_or_else(
                    || HlsRuntimeCustomTailRevision::missing(reason),
                    HlsRuntimeCustomTailRevision::from_identity,
                )
            }),
        }
    }

    pub(crate) fn current_identity(&self) -> HlsRuntimeCustomTailRevision {
        (self.current)()
    }

    pub(crate) fn validate_current(&self) -> HlsTerminalAssetRevisionValidation {
        let current = self.current_identity();
        if current == self.expected {
            HlsTerminalAssetRevisionValidation::Current
        } else {
            HlsTerminalAssetRevisionValidation::Changed { current }
        }
    }

    /// Authorizes a pending bundle only when both the request snapshot and the
    /// authoritative current configuration identify that exact asset.
    pub(crate) fn authorizes_current_asset(
        &self,
        asset: HlsRuntimeCustomTailAssetIdentity,
    ) -> bool {
        self.expected == HlsRuntimeCustomTailRevision::from_identity(asset)
            && self.current_identity() == HlsRuntimeCustomTailRevision::from_identity(asset)
    }

    fn strictly_supersedes_asset_binding(&self, existing: &Self) -> bool {
        let current = self.current_identity();
        self.expected.reason == existing.expected.reason && self.expected == current && existing.expected != current
    }

    #[cfg(test)]
    pub(crate) fn matching_for_test(expected: Option<super::terminal_tail::HlsTerminalAssetIdentity>) -> Self {
        Self::new(expected, move || expected)
    }

    #[cfg(test)]
    pub(crate) fn matching_runtime_for_test(expected: HlsRuntimeCustomTailAssetIdentity) -> Self {
        Self::for_runtime_tail(expected, move || Some(expected))
    }
}

impl std::fmt::Debug for HlsTerminalAssetRevisionGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsTerminalAssetRevisionGuard")
            .field("reason", &self.expected.reason)
            .field("has_expected_asset", &self.expected.asset.is_some())
            .finish_non_exhaustive()
    }
}

/// Canonical singleflight slot for one lease incarnation and terminal decision generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct HlsTerminalCommitOwnerKey {
    pub proxy_session_id: ProxySessionId,
    pub lease_id: HlsAccessLeaseId,
    pub lease_issued_at_ms: u64,
    pub decision_generation: u64,
}

impl HlsTerminalCommitOwnerKey {
    pub(crate) fn from_preparation(
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsTerminalTailPreparation,
    ) -> Self {
        Self {
            proxy_session_id: proxy_session_id.clone(),
            lease_id: lease_id.clone(),
            lease_issued_at_ms: preparation.lease_issued_at_ms,
            decision_generation: preparation.decision_generation,
        }
    }
}

/// Immutable command retained independently of the client request on `LockBusy`.
pub(crate) struct HlsTerminalCommitCommand {
    pub key: HlsTerminalCommitOwnerKey,
    pub session: HlsSessionHandle,
    pub session_incarnation: HlsSessionIncarnation,
    pub preparation: HlsTerminalTailPreparation,
    pub decision: HlsTerminalLeaseDecision,
    pub media_guard: Option<HlsTerminalCommitMediaGuard>,
    pub asset_revision_guard: HlsTerminalAssetRevisionGuard,
    pub cancellation_epoch: u64,
    pub submission_token: HlsTerminalCommitOwnerToken,
}

impl HlsTerminalCommitCommand {
    fn latest_safe_terminal_commit_at_ms(&self) -> u64 {
        self.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch()
    }

    /// Monotonic session/lease evidence used to prevent a late stale request
    /// from replacing a newer command for the same lease decision generation.
    fn evidence_version(&self) -> [u64; 7] {
        [
            self.preparation.origin_epoch,
            self.preparation.origin_progress_generation,
            self.preparation.media_readiness_generation,
            self.preparation.manifest_snapshot_generation,
            self.preparation.cursor_generation,
            self.preparation.expected_acceptance_generation.0,
            self.preparation.last_media_progress_at_ms.unwrap_or(0),
        ]
    }

    fn is_unavailable_after_owner_failure(&self) -> bool {
        matches!(&self.decision, HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_))
    }

    fn may_replace(&self, current: &Self) -> bool {
        if !Arc::ptr_eq(&self.session, &current.session) {
            // Request order does not prove session-incarnation order while the
            // session index itself is contended. Only the store-assigned
            // monotonic identity may displace another handle.
            return self.session_incarnation > current.session_incarnation;
        }
        let evidence_order = self.evidence_version().cmp(&current.evidence_version());
        if self.is_unavailable_after_owner_failure()
            && !current.is_unavailable_after_owner_failure()
            && evidence_order != std::cmp::Ordering::Less
        {
            return true;
        }
        terminal_commit_command_binding_may_replace(
            evidence_order,
            &self.asset_revision_guard,
            &current.asset_revision_guard,
        )
    }
}

fn terminal_commit_command_binding_may_replace(
    evidence_order: std::cmp::Ordering,
    incoming: &HlsTerminalAssetRevisionGuard,
    existing: &HlsTerminalAssetRevisionGuard,
) -> bool {
    match evidence_order {
        std::cmp::Ordering::Greater if incoming.expected == existing.expected => true,
        std::cmp::Ordering::Greater | std::cmp::Ordering::Equal => {
            incoming.strictly_supersedes_asset_binding(existing)
        }
        std::cmp::Ordering::Less => false,
    }
}

impl std::fmt::Debug for HlsTerminalCommitCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let decision_kind = match &self.decision {
            HlsTerminalLeaseDecision::Tail(_) => "tail",
            HlsTerminalLeaseDecision::Unavailable(_) => "unavailable",
            HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_) => "unavailable-after-owner-failure",
        };
        formatter
            .debug_struct("HlsTerminalCommitCommand")
            .field("lease_issued_at_ms", &self.preparation.lease_issued_at_ms)
            .field("decision_generation", &self.preparation.decision_generation)
            .field("session_incarnation", &self.session_incarnation)
            .field("manifest_snapshot_generation", &self.preparation.manifest_snapshot_generation)
            .field("cursor_generation", &self.preparation.cursor_generation)
            .field("origin_epoch", &self.preparation.origin_epoch)
            .field("origin_progress_generation", &self.preparation.origin_progress_generation)
            .field("media_readiness_generation", &self.preparation.media_readiness_generation)
            .field("acceptance_generation", &self.preparation.expected_acceptance_generation)
            .field("decision_kind", &decision_kind)
            .field("holds_media_guard", &self.media_guard.is_some())
            .field("asset_revision_guard", &self.asset_revision_guard)
            .field("cancellation_epoch", &self.cancellation_epoch)
            .field("submission_token", &self.submission_token)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum HlsTerminalCommitRetryDecision {
    Schedule { retry_at_ms: u64, attempts_completed: u8 },
    AttemptsExhausted,
    SafeDeadlineElapsed,
}

const fn terminal_commit_retry_backoff_ms(attempts_completed: u8) -> u64 {
    let candidate_exponent = attempts_completed.saturating_sub(1) as u32;
    let exponent = if candidate_exponent < 31 { candidate_exponent } else { 31 };
    match HLS_TERMINAL_COMMIT_INITIAL_BACKOFF_MS.checked_shl(exponent) {
        Some(backoff_ms) => {
            if backoff_ms < HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS {
                backoff_ms
            } else {
                HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS
            }
        }
        None => HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS,
    }
}

/// Pure bounded retry policy. Every retry timestamp is strictly earlier than
/// the frozen latest-safe commit timestamp; equality closes the acquisition
/// window and never creates technical grace after the transition margin.
pub(crate) fn next_terminal_commit_retry(
    attempts_completed: u8,
    last_attempt_at_ms: u64,
    latest_safe_terminal_commit_at_ms: u64,
) -> HlsTerminalCommitRetryDecision {
    if last_attempt_at_ms >= latest_safe_terminal_commit_at_ms {
        return HlsTerminalCommitRetryDecision::SafeDeadlineElapsed;
    }
    if attempts_completed >= HLS_TERMINAL_COMMIT_MAX_ATTEMPTS {
        return HlsTerminalCommitRetryDecision::AttemptsExhausted;
    }
    let next_attempt = attempts_completed.saturating_add(1);
    let backoff_ms = terminal_commit_retry_backoff_ms(attempts_completed);
    let retry_at_ms = last_attempt_at_ms.saturating_add(backoff_ms);
    if retry_at_ms >= latest_safe_terminal_commit_at_ms {
        return HlsTerminalCommitRetryDecision::SafeDeadlineElapsed;
    }
    HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed: next_attempt }
}

/// Authoritative owner selected before an immediate terminal CAS.
#[must_use]
pub(crate) enum HlsTerminalCommitSubmissionDecision {
    Attempt {
        command: Arc<HlsTerminalCommitCommand>,
        owner_token: HlsTerminalCommitOwnerToken,
    },
    PendingExisting { retry_before_ms: u64 },
    Failed(HlsTerminalCommitOutcome),
    Cancelled,
    CapacityExceeded,
}

#[must_use]
pub(crate) enum HlsTerminalCommitRetryScheduleDecision {
    Scheduled { worker_token: Option<HlsTerminalCommitWorkerToken> },
    Failed(HlsTerminalCommitOutcome),
    Cancelled,
    WorkerUnavailable,
}

/// Process-lifetime token binding an authorized command to one owner state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HlsTerminalCommitOwnerToken(u64);

impl HlsTerminalCommitOwnerToken {
    #[cfg(test)]
    const fn for_test(token: u64) -> Self { Self(token) }

    #[cfg(test)]
    const fn as_u64(self) -> u64 { self.0 }
}

/// Generation assigned to one uniquely active retry worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HlsTerminalCommitWorkerToken(u64);

/// Lifecycle action after the current worker has stopped for any reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum HlsTerminalCommitWorkerFinished {
    Stopped,
    Restart { worker_token: HlsTerminalCommitWorkerToken },
    RestartBudgetExhausted,
    StaleWorker,
}

#[must_use]
pub(crate) enum HlsTerminalCommitRetryAction {
    Due { key: HlsTerminalCommitOwnerKey, owner_token: HlsTerminalCommitOwnerToken, session: HlsSessionHandle },
    WaitUntil(u64),
    AwaitNotification,
    Stop,
}

struct HlsTerminalCommitRetryOwner {
    owner_token: HlsTerminalCommitOwnerToken,
    command: Arc<HlsTerminalCommitCommand>,
    attempts_completed: u8,
    effective_deadline_ms: u64,
    scheduled_at_ms: u64,
    scheduling_sequence: u64,
    in_flight: bool,
    terminal_failure: Option<HlsTerminalCommitOutcome>,
}

struct HlsTerminalCommitWorkerOwner {
    worker_token: HlsTerminalCommitWorkerToken,
    key: HlsTerminalCommitOwnerKey,
    owner_token: HlsTerminalCommitOwnerToken,
}

struct HlsTerminalCommitExistingOwner {
    owner_token: HlsTerminalCommitOwnerToken,
    command: Arc<HlsTerminalCommitCommand>,
    attempts_completed: u8,
    effective_deadline_ms: u64,
    scheduled_at_ms: u64,
    in_flight: bool,
    terminal_failure: Option<HlsTerminalCommitOutcome>,
}

impl HlsTerminalCommitExistingOwner {
    fn snapshot(owner: &HlsTerminalCommitRetryOwner) -> Self {
        Self {
            owner_token: owner.owner_token,
            command: Arc::clone(&owner.command),
            attempts_completed: owner.attempts_completed,
            effective_deadline_ms: owner.effective_deadline_ms,
            scheduled_at_ms: owner.scheduled_at_ms,
            in_flight: owner.in_flight,
            terminal_failure: owner.terminal_failure,
        }
    }
}

#[derive(Default)]
struct HlsTerminalCommitRetryState {
    owners: HashMap<HlsTerminalCommitOwnerKey, HlsTerminalCommitRetryOwner>,
    next_owner_token: u64,
    next_scheduling_sequence: u64,
    worker_generation: u64,
    active_worker: Option<HlsTerminalCommitWorkerToken>,
    active_worker_owner: Option<HlsTerminalCommitWorkerOwner>,
    worker_restarts: u8,
    cancellation_epoch: u64,
}

fn terminal_commit_schedule_cmp(
    left_at_ms: u64,
    left_sequence: u64,
    right_at_ms: u64,
    right_sequence: u64,
) -> std::cmp::Ordering {
    left_at_ms.cmp(&right_at_ms).then_with(|| left_sequence.cmp(&right_sequence))
}

fn terminal_commit_scheduled_before_deadline(
    requested_at_ms: u64,
    now_ms: u64,
    effective_deadline_ms: u64,
) -> Option<u64> {
    if now_ms >= effective_deadline_ms {
        return None;
    }
    Some(requested_at_ms.min(effective_deadline_ms.saturating_sub(1)).max(now_ms))
}

fn next_terminal_commit_scheduling_sequence(state: &mut HlsTerminalCommitRetryState) -> u64 {
    if let Some(next) = state.next_scheduling_sequence.checked_add(1) {
        state.next_scheduling_sequence = next;
        return next;
    }

    // The coordinator is bounded, so an exhausted process-lifetime counter can
    // be compacted without dropping an owner or changing the existing order.
    let mut ordered = state
        .owners
        .iter()
        .map(|(key, owner)| (key.clone(), owner.scheduling_sequence, owner.owner_token))
        .collect::<Vec<_>>();
    ordered.sort_by(|(_, left_sequence, left_token), (_, right_sequence, right_token)| {
        left_sequence.cmp(right_sequence).then_with(|| left_token.cmp(right_token))
    });
    for (position, (key, _, _)) in ordered.iter().enumerate() {
        if let Some(owner) = state.owners.get_mut(key) {
            owner.scheduling_sequence = u64::try_from(position).unwrap_or(u64::MAX).saturating_add(1);
        }
    }
    state.next_scheduling_sequence = u64::try_from(ordered.len()).unwrap_or(u64::MAX.saturating_sub(1));
    let next = state.next_scheduling_sequence.saturating_add(1);
    state.next_scheduling_sequence = next;
    next
}

fn claim_terminal_commit_worker(state: &mut HlsTerminalCommitRetryState) -> Option<HlsTerminalCommitWorkerToken> {
    if state.active_worker.is_some() {
        return None;
    }
    let generation = state.worker_generation.checked_add(1)?;
    state.worker_generation = generation;
    let token = HlsTerminalCommitWorkerToken(generation);
    state.active_worker = Some(token);
    Some(token)
}

fn claim_terminal_commit_worker_for_registration(
    state: &mut HlsTerminalCommitRetryState,
) -> Option<HlsTerminalCommitWorkerToken> {
    if state.worker_restarts >= HLS_TERMINAL_COMMIT_MAX_WORKER_RESTARTS {
        return None;
    }
    claim_terminal_commit_worker(state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsTerminalCommitWorkerOwnedWork {
    Drained,
    Remaining,
}

fn finish_terminal_commit_worker(
    state: &mut HlsTerminalCommitRetryState,
    worker_token: HlsTerminalCommitWorkerToken,
    owned_work: HlsTerminalCommitWorkerOwnedWork,
) -> HlsTerminalCommitWorkerFinished {
    if state.active_worker != Some(worker_token) {
        return HlsTerminalCommitWorkerFinished::StaleWorker;
    }
    state.active_worker = None;
    if owned_work == HlsTerminalCommitWorkerOwnedWork::Drained {
        state.worker_restarts = 0;
        return HlsTerminalCommitWorkerFinished::Stopped;
    }
    if state.worker_restarts >= HLS_TERMINAL_COMMIT_MAX_WORKER_RESTARTS {
        return HlsTerminalCommitWorkerFinished::RestartBudgetExhausted;
    }
    state.worker_restarts = state.worker_restarts.saturating_add(1);
    match claim_terminal_commit_worker(state) {
        Some(worker_token) => HlsTerminalCommitWorkerFinished::Restart { worker_token },
        None => HlsTerminalCommitWorkerFinished::RestartBudgetExhausted,
    }
}

/// Bounded coordinator with one owner per exact lease incarnation and decision generation.
pub(crate) struct HlsTerminalCommitRetryCoordinator {
    state: std::sync::Mutex<HlsTerminalCommitRetryState>,
    notify: tokio::sync::Notify,
    capacity: AtomicUsize,
}

impl Default for HlsTerminalCommitRetryCoordinator {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(HlsTerminalCommitRetryState::default()),
            notify: tokio::sync::Notify::new(),
            capacity: AtomicUsize::new(HLS_TERMINAL_COMMIT_RETRY_CAPACITY),
        }
    }
}

impl HlsTerminalCommitRetryCoordinator {
    /// Linearizes every client-driven terminal submission before any CAS. The
    /// returned token is the only token allowed to execute or complete the
    /// selected command.
    pub(crate) fn submit(
        &self,
        command: HlsTerminalCommitCommand,
        now_ms: u64,
    ) -> HlsTerminalCommitSubmissionDecision {
        let mut state = self.lock_state();
        if command.cancellation_epoch != state.cancellation_epoch {
            return HlsTerminalCommitSubmissionDecision::Cancelled;
        }
        let key = command.key.clone();
        let incoming_deadline_ms = command.latest_safe_terminal_commit_at_ms();
        if let Some(existing) = state.owners.get(&key).map(HlsTerminalCommitExistingOwner::snapshot) {
            return self.submit_existing(&mut state, command, &key, now_ms, incoming_deadline_ms, existing);
        }
        self.submit_new(&mut state, command, key, now_ms, incoming_deadline_ms)
    }

    fn submit_existing(
        &self,
        state: &mut HlsTerminalCommitRetryState,
        command: HlsTerminalCommitCommand,
        key: &HlsTerminalCommitOwnerKey,
        now_ms: u64,
        incoming_deadline_ms: u64,
        existing: HlsTerminalCommitExistingOwner,
    ) -> HlsTerminalCommitSubmissionDecision {
        let effective_deadline_ms = existing.effective_deadline_ms.min(incoming_deadline_ms);
        if !command.may_replace(&existing.command) {
            let deadline_tightened = effective_deadline_ms < existing.effective_deadline_ms;
                let tightened_scheduled_at_ms = if deadline_tightened {
                    terminal_commit_scheduled_before_deadline(
                        existing.scheduled_at_ms,
                        now_ms,
                        effective_deadline_ms,
                    )
                } else {
                    None
                };
                let schedule_changed = tightened_scheduled_at_ms
                    .is_some_and(|scheduled_at_ms| scheduled_at_ms != existing.scheduled_at_ms);
                let scheduling_sequence =
                    schedule_changed.then(|| next_terminal_commit_scheduling_sequence(state));
                if let Some(owner) = state.owners.get_mut(key) {
                    owner.effective_deadline_ms = effective_deadline_ms;
                    if let (Some(scheduled_at_ms), Some(scheduling_sequence)) =
                        (tightened_scheduled_at_ms, scheduling_sequence)
                    {
                        owner.scheduled_at_ms = scheduled_at_ms;
                        owner.scheduling_sequence = scheduling_sequence;
                    }
                    if now_ms >= effective_deadline_ms {
                        owner.in_flight = false;
                        owner.terminal_failure = Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
                    }
                }
                if deadline_tightened {
                    self.notify.notify_one();
                }
                if now_ms >= effective_deadline_ms {
                    return HlsTerminalCommitSubmissionDecision::Failed(
                        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
                    );
                }
                if let Some(outcome) = existing.terminal_failure {
                    return HlsTerminalCommitSubmissionDecision::Failed(outcome);
                }
                if existing.in_flight || existing.scheduled_at_ms > now_ms {
                    let retry_before_ms = terminal_commit_scheduled_before_deadline(
                        existing.scheduled_at_ms.max(now_ms.saturating_add(1)),
                        now_ms,
                        effective_deadline_ms,
                    )
                    .unwrap_or(effective_deadline_ms);
                    return HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms };
                }
                if let Some(owner) = state.owners.get_mut(key) {
                    owner.in_flight = true;
                }
                return HlsTerminalCommitSubmissionDecision::Attempt {
                    command: existing.command,
                    owner_token: existing.owner_token,
                };
        }

        let owner_token = command.submission_token;
        let command = Arc::new(command);
        let scheduling_sequence = next_terminal_commit_scheduling_sequence(state);
        let deadline_elapsed = now_ms >= effective_deadline_ms;
        if let Some(owner) = state.owners.get_mut(key) {
            owner.owner_token = owner_token;
            owner.command = Arc::clone(&command);
            owner.attempts_completed = existing.attempts_completed.max(1);
            owner.effective_deadline_ms = effective_deadline_ms;
            owner.scheduled_at_ms = now_ms;
            owner.scheduling_sequence = scheduling_sequence;
            owner.in_flight = !deadline_elapsed;
            owner.terminal_failure = deadline_elapsed.then_some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
        }
        self.notify.notify_one();
        if deadline_elapsed {
            return HlsTerminalCommitSubmissionDecision::Failed(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
        }
        HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token }
    }

    fn submit_new(
        &self,
        state: &mut HlsTerminalCommitRetryState,
        command: HlsTerminalCommitCommand,
        key: HlsTerminalCommitOwnerKey,
        now_ms: u64,
        incoming_deadline_ms: u64,
    ) -> HlsTerminalCommitSubmissionDecision {
        if now_ms >= incoming_deadline_ms {
            return HlsTerminalCommitSubmissionDecision::Failed(
                HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
            );
        }
        if state.owners.len() >= self.capacity.load(Ordering::Acquire) {
            return HlsTerminalCommitSubmissionDecision::CapacityExceeded;
        }
        if state.owners.is_empty() && state.active_worker.is_none() {
            state.worker_restarts = 0;
        }
        let owner_token = command.submission_token;
        let command = Arc::new(command);
        let scheduling_sequence = next_terminal_commit_scheduling_sequence(state);
        state.owners.insert(
            key,
            HlsTerminalCommitRetryOwner {
                owner_token,
                command: Arc::clone(&command),
                attempts_completed: 1,
                effective_deadline_ms: incoming_deadline_ms,
                scheduled_at_ms: now_ms,
                scheduling_sequence,
                in_flight: true,
                terminal_failure: None,
            },
        );
        HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token }
    }

    pub(crate) fn schedule_current(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        attempts_completed: u8,
        scheduled_at_ms: u64,
        now_ms: u64,
    ) -> HlsTerminalCommitRetryScheduleDecision {
        let mut state = self.lock_state();
        let Some(owner) = state
            .owners
            .get(key)
            .filter(|owner| owner.owner_token == owner_token)
        else {
            return HlsTerminalCommitRetryScheduleDecision::Cancelled;
        };
        if let Some(outcome) = owner.terminal_failure {
            return HlsTerminalCommitRetryScheduleDecision::Failed(outcome);
        }
        let effective_deadline_ms = owner.effective_deadline_ms;
        let Some(scheduled_at_ms) =
            terminal_commit_scheduled_before_deadline(scheduled_at_ms, now_ms, effective_deadline_ms)
        else {
            if let Some(owner) = state.owners.get_mut(key) {
                owner.in_flight = false;
                owner.terminal_failure = Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
            }
            return HlsTerminalCommitRetryScheduleDecision::Failed(
                HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
            );
        };
        let scheduling_sequence = next_terminal_commit_scheduling_sequence(&mut state);
        if let Some(owner) = state.owners.get_mut(key) {
            owner.in_flight = false;
            owner.attempts_completed = owner.attempts_completed.max(attempts_completed);
            owner.scheduled_at_ms = scheduled_at_ms;
            owner.scheduling_sequence = scheduling_sequence;
        }
        let worker_token = if state.active_worker.is_none() {
            let Some(worker_token) = claim_terminal_commit_worker_for_registration(&mut state) else {
                if let Some(owner) = state.owners.get_mut(key) {
                    owner.terminal_failure = Some(HlsTerminalCommitOutcome::RetryWorkerUnavailable);
                }
                return HlsTerminalCommitRetryScheduleDecision::WorkerUnavailable;
            };
            Some(worker_token)
        } else {
            None
        };
        drop(state);
        self.notify.notify_one();
        HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token }
    }

    pub(crate) fn next_action(
        &self,
        worker_token: HlsTerminalCommitWorkerToken,
        now_ms: u64,
    ) -> HlsTerminalCommitRetryAction {
        let mut state = self.lock_state();
        if state.active_worker != Some(worker_token) {
            return HlsTerminalCommitRetryAction::Stop;
        }
        if state
            .active_worker_owner
            .as_ref()
            .is_some_and(|owner| owner.worker_token == worker_token)
        {
            state.active_worker_owner = None;
        }
        for owner in state.owners.values_mut().filter(|owner| {
            owner.terminal_failure.is_none() && now_ms >= owner.effective_deadline_ms
        }) {
            owner.in_flight = false;
            owner.terminal_failure = Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
        }
        let next = state
            .owners
            .iter()
            .filter(|(_, owner)| !owner.in_flight && owner.terminal_failure.is_none())
            .map(|(key, owner)| (key.clone(), owner.scheduled_at_ms, owner.scheduling_sequence, owner.owner_token))
            .min_by(|(_, left_at, left_sequence, left_token), (_, right_at, right_sequence, right_token)| {
                terminal_commit_schedule_cmp(*left_at, *left_sequence, *right_at, *right_sequence)
                    .then_with(|| left_token.cmp(right_token))
            });
        let Some((key, scheduled_at_ms, _, _)) = next else {
            if state.owners.values().any(|owner| owner.in_flight && owner.terminal_failure.is_none()) {
                return HlsTerminalCommitRetryAction::AwaitNotification;
            }
            return HlsTerminalCommitRetryAction::Stop;
        };
        if scheduled_at_ms > now_ms {
            return HlsTerminalCommitRetryAction::WaitUntil(scheduled_at_ms);
        }
        let Some(owner) = state.owners.get_mut(&key) else {
            return HlsTerminalCommitRetryAction::Stop;
        };
        owner.in_flight = true;
        let owner_token = owner.owner_token;
        let session = Arc::clone(&owner.command.session);
        state.active_worker_owner = Some(HlsTerminalCommitWorkerOwner {
            worker_token,
            key: key.clone(),
            owner_token,
        });
        HlsTerminalCommitRetryAction::Due {
            key,
            owner_token,
            session,
        }
    }

    /// Releases exactly the worker generation that exited. If bounded work is
    /// still owned, the caller receives the only token authorized to start a
    /// replacement. A stale worker can neither stop nor replace the active one.
    pub(crate) fn worker_finished(
        &self,
        worker_token: HlsTerminalCommitWorkerToken,
    ) -> HlsTerminalCommitWorkerFinished {
        let mut state = self.lock_state();
        if state.active_worker != Some(worker_token) {
            return HlsTerminalCommitWorkerFinished::StaleWorker;
        }
        if let Some(worker_owner) = state.active_worker_owner.take().filter(|owner| {
            owner.worker_token == worker_token
        }) {
            if let Some(owner) = state
                .owners
                .get_mut(&worker_owner.key)
                .filter(|owner| owner.owner_token == worker_owner.owner_token)
            {
                owner.in_flight = false;
            }
        }
        let owned_work = if state.owners.values().all(|owner| owner.terminal_failure.is_some()) {
            HlsTerminalCommitWorkerOwnedWork::Drained
        } else {
            HlsTerminalCommitWorkerOwnedWork::Remaining
        };
        let finished = finish_terminal_commit_worker(&mut state, worker_token, owned_work);
        drop(state);
        self.notify.notify_waiters();
        finished
    }

    #[cfg(test)]
    pub(crate) fn early_worker_exit_for_test(
        &self,
        worker_token: HlsTerminalCommitWorkerToken,
    ) -> HlsTerminalCommitWorkerFinished {
        self.worker_finished(worker_token)
    }

    pub(crate) fn reschedule(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        attempts_completed: u8,
        scheduled_at_ms: u64,
    ) {
        let mut state = self.lock_state();
        let current = state.owners.get(key).is_some_and(|owner| owner.owner_token == owner_token);
        if current {
            let scheduling_sequence = next_terminal_commit_scheduling_sequence(&mut state);
            if let Some(owner) = state.owners.get_mut(key) {
                owner.in_flight = false;
                owner.attempts_completed = attempts_completed;
                owner.scheduled_at_ms = scheduled_at_ms;
                owner.scheduling_sequence = scheduling_sequence;
            }
        }
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) fn complete_owner(&self, key: &HlsTerminalCommitOwnerKey, owner_token: HlsTerminalCommitOwnerToken) {
        let mut state = self.lock_state();
        let remove = state.owners.get(key).is_some_and(|owner| owner.owner_token == owner_token);
        if remove {
            state.owners.remove(key);
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Retains a bounded fail-closed intent for the exact command generation.
    /// A later equal request observes the typed terminal failure; only newer
    /// monotonic evidence may replace it with fresh autonomous work.
    pub(crate) fn fail_owner(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        outcome: HlsTerminalCommitOutcome,
    ) {
        let mut state = self.lock_state();
        if let Some(owner) = state
            .owners
            .get_mut(key)
            .filter(|owner| owner.owner_token == owner_token)
        {
            owner.in_flight = false;
            owner.terminal_failure = Some(outcome);
        }
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) fn fail_runnable_owners(&self, outcome: HlsTerminalCommitOutcome) {
        let mut state = self.lock_state();
        for owner in state.owners.values_mut().filter(|owner| owner.terminal_failure.is_none()) {
            owner.in_flight = false;
            owner.terminal_failure = Some(outcome);
        }
        drop(state);
        self.notify.notify_waiters();
    }

    pub(crate) fn discard_owner(&self, key: &HlsTerminalCommitOwnerKey, owner_token: HlsTerminalCommitOwnerToken) {
        let mut state = self.lock_state();
        if state.owners.get(key).is_some_and(|owner| owner.owner_token == owner_token) {
            state.owners.remove(key);
        }
        drop(state);
        self.notify.notify_one();
    }

    /// Runs one synchronous CAS only while this exact retry owner remains
    /// registered. Cancellation and cleanup use the same mutex, so a removed
    /// in-flight command cannot mutate state after cancellation has returned.
    pub(crate) fn with_current_owner<R>(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        operation: impl FnOnce(&HlsTerminalCommitCommand, u64) -> R,
    ) -> Option<(u8, R)> {
        let state = self.lock_state();
        let owner = state.owners.get(key).filter(|owner| {
            owner.owner_token == owner_token && owner.in_flight && owner.terminal_failure.is_none()
        })?;
        let attempts_completed = owner.attempts_completed;
        let result = operation(&owner.command, owner.effective_deadline_ms);
        drop(state);
        Some((attempts_completed, result))
    }

    pub(crate) fn reserve_submission(&self) -> Option<(u64, HlsTerminalCommitOwnerToken)> {
        let mut state = self.lock_state();
        let submission_token = state.next_owner_token.checked_add(1)?;
        state.next_owner_token = submission_token;
        Some((state.cancellation_epoch, HlsTerminalCommitOwnerToken(submission_token)))
    }

    pub(crate) fn cancel_lease(&self, lease_id: &HlsAccessLeaseId) {
        let mut state = self.lock_state();
        state.owners.retain(|key, _| &key.lease_id != lease_id);
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) fn cancel_session(&self, proxy_session_id: &ProxySessionId) {
        let mut state = self.lock_state();
        state.owners.retain(|key, _| &key.proxy_session_id != proxy_session_id);
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) fn clear(&self) {
        let mut state = self.lock_state();
        state.owners.clear();
        state.cancellation_epoch = state.cancellation_epoch.saturating_add(1);
        drop(state);
        self.notify.notify_one();
    }

    pub(crate) async fn notified(&self) { self.notify.notified().await }

    #[cfg(test)]
    pub(crate) fn owner_count(&self) -> usize {
        self.lock_state()
            .owners
            .values()
            .filter(|owner| owner.terminal_failure.is_none())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn set_capacity_for_test(&self, capacity: usize) {
        self.capacity.store(capacity, Ordering::Release);
        self.notify.notify_one();
    }

    #[cfg(test)]
    fn owner_snapshot(&self, key: &HlsTerminalCommitOwnerKey) -> Option<HlsTerminalCommitRetryOwnerSnapshot> {
        self.lock_state().owners.get(key).map(|owner| HlsTerminalCommitRetryOwnerSnapshot {
            owner_token: owner.owner_token,
            command_submission_token: owner.command.submission_token,
            attempts_completed: owner.attempts_completed,
            effective_deadline_ms: owner.effective_deadline_ms,
            scheduled_at_ms: owner.scheduled_at_ms,
            scheduling_sequence: owner.scheduling_sequence,
            in_flight: owner.in_flight,
            terminal_failure: owner.terminal_failure,
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HlsTerminalCommitRetryState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsTerminalCommitRetryOwnerSnapshot {
    owner_token: HlsTerminalCommitOwnerToken,
    command_submission_token: HlsTerminalCommitOwnerToken,
    attempts_completed: u8,
    effective_deadline_ms: u64,
    scheduled_at_ms: u64,
    scheduling_sequence: u64,
    in_flight: bool,
    terminal_failure: Option<HlsTerminalCommitOutcome>,
}

pub(super) fn spawn_terminal_commit_retry_worker(
    sessions: Arc<HlsSessionStore>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    retries: Arc<HlsTerminalCommitRetryCoordinator>,
    clock: Arc<HlsTerminalCommitClock>,
    worker_token: HlsTerminalCommitWorkerToken,
    attempt_terminal_commit: HlsTerminalCommitAttemptFn,
) {
    drop(tokio::spawn(supervise_terminal_commit_retry_worker(
        sessions,
        access_leases,
        retries,
        clock,
        worker_token,
        attempt_terminal_commit,
    )));
}

async fn supervise_terminal_commit_retry_worker(
    sessions: Arc<HlsSessionStore>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    retries: Arc<HlsTerminalCommitRetryCoordinator>,
    clock: Arc<HlsTerminalCommitClock>,
    mut worker_token: HlsTerminalCommitWorkerToken,
    attempt_terminal_commit: HlsTerminalCommitAttemptFn,
) {
    loop {
        let worker_result = std::panic::AssertUnwindSafe(run_terminal_commit_retry_worker(
            Arc::clone(&sessions),
            Arc::clone(&access_leases),
            Arc::clone(&retries),
            Arc::clone(&clock),
            worker_token,
            attempt_terminal_commit,
        ))
        .catch_unwind()
        .await;
        if worker_result.is_err() {
            error!("HLS terminal commit retry worker exited unexpectedly: reason=panic");
        }
        match retries.worker_finished(worker_token) {
            HlsTerminalCommitWorkerFinished::Stopped | HlsTerminalCommitWorkerFinished::StaleWorker => return,
            HlsTerminalCommitWorkerFinished::Restart { worker_token: replacement } => {
                worker_token = replacement;
                tokio::task::yield_now().await;
            }
            HlsTerminalCommitWorkerFinished::RestartBudgetExhausted => {
                retries.fail_runnable_owners(HlsTerminalCommitOutcome::RetryWorkerUnavailable);
                error!("HLS terminal commit retry worker stopped: reason=restart_budget_exhausted");
                return;
            }
        }
    }
}

async fn run_terminal_commit_retry_worker(
    sessions: Arc<HlsSessionStore>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    retries: Arc<HlsTerminalCommitRetryCoordinator>,
    clock: Arc<HlsTerminalCommitClock>,
    worker_token: HlsTerminalCommitWorkerToken,
    attempt_terminal_commit: HlsTerminalCommitAttemptFn,
) {
    loop {
        match retries.next_action(worker_token, clock.now_ms()) {
            HlsTerminalCommitRetryAction::Due { key, owner_token, session } => {
                let current_attempt = match sessions.try_with_current_proxy_session(
                    &key.proxy_session_id,
                    &session,
                    || {
                        retries.with_current_owner(&key, owner_token, |command, latest_safe_terminal_commit_at_ms| {
                            let attempt_now_ms = clock.now_ms();
                            let attempt = attempt_terminal_commit(
                                &access_leases,
                                command,
                                attempt_now_ms,
                                latest_safe_terminal_commit_at_ms,
                            );
                            (attempt_now_ms, latest_safe_terminal_commit_at_ms, attempt)
                        })
                    },
                ) {
                    HlsCurrentProxySessionAccess::Acquired(current) => current,
                    HlsCurrentProxySessionAccess::Superseded => {
                        retries.discard_owner(&key, owner_token);
                        None
                    }
                    HlsCurrentProxySessionAccess::LockBusy => {
                        retries.with_current_owner(&key, owner_token, |_command, latest_safe_terminal_commit_at_ms| {
                            let attempt_now_ms = clock.now_ms();
                            (
                                attempt_now_ms,
                                latest_safe_terminal_commit_at_ms,
                                HlsTerminalCommitAttempt::LockBusy,
                            )
                        })
                    }
                };
                let Some((attempts_completed, (attempt_now_ms, deadline_ms, attempt))) = current_attempt else {
                    continue;
                };
                let rescheduled = finish_terminal_commit_retry_attempt(
                    &retries,
                    &key,
                    owner_token,
                    attempts_completed,
                    attempt_now_ms,
                    deadline_ms,
                    attempt,
                );
                if rescheduled {
                    tokio::task::yield_now().await;
                }
            }
            HlsTerminalCommitRetryAction::WaitUntil(retry_at_ms) => {
                if clock.advance_fixed_retry_clock(retry_at_ms) {
                    tokio::task::yield_now().await;
                    continue;
                }
                let wait_ms = retry_at_ms.saturating_sub(clock.now_ms());
                tokio::select! {
                    () = retries.notified() => {}
                    () = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                }
            }
            HlsTerminalCommitRetryAction::AwaitNotification => retries.notified().await,
            HlsTerminalCommitRetryAction::Stop => return,
        }
    }
}

fn finish_terminal_commit_retry_attempt(
    retries: &HlsTerminalCommitRetryCoordinator,
    key: &HlsTerminalCommitOwnerKey,
    owner_token: HlsTerminalCommitOwnerToken,
    attempts_completed: u8,
    attempt_now_ms: u64,
    deadline_ms: u64,
    attempt: HlsTerminalCommitAttempt,
) -> bool {
    match attempt {
        HlsTerminalCommitAttempt::Completed(outcome) => {
            match outcome {
                HlsTerminalCommitOutcome::Committed
                | HlsTerminalCommitOutcome::AlreadyCommitted
                | HlsTerminalCommitOutcome::SupersededGeneration
                | HlsTerminalCommitOutcome::LeaseNoLongerEligible
                | HlsTerminalCommitOutcome::RecoveryCommitted
                | HlsTerminalCommitOutcome::CutoverNoLongerRequired => {
                    retries.complete_owner(key, owner_token);
                    debug!(
                        "HLS terminal commit retry owner completed: outcome={}",
                        terminal_commit_outcome_label(outcome)
                    );
                }
                HlsTerminalCommitOutcome::BundleNotReady
                | HlsTerminalCommitOutcome::BundleIncompatible
                | HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed
                | HlsTerminalCommitOutcome::RetryCapacityExceeded
                | HlsTerminalCommitOutcome::RetryAttemptsExhausted
                | HlsTerminalCommitOutcome::RetryWorkerUnavailable => {
                    retries.fail_owner(key, owner_token, outcome);
                    error!(
                        "HLS terminal commit retry owner failed closed: reason={}",
                        terminal_commit_outcome_label(outcome)
                    );
                }
                HlsTerminalCommitOutcome::LockBusy { .. } => {
                    retries.fail_owner(key, owner_token, HlsTerminalCommitOutcome::RetryWorkerUnavailable);
                    error!("HLS terminal commit retry owner failed closed: reason=invalid_lock_busy_completion");
                }
            }
            false
        }
        HlsTerminalCommitAttempt::LockBusy => match next_terminal_commit_retry(
            attempts_completed,
            attempt_now_ms,
            deadline_ms,
        ) {
            HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed } => {
                retries.reschedule(key, owner_token, attempts_completed, retry_at_ms);
                true
            }
            HlsTerminalCommitRetryDecision::AttemptsExhausted => {
                retries.fail_owner(key, owner_token, HlsTerminalCommitOutcome::RetryAttemptsExhausted);
                error!("HLS terminal commit retry owner failed closed: reason=retry_attempts_exhausted");
                false
            }
            HlsTerminalCommitRetryDecision::SafeDeadlineElapsed => {
                retries.fail_owner(key, owner_token, HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
                error!("HLS terminal commit retry owner failed closed: reason=safe_commit_deadline_elapsed");
                false
            }
        },
    }
}

const fn terminal_commit_outcome_label(outcome: HlsTerminalCommitOutcome) -> &'static str {
    match outcome {
        HlsTerminalCommitOutcome::Committed => "committed",
        HlsTerminalCommitOutcome::AlreadyCommitted => "already_committed",
        HlsTerminalCommitOutcome::SupersededGeneration => "superseded_generation",
        HlsTerminalCommitOutcome::LeaseNoLongerEligible => "lease_no_longer_eligible",
        HlsTerminalCommitOutcome::RecoveryCommitted => "recovery_committed",
        HlsTerminalCommitOutcome::CutoverNoLongerRequired => "cutover_no_longer_required",
        HlsTerminalCommitOutcome::BundleNotReady => "bundle_not_ready",
        HlsTerminalCommitOutcome::BundleIncompatible => "bundle_incompatible",
        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed => "safe_commit_deadline_elapsed",
        HlsTerminalCommitOutcome::RetryCapacityExceeded => "retry_capacity_exceeded",
        HlsTerminalCommitOutcome::RetryAttemptsExhausted => "retry_attempts_exhausted",
        HlsTerminalCommitOutcome::RetryWorkerUnavailable => "retry_worker_unavailable",
        HlsTerminalCommitOutcome::LockBusy { .. } => "lock_busy",
    }
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        claim_terminal_commit_worker, claim_terminal_commit_worker_for_registration, finish_terminal_commit_worker,
        next_terminal_commit_retry, next_terminal_commit_scheduling_sequence, terminal_commit_command_binding_may_replace,
        terminal_commit_retry_handoff_budget_ms, terminal_commit_retry_schedule_budget_ms, terminal_commit_schedule_cmp,
        HlsTerminalAssetRevisionGuard, HlsTerminalCommitCommand, HlsTerminalCommitOutcome,
        HlsTerminalCommitOwnerToken, HlsTerminalCommitRetryAction, HlsTerminalCommitRetryCoordinator,
        HlsTerminalCommitRetryDecision, HlsTerminalCommitRetryScheduleDecision, HlsTerminalCommitRetryState,
        HlsTerminalCommitSubmissionDecision, HlsTerminalCommitWorkerFinished, HlsTerminalCommitWorkerOwnedWork,
        HlsTerminalLeaseDecision,
        HLS_TERMINAL_COMMIT_EXCLUSIVE_DEADLINE_GUARD_MS, HLS_TERMINAL_COMMIT_MAX_ATTEMPTS,
        HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS, HLS_TERMINAL_COMMIT_MAX_WORKER_RESTARTS,
    };
    use crate::api::model::hls_cache::{
        lease::{HlsTerminalMediaRequirementSource, HlsTerminalTailPreparation},
        manifest_acceptance::HlsManifestAcceptanceGeneration,
        media_reserve::{
            HlsLeaseManifestSnapshot, HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot,
            HlsManifestDeliveryMode, HlsManifestSourceRenderMarker,
        },
        recovery_timing::{
            HlsLeaseCutoverTiming, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
            HlsTransitionMarginMs,
        },
        session_store::HlsSessionIncarnation,
        runtime_custom_tail::HlsRuntimeCustomTailRevision,
        terminal_tail::{HlsMediaContainer, HlsTerminalAssetIdentity, HlsTerminalTailCompatibility},
        HlsRuntimeCustomTailReason,
        HlsAccessLeaseId, HlsSession, HlsSessionKey, ProxySessionId,
    };
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn retry_command(
        lease: &str,
        deadline_ms: u64,
        submission_token: u64,
        evidence_generation: u64,
    ) -> HlsTerminalCommitCommand {
        let proxy_session_id = ProxySessionId("terminal-retry-test".to_string());
        let transition_margin = HlsTransitionMarginMs::from_millis(100);
        let reserve = HlsLeaseReserveSnapshot {
            availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
            guaranteed_media_horizon_ms: deadline_ms.saturating_add(100),
            conservative_playback_position_ms: 0,
            guaranteed_reserve_ms: deadline_ms.saturating_add(100),
            initial_hidden_ready_duration_ms: 0,
            transition_margin,
            key_readiness_valid_until_ms: None,
            recovery_required: true,
            cutover_required: true,
        };
        let preparation = HlsTerminalTailPreparation {
            trigger: super::super::runtime_custom_tail::HlsFiniteTailTrigger::AvailabilityReserve,
            runtime_policy_revocation: None,
            lease_issued_at_ms: 1,
            decision_generation: 1,
            expected_admission_generation: 1,
            manifest_snapshot_generation: evidence_generation,
            cursor_generation: 1,
            origin_progress_generation: 1,
            media_readiness_generation: 1,
            origin_epoch: 1,
            last_media_progress_at_ms: Some(1),
            expected_acceptance_generation: HlsManifestAcceptanceGeneration(1),
            terminal_media_requirement_source: HlsTerminalMediaRequirementSource::AcceptanceEpisode {
                generation: HlsManifestAcceptanceGeneration(1),
            },
            cutover_timing: HlsLeaseCutoverTiming::from_reserve(
                0,
                deadline_ms.saturating_add(100),
                transition_margin,
                None,
            ),
            commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
            required_terminal_media_key: None,
            terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
            reserve,
            manifest_snapshot: HlsLeaseManifestSnapshot {
                delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
                source_render_marker: HlsManifestSourceRenderMarker::new(1),
                snapshot_generation: evidence_generation,
                delivered_at_ms: 1,
                first_proxy_seq: 0,
                last_proxy_seq: 0,
                visible_segments: Arc::from([]),
                discontinuity_sequence: 0,
                target_duration_ms: 100,
                playlist_duration_ms: 0,
                last_visible_media_end_ms: 0,
                active_map: None,
                active_encryption: None,
                container: HlsMediaContainer::MpegTs,
            },
        };
        HlsTerminalCommitCommand {
            key: super::HlsTerminalCommitOwnerKey {
                proxy_session_id,
                lease_id: HlsAccessLeaseId(lease.to_string()),
                lease_issued_at_ms: 1,
                decision_generation: 1,
            },
            session: Arc::new(RwLock::new(HlsSession::new(
                HlsSessionKey::new(1, lease),
                b"terminal-retry-test",
                0,
            ))),
            session_incarnation: HlsSessionIncarnation::for_test(1),
            preparation,
            decision: HlsTerminalLeaseDecision::Unavailable(HlsTerminalTailCompatibility::MissingAsset),
            media_guard: None,
            asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_for_test(None),
            cancellation_epoch: 0,
            submission_token: HlsTerminalCommitOwnerToken::for_test(submission_token),
        }
    }

    fn scheduled_worker_token(
        decision: &HlsTerminalCommitRetryScheduleDecision,
    ) -> super::HlsTerminalCommitWorkerToken {
        let HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token: Some(worker_token) } = decision else {
            panic!("first terminal retry owner must start one worker");
        };
        *worker_token
    }

    fn submit_and_schedule(
        coordinator: &HlsTerminalCommitRetryCoordinator,
        command: HlsTerminalCommitCommand,
        attempts_completed: u8,
        scheduled_at_ms: u64,
        now_ms: u64,
    ) -> (
        super::HlsTerminalCommitWorkerToken,
        super::HlsTerminalCommitOwnerKey,
        HlsTerminalCommitOwnerToken,
    ) {
        let HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token, .. } =
            coordinator.submit(command, now_ms)
        else {
            panic!("new terminal submission must own its immediate attempt");
        };
        let key = command.key.clone();
        let worker_token = scheduled_worker_token(&coordinator.schedule_current(
            &key,
            owner_token,
            attempts_completed,
            scheduled_at_ms,
            now_ms,
        ));
        (worker_token, key, owner_token)
    }

    fn asset_identity(revision: u64, fingerprint: u8) -> HlsTerminalAssetIdentity {
        HlsTerminalAssetIdentity { revision, fingerprint: [fingerprint; 32] }
    }

    fn asset_guard(
        expected: Option<HlsTerminalAssetIdentity>,
        current: Option<HlsTerminalAssetIdentity>,
    ) -> HlsTerminalAssetRevisionGuard {
        HlsTerminalAssetRevisionGuard::new(expected, move || current)
    }

    #[test]
    fn hls_terminal_commit_retry_equal_evidence_unavailable_cannot_replace_tail_without_current_asset_binding() {
        let stale_asset = asset_identity(900, 9);
        let current_asset = asset_identity(3, 3);
        let current_tail = asset_guard(Some(current_asset), Some(current_asset));
        let delayed_unavailable = asset_guard(Some(stale_asset), Some(current_asset));

        assert!(!terminal_commit_command_binding_may_replace(
            std::cmp::Ordering::Equal,
            &delayed_unavailable,
            &current_tail,
        ));
        assert!(!terminal_commit_command_binding_may_replace(
            std::cmp::Ordering::Greater,
            &delayed_unavailable,
            &current_tail,
        ));
    }

    #[test]
    fn hls_terminal_commit_retry_current_asset_binding_replaces_stale_owner_without_revision_ordering() {
        let stale_asset = asset_identity(900, 9);
        let current_asset = asset_identity(3, 3);
        let stale_owner = asset_guard(Some(stale_asset), Some(current_asset));
        let current_request = asset_guard(Some(current_asset), Some(current_asset));

        assert!(terminal_commit_command_binding_may_replace(
            std::cmp::Ordering::Equal,
            &current_request,
            &stale_owner,
        ));
        assert_eq!(
            current_request.current_identity(),
            HlsRuntimeCustomTailRevision {
                reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
                asset: Some(current_asset),
            }
        );

        let equal_current_owner = asset_guard(Some(current_asset), Some(current_asset));
        assert!(!terminal_commit_command_binding_may_replace(
            std::cmp::Ordering::Equal,
            &current_request,
            &equal_current_owner,
        ));
        assert!(terminal_commit_command_binding_may_replace(
            std::cmp::Ordering::Greater,
            &current_request,
            &equal_current_owner,
        ));
    }

    #[test]
    fn hls_terminal_commit_submission_unauthorized_incoming_cannot_fail_existing_owner() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let current_asset = asset_identity(3, 3);
        let stale_asset = asset_identity(9, 9);
        let mut existing = retry_command("unauthorized-failure", 300, 1, 1);
        existing.asset_revision_guard = asset_guard(Some(current_asset), Some(current_asset));
        let session = Arc::clone(&existing.session);
        let key = existing.key.clone();
        let (_worker, _, owner_token) = submit_and_schedule(&coordinator, existing, 7, 220, 100);
        let mut incoming = retry_command("unauthorized-failure", 200, 2, 1);
        incoming.session = session;
        incoming.asset_revision_guard = asset_guard(Some(stale_asset), Some(current_asset));

        assert!(matches!(
            coordinator.submit(incoming, 101),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms: 199 }
        ));
        let unauthorized_token = HlsTerminalCommitOwnerToken::for_test(2);
        coordinator.fail_owner(&key, unauthorized_token, HlsTerminalCommitOutcome::BundleIncompatible);
        coordinator.complete_owner(&key, unauthorized_token);

        let owner = coordinator.owner_snapshot(&key).expect("authorized owner remains registered");
        assert_eq!(owner.owner_token, owner_token);
        assert_eq!(owner.command_submission_token, owner_token);
        assert_eq!(owner.attempts_completed, 7);
        assert_eq!(owner.effective_deadline_ms, 200);
        assert_eq!(owner.terminal_failure, None);
    }

    #[test]
    fn hls_terminal_commit_submission_stronger_binding_replaces_before_attempt() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let current_asset = asset_identity(3, 3);
        let stale_asset = asset_identity(9, 9);
        let mut existing = retry_command("stronger-binding", 300, 1, 1);
        existing.asset_revision_guard = asset_guard(Some(stale_asset), Some(current_asset));
        let session = Arc::clone(&existing.session);
        let key = existing.key.clone();
        let (_worker, _, _) = submit_and_schedule(&coordinator, existing, 9, 220, 100);
        let mut incoming = retry_command("stronger-binding", 250, 2, 1);
        incoming.session = session;
        incoming.asset_revision_guard = asset_guard(Some(current_asset), Some(current_asset));

        assert!(matches!(
            coordinator.submit(incoming, 101),
            HlsTerminalCommitSubmissionDecision::Attempt { owner_token, .. } if owner_token.as_u64() == 2
        ));
        let owner = coordinator.owner_snapshot(&key).expect("replacement owner");
        assert_eq!(owner.owner_token.as_u64(), 2);
        assert_eq!(owner.command_submission_token.as_u64(), 2);
        assert_eq!(owner.attempts_completed, 9);
        assert_eq!(owner.effective_deadline_ms, 250);
        assert!(owner.in_flight);
    }

    #[test]
    fn hls_terminal_commit_submission_in_flight_owner_prevents_parallel_attempt() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("in-flight-singleflight", 300, 1, 1);
        let session = Arc::clone(&initial.session);
        let key = initial.key.clone();
        assert!(matches!(
            coordinator.submit(initial, 100),
            HlsTerminalCommitSubmissionDecision::Attempt { owner_token, .. } if owner_token.as_u64() == 1
        ));
        let mut incoming = retry_command("in-flight-singleflight", 300, 2, 1);
        incoming.session = session;

        assert!(matches!(
            coordinator.submit(incoming, 101),
            HlsTerminalCommitSubmissionDecision::PendingExisting { .. }
        ));
        let owner = coordinator.owner_snapshot(&key).expect("in-flight owner");
        assert_eq!(owner.owner_token.as_u64(), 1);
        assert!(owner.in_flight);
    }

    #[test]
    fn hls_terminal_commit_retry_never_schedules_past_safe_deadline() {
        assert_eq!(
            next_terminal_commit_retry(1, 1_000, 1_005),
            HlsTerminalCommitRetryDecision::Schedule { retry_at_ms: 1_001, attempts_completed: 2 }
        );
        assert_eq!(next_terminal_commit_retry(4, 1_004, 1_005), HlsTerminalCommitRetryDecision::SafeDeadlineElapsed);
        assert_eq!(next_terminal_commit_retry(5, 1_005, 1_005), HlsTerminalCommitRetryDecision::SafeDeadlineElapsed);
        assert_eq!(next_terminal_commit_retry(5, 1_006, 1_005), HlsTerminalCommitRetryDecision::SafeDeadlineElapsed);
    }

    #[test]
    fn unavailable_after_owner_failure_retains_the_exclusive_media_tail_deadline() {
        let mut command = retry_command("failed-closed-unavailable", 10_000, 1, 1);
        let media_tail_deadline = command
            .preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch();
        assert_eq!(command.latest_safe_terminal_commit_at_ms(), media_tail_deadline);

        command.decision = HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(
            HlsTerminalTailCompatibility::TerminalMediaNotReady,
        );
        assert_eq!(command.latest_safe_terminal_commit_at_ms(), media_tail_deadline);
    }

    #[test]
    fn failed_media_tail_owner_cannot_be_replaced_at_the_exclusive_deadline() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let command = retry_command("failed-owner-replacement", 10_000, 1, 1);
        let key = command.key.clone();
        let session = Arc::clone(&command.session);
        let session_incarnation = command.session_incarnation;
        let preparation = command.preparation.clone();
        let failed_at_ms = command.latest_safe_terminal_commit_at_ms();
        let HlsTerminalCommitSubmissionDecision::Attempt { owner_token, .. } =
            coordinator.submit(command, 1)
        else {
            panic!("initial media-tail owner expected");
        };
        coordinator.fail_owner(&key, owner_token, HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);

        let replacement = HlsTerminalCommitCommand {
            key,
            session,
            session_incarnation,
            preparation,
            decision: HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(
                HlsTerminalTailCompatibility::TerminalMediaNotReady,
            ),
            media_guard: None,
            asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_for_test(None),
            cancellation_epoch: 0,
            submission_token: HlsTerminalCommitOwnerToken::for_test(2),
        };
        assert!(matches!(
            coordinator.submit(replacement, failed_at_ms),
            HlsTerminalCommitSubmissionDecision::Failed(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed)
        ));
    }

    #[test]
    fn hls_terminal_commit_fail_closed_handoff_has_real_retry_time_before_deadline() {
        let deadline_ms = 10_000_u64;
        let handoff_ms = terminal_commit_retry_handoff_budget_ms();
        let first_attempt_at_ms = deadline_ms.saturating_sub(handoff_ms);

        assert_eq!(
            handoff_ms,
            HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS
                .saturating_add(HLS_TERMINAL_COMMIT_EXCLUSIVE_DEADLINE_GUARD_MS)
        );
        assert!(handoff_ms < terminal_commit_retry_schedule_budget_ms());
        assert!(matches!(
            next_terminal_commit_retry(1, first_attempt_at_ms, deadline_ms),
            HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed: 2 }
                if retry_at_ms > first_attempt_at_ms && retry_at_ms < deadline_ms
        ));
    }

    #[test]
    fn hls_terminal_commit_retry_budget_fits_every_attempt_before_the_exclusive_deadline() {
        let started_at_ms = 5_000_u64;
        let budget_ms = terminal_commit_retry_schedule_budget_ms();
        let deadline_ms = started_at_ms.saturating_add(budget_ms);
        let mut attempts = 1;
        let mut attempt_at_ms = started_at_ms;
        while attempts < HLS_TERMINAL_COMMIT_MAX_ATTEMPTS {
            let HlsTerminalCommitRetryDecision::Schedule { attempts_completed, retry_at_ms } =
                next_terminal_commit_retry(attempts, attempt_at_ms, deadline_ms)
            else {
                panic!("derived acquisition budget must fit the complete bounded retry schedule");
            };
            assert!(retry_at_ms > attempt_at_ms);
            assert!(retry_at_ms < deadline_ms);
            attempts = attempts_completed;
            attempt_at_ms = retry_at_ms;
        }
        assert_eq!(
            next_terminal_commit_retry(attempts, attempt_at_ms, deadline_ms),
            HlsTerminalCommitRetryDecision::AttemptsExhausted
        );
        assert!(
            budget_ms
                <= u64::from(HLS_TERMINAL_COMMIT_MAX_ATTEMPTS)
                    .saturating_mul(HLS_TERMINAL_COMMIT_MAX_BACKOFF_MS)
                    .saturating_add(1)
        );
    }

    #[test]
    fn hls_terminal_commit_retry_exhaustion_outcomes_remain_distinct() {
        assert_eq!(
            next_terminal_commit_retry(HLS_TERMINAL_COMMIT_MAX_ATTEMPTS, 1_000, 5_000),
            HlsTerminalCommitRetryDecision::AttemptsExhausted
        );
        assert_ne!(HlsTerminalCommitOutcome::RetryAttemptsExhausted, HlsTerminalCommitOutcome::RetryCapacityExceeded);
        assert_ne!(HlsTerminalCommitOutcome::RetryWorkerUnavailable, HlsTerminalCommitOutcome::RetryCapacityExceeded);
        assert_ne!(HlsTerminalCommitOutcome::RetryWorkerUnavailable, HlsTerminalCommitOutcome::RetryAttemptsExhausted);
    }

    #[test]
    fn hls_terminal_commit_submission_weaker_command_tightens_deadline_without_replacing_owner() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("deadline-tighten", 300, 1, 1);
        let key = initial.key.clone();
        let (_worker, _, _) = submit_and_schedule(&coordinator, initial, 5, 120, 100);
        let before = coordinator.owner_snapshot(&key).expect("initial retry owner");

        assert!(matches!(
            coordinator.submit(retry_command("deadline-tighten", 200, 2, 1), 101),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms: 120 }
        ));
        let after = coordinator.owner_snapshot(&key).expect("tightened retry owner");
        assert_eq!(after.effective_deadline_ms, 200);
        assert_eq!(
            after.command_submission_token.as_u64(),
            1,
            "the stronger existing command remains bound"
        );
        assert_eq!(after.scheduling_sequence, before.scheduling_sequence);
    }

    #[test]
    fn hls_terminal_commit_submission_equal_evidence_never_extends_deadline() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("deadline-no-extension", 200, 1, 1);
        let key = initial.key.clone();
        let session = Arc::clone(&initial.session);
        let (_worker, _, _) = submit_and_schedule(&coordinator, initial, 3, 120, 100);

        assert!(matches!(
            coordinator.submit(retry_command("deadline-no-extension", 400, 2, 1), 101),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms: 120 }
        ));
        assert_eq!(
            coordinator.owner_snapshot(&key).map(|owner| owner.effective_deadline_ms),
            Some(200)
        );
        let mut newer = retry_command("deadline-no-extension", 500, 3, 2);
        newer.session = session;
        assert!(matches!(
            coordinator.submit(newer, 102),
            HlsTerminalCommitSubmissionDecision::Attempt { owner_token, .. } if owner_token.as_u64() == 3
        ));
        let owner = coordinator.owner_snapshot(&key).expect("newer command replaces existing binding");
        assert_eq!(owner.command_submission_token.as_u64(), 3);
        assert_eq!(owner.effective_deadline_ms, 200);
    }

    #[test]
    fn hls_terminal_commit_submission_deadline_tightening_preserves_attempt_progress() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("deadline-attempts", 300, 1, 1);
        let key = initial.key.clone();
        let (_worker, _, _) = submit_and_schedule(&coordinator, initial, 11, 250, 100);

        assert!(matches!(
            coordinator.submit(retry_command("deadline-attempts", 200, 2, 1), 150),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms: 199 }
        ));
        let owner = coordinator.owner_snapshot(&key).expect("tightened retry owner");
        assert_eq!(owner.attempts_completed, 11);
        assert_eq!(owner.scheduled_at_ms, 199);
    }

    #[test]
    fn hls_terminal_commit_retry_scheduled_at_tightened_deadline_is_not_run() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("deadline-schedule", 300, 1, 1);
        let key = initial.key.clone();
        let (worker, _, _) = submit_and_schedule(&coordinator, initial, 3, 250, 100);

        assert!(matches!(
            coordinator.submit(retry_command("deadline-schedule", 200, 2, 1), 150),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms: 199 }
        ));
        assert!(matches!(coordinator.next_action(worker, 200), HlsTerminalCommitRetryAction::Stop));
        assert_eq!(
            coordinator.owner_snapshot(&key).and_then(|owner| owner.terminal_failure),
            Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed)
        );
    }

    #[test]
    fn hls_terminal_commit_submission_tightening_invalidates_attempt_without_rebinding_token() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let initial = retry_command("deadline-in-flight", 300, 1, 1);
        let key = initial.key.clone();
        let (worker, _, _) = submit_and_schedule(&coordinator, initial, 3, 100, 90);
        let HlsTerminalCommitRetryAction::Due { owner_token, .. } = coordinator.next_action(worker, 100) else {
            panic!("initial retry must be due");
        };

        assert!(matches!(
            coordinator.submit(retry_command("deadline-in-flight", 100, 2, 1), 100),
            HlsTerminalCommitSubmissionDecision::Failed(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed)
        ));
        assert!(coordinator.with_current_owner(&key, owner_token, |_, _| ()).is_none());
        let owner = coordinator.owner_snapshot(&key).expect("failed-closed owner remains typed");
        assert_eq!(owner.owner_token, owner_token);
        assert_eq!(owner.command_submission_token, owner_token);
        assert_eq!(owner.terminal_failure, Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed));
    }

    #[test]
    fn hls_terminal_commit_retry_reschedule_advances_equal_due_peer_first() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let first = retry_command("fairness-a", 300, 1, 1);
        let first_key = first.key.clone();
        let (worker, _, _) = submit_and_schedule(&coordinator, first, 2, 100, 90);
        let second = retry_command("fairness-b", 300, 2, 1);
        let second_key = second.key.clone();
        let HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token, .. } =
            coordinator.submit(second, 90)
        else {
            panic!("second lease owns an independent submission");
        };
        assert!(matches!(
            coordinator.schedule_current(&command.key, owner_token, 2, 100, 90),
            HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token: None }
        ));

        let HlsTerminalCommitRetryAction::Due { key, owner_token, .. } = coordinator.next_action(worker, 100) else {
            panic!("first owner must be due");
        };
        assert_eq!(key, first_key);
        coordinator.reschedule(&key, owner_token, 3, 100);
        let HlsTerminalCommitRetryAction::Due { key, .. } = coordinator.next_action(worker, 100) else {
            panic!("equal-due peer must advance after reschedule");
        };
        assert_eq!(key, second_key);
    }

    #[test]
    fn hls_terminal_commit_rescheduled_owner_moves_behind_equal_due_peers() {
        let mut state = HlsTerminalCommitRetryState::default();
        let first_sequence = next_terminal_commit_scheduling_sequence(&mut state);
        let peer_sequence = next_terminal_commit_scheduling_sequence(&mut state);
        let rescheduled_sequence = next_terminal_commit_scheduling_sequence(&mut state);

        assert!(first_sequence < peer_sequence);
        assert!(peer_sequence < rescheduled_sequence);
        assert!(terminal_commit_schedule_cmp(1_000, peer_sequence, 1_000, rescheduled_sequence).is_lt());
        assert!(terminal_commit_schedule_cmp(999, rescheduled_sequence, 1_000, peer_sequence).is_lt());
    }

    #[test]
    fn hls_terminal_commit_worker_early_exit_has_bounded_generation_safe_restarts() {
        let mut state = HlsTerminalCommitRetryState::default();
        let first = claim_terminal_commit_worker(&mut state).expect("initial worker token");
        assert!(claim_terminal_commit_worker(&mut state).is_none());
        let HlsTerminalCommitWorkerFinished::Restart { mut worker_token } =
            finish_terminal_commit_worker(&mut state, first, HlsTerminalCommitWorkerOwnedWork::Remaining)
        else {
            panic!("remaining owner must request a controlled restart");
        };
        assert_ne!(first, worker_token);
        assert_eq!(
            finish_terminal_commit_worker(&mut state, first, HlsTerminalCommitWorkerOwnedWork::Remaining),
            HlsTerminalCommitWorkerFinished::StaleWorker
        );

        for _ in 1..HLS_TERMINAL_COMMIT_MAX_WORKER_RESTARTS {
            let HlsTerminalCommitWorkerFinished::Restart { worker_token: next } =
                finish_terminal_commit_worker(&mut state, worker_token, HlsTerminalCommitWorkerOwnedWork::Remaining)
            else {
                panic!("restart budget should still be available");
            };
            worker_token = next;
        }
        assert_eq!(
            finish_terminal_commit_worker(&mut state, worker_token, HlsTerminalCommitWorkerOwnedWork::Remaining,),
            HlsTerminalCommitWorkerFinished::RestartBudgetExhausted
        );
        assert!(state.active_worker.is_none());
        assert!(claim_terminal_commit_worker_for_registration(&mut state).is_none());

        state.worker_restarts = 0;
        let drained = claim_terminal_commit_worker(&mut state).expect("drain worker token");
        assert_eq!(
            finish_terminal_commit_worker(&mut state, drained, HlsTerminalCommitWorkerOwnedWork::Drained),
            HlsTerminalCommitWorkerFinished::Stopped
        );
        assert_eq!(state.worker_restarts, 0);

        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let coordinator_token = {
            let mut coordinator_state = coordinator.lock_state();
            claim_terminal_commit_worker(&mut coordinator_state).expect("coordinator worker token")
        };
        assert_eq!(coordinator.early_worker_exit_for_test(coordinator_token), HlsTerminalCommitWorkerFinished::Stopped);
    }

    #[test]
    fn hls_terminal_commit_worker_exit_releases_only_its_claimed_owner() {
        let coordinator = HlsTerminalCommitRetryCoordinator::default();
        let first = retry_command("worker-owned", 300, 1, 1);
        let (worker, first_key, first_token) = submit_and_schedule(&coordinator, first, 2, 100, 90);
        let second = retry_command("client-owned", 300, 2, 1);
        let second_key = second.key.clone();
        assert!(matches!(
            coordinator.submit(second, 90),
            HlsTerminalCommitSubmissionDecision::Attempt { owner_token, .. } if owner_token.as_u64() == 2
        ));

        assert!(matches!(
            coordinator.next_action(worker, 100),
            HlsTerminalCommitRetryAction::Due { key, owner_token, .. }
                if key == first_key && owner_token == first_token
        ));
        assert!(coordinator.owner_snapshot(&first_key).expect("worker owner").in_flight);
        assert!(coordinator.owner_snapshot(&second_key).expect("client owner").in_flight);

        assert!(matches!(
            coordinator.early_worker_exit_for_test(worker),
            HlsTerminalCommitWorkerFinished::Restart { .. }
        ));
        assert!(!coordinator.owner_snapshot(&first_key).expect("released worker owner").in_flight);
        assert!(
            coordinator
                .owner_snapshot(&second_key)
                .expect("unrelated client owner")
                .in_flight
        );
    }
}
