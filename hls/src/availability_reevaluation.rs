use super::{
    lease::HlsAvailabilityEvidenceGeneration, refresh::HlsPostRefreshAvailabilityReason,
    session_store::HlsSessionIncarnation, ProxySessionId,
};
use std::{future::Future, sync::Arc};
use tokio::sync::{watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

const HLS_AVAILABILITY_REEVALUATION_CAPACITY: usize = 256;

/// Identity of one immutable session state whose recovery pressure could not
/// be evaluated synchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsAvailabilityReevaluationOwnerKey {
    pub session_incarnation: HlsSessionIncarnation,
    pub proxy_session_id: ProxySessionId,
    pub origin_progress_generation: u64,
    pub media_readiness_generation: u64,
    pub availability_evidence_generation: HlsAvailabilityEvidenceGeneration,
}

/// Immutable session evidence authorizing one recovery-pressure refresh.
/// The refresh-start CAS validates every field before mutating session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsRecoveryPressureGuard {
    pub session_incarnation: HlsSessionIncarnation,
    pub proxy_session_id: ProxySessionId,
    pub origin_progress_generation: u64,
    pub media_readiness_generation: u64,
    pub availability_evidence_generation: HlsAvailabilityEvidenceGeneration,
}

impl HlsRecoveryPressureGuard {
    pub fn from_owner_key(owner_key: &HlsAvailabilityReevaluationOwnerKey) -> Self {
        Self {
            session_incarnation: owner_key.session_incarnation,
            proxy_session_id: owner_key.proxy_session_id.clone(),
            origin_progress_generation: owner_key.origin_progress_generation,
            media_readiness_generation: owner_key.media_readiness_generation,
            availability_evidence_generation: owner_key.availability_evidence_generation,
        }
    }
}

/// Result of the non-blocking session-index and recovery-evidence start CAS.
pub enum HlsRecoveryPressureGuardAccess<R> {
    Acquired(R),
    Superseded,
    LockBusy,
}

impl HlsAvailabilityReevaluationOwnerKey {
    fn supersedes(&self, current: &Self) -> bool {
        if self.proxy_session_id != current.proxy_session_id {
            return false;
        }
        if self.session_incarnation != current.session_incarnation {
            return self.session_incarnation > current.session_incarnation;
        }
        self.origin_progress_generation >= current.origin_progress_generation
            && self.media_readiness_generation >= current.media_readiness_generation
            && self.availability_evidence_generation >= current.availability_evidence_generation
            && (self.origin_progress_generation > current.origin_progress_generation
                || self.media_readiness_generation > current.media_readiness_generation
                || self.availability_evidence_generation > current.availability_evidence_generation)
    }
}

/// Bounded registration result. `Scheduled`, `AlreadyOwned`, and `Superseded`
/// prove that an authoritative owner exists for the proxy session at return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsAvailabilityReevaluationRegistration {
    Scheduled,
    AlreadyOwned,
    Superseded,
    CapacityExceeded,
    RuntimeUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsAvailabilityReevaluationMode {
    RecoveryPressure,
    PostRefresh(HlsPostRefreshAvailabilityReason),
}

impl HlsAvailabilityReevaluationMode {
    fn merge(self, incoming: Self) -> Self {
        match (self, incoming) {
            (Self::PostRefresh(current), Self::PostRefresh(incoming)) => {
                Self::PostRefresh(stronger_post_refresh_reason(current, incoming))
            }
            (Self::PostRefresh(reason), Self::RecoveryPressure)
            | (Self::RecoveryPressure, Self::PostRefresh(reason)) => Self::PostRefresh(reason),
            (Self::RecoveryPressure, Self::RecoveryPressure) => Self::RecoveryPressure,
        }
    }
}

const fn stronger_post_refresh_reason(
    current: HlsPostRefreshAvailabilityReason,
    incoming: HlsPostRefreshAvailabilityReason,
) -> HlsPostRefreshAvailabilityReason {
    match (current, incoming) {
        (HlsPostRefreshAvailabilityReason::HardManifestFailure, _)
        | (_, HlsPostRefreshAvailabilityReason::HardManifestFailure) => {
            HlsPostRefreshAvailabilityReason::HardManifestFailure
        }
        (
            HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
            HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
        ) => HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
    }
}

/// Result of transferring a running task to newer evidence for the same
/// proxy session. The task identity, cancellation token, and semaphore permit
/// remain unchanged across `Continue`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum HlsAvailabilityOwnerHandoffDecision {
    Continue { owner_key: HlsAvailabilityReevaluationOwnerKey },
    AlreadyCurrent,
    Superseded,
}

struct HlsAvailabilityReevaluationEntry {
    key: HlsAvailabilityReevaluationOwnerKey,
    mode: HlsAvailabilityReevaluationMode,
    owner_token: u64,
    cancellation: CancellationToken,
    wake: Arc<Notify>,
    observer_revision: u64,
    observer_tx: watch::Sender<u64>,
    rerun_requested: bool,
}

impl HlsAvailabilityReevaluationEntry {
    fn notify_observers(&mut self) {
        self.observer_revision = self.observer_revision.saturating_add(1);
        self.observer_tx.send_replace(self.observer_revision);
    }
}

#[derive(Default)]
struct HlsAvailabilityReevaluationState {
    owners: std::collections::HashMap<ProxySessionId, HlsAvailabilityReevaluationEntry>,
    next_owner_token: u64,
}

/// Bounded, per-session singleflight for autonomous availability evaluation.
///
/// The map retains only identity and cancellation state. The spawned bounded
/// task owns its refresh payload, preventing a manager -> request -> manager
/// reference cycle. Same-session evidence handoffs retain the existing task
/// and permit; cancellation retains the permit until the task exits.
pub struct HlsAvailabilityReevaluationCoordinator {
    state: std::sync::Mutex<HlsAvailabilityReevaluationState>,
    task_capacity: Arc<Semaphore>,
}

impl Default for HlsAvailabilityReevaluationCoordinator {
    fn default() -> Self { Self::with_capacity(HLS_AVAILABILITY_REEVALUATION_CAPACITY) }
}

impl HlsAvailabilityReevaluationCoordinator {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(HlsAvailabilityReevaluationState::default()),
            task_capacity: Arc::new(Semaphore::new(capacity)),
        }
    }

    pub fn register<Work, WorkFuture>(
        self: &Arc<Self>,
        key: HlsAvailabilityReevaluationOwnerKey,
        mode: HlsAvailabilityReevaluationMode,
        work: Work,
    ) -> HlsAvailabilityReevaluationRegistration
    where
        Work: FnOnce(HlsAvailabilityReevaluationOwnership) -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = ()> + Send + 'static,
    {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return HlsAvailabilityReevaluationRegistration::RuntimeUnavailable;
        };
        let mut state = self.lock_state();
        if let Some(current) = state.owners.get_mut(&key.proxy_session_id) {
            if current.key == key {
                current.mode = current.mode.merge(mode);
                current.rerun_requested = true;
                current.wake.notify_one();
                return HlsAvailabilityReevaluationRegistration::AlreadyOwned;
            }
            if !key.supersedes(&current.key) {
                return HlsAvailabilityReevaluationRegistration::Superseded;
            }
            // A newer demand for the same proxy session is owned by the
            // already-counted task. The worker will snapshot current session
            // evidence and atomically rekey itself without acquiring another
            // permit. Keeping one dirty bit preserves a bounded successor
            // cycle while coalescing repeated registrations.
            current.mode = current.mode.merge(mode);
            current.rerun_requested = true;
            current.wake.notify_one();
            return HlsAvailabilityReevaluationRegistration::AlreadyOwned;
        }
        let Ok(task_permit) = Arc::clone(&self.task_capacity).try_acquire_owned() else {
            return HlsAvailabilityReevaluationRegistration::CapacityExceeded;
        };
        let Some(owner_token) = state.next_owner_token.checked_add(1) else {
            return HlsAvailabilityReevaluationRegistration::CapacityExceeded;
        };
        state.next_owner_token = owner_token;
        if let Some(replaced) = state.owners.remove(&key.proxy_session_id) {
            replaced.cancellation.cancel();
        }
        let cancellation = CancellationToken::new();
        let wake = Arc::new(Notify::new());
        let (observer_tx, _) = watch::channel(0);
        state.owners.insert(
            key.proxy_session_id.clone(),
            HlsAvailabilityReevaluationEntry {
                key: key.clone(),
                mode,
                owner_token,
                cancellation: cancellation.clone(),
                wake: Arc::clone(&wake),
                observer_revision: 0,
                observer_tx,
                rerun_requested: false,
            },
        );
        drop(state);

        let ownership = HlsAvailabilityReevaluationOwnership {
            coordinator: Arc::clone(self),
            proxy_session_id: key.proxy_session_id,
            owner_token,
            cancellation,
            wake,
        };
        let completion =
            HlsAvailabilityReevaluationCompletion { ownership: ownership.clone(), _task_permit: task_permit };
        drop(runtime.spawn(async move {
            work(ownership).await;
            drop(completion);
        }));
        HlsAvailabilityReevaluationRegistration::Scheduled
    }

    pub fn cancel_session(&self, proxy_session_id: &ProxySessionId) {
        let removed = self.lock_state().owners.remove(proxy_session_id);
        if let Some(removed) = removed {
            removed.cancellation.cancel();
        }
    }

    /// Marks bounded same-session evidence dirty and wakes the existing owner
    /// without creating work, acquiring a permit, or changing its mode.
    pub fn notify_session_evidence_changed(&self, proxy_session_id: &ProxySessionId) -> bool {
        let mut state = self.lock_state();
        let Some(owner) = state.owners.get_mut(proxy_session_id) else {
            return false;
        };
        owner.rerun_requested = true;
        owner.wake.notify_one();
        owner.notify_observers();
        true
    }

    /// Subscribes before the caller rechecks lease/session state, preventing a
    /// completion between state evaluation and waiting from being missed.
    pub fn observe_owner(&self, proxy_session_id: &ProxySessionId) -> Option<HlsAvailabilityReevaluationObserver> {
        self.lock_state()
            .owners
            .get(proxy_session_id)
            .map(|owner| HlsAvailabilityReevaluationObserver { receiver: owner.observer_tx.subscribe() })
    }

    pub fn clear(&self) {
        let mut state = self.lock_state();
        let owners = std::mem::take(&mut state.owners);
        drop(state);
        for owner in owners.into_values() {
            owner.cancellation.cancel();
        }
    }

    fn is_current(&self, key: &HlsAvailabilityReevaluationOwnerKey, owner_token: u64) -> bool {
        self.lock_state()
            .owners
            .get(&key.proxy_session_id)
            .is_some_and(|owner| owner.owner_token == owner_token && owner.key == *key)
    }

    fn current_mode(
        &self,
        key: &HlsAvailabilityReevaluationOwnerKey,
        owner_token: u64,
    ) -> Option<HlsAvailabilityReevaluationMode> {
        self.lock_state()
            .owners
            .get(&key.proxy_session_id)
            .and_then(|owner| (owner.owner_token == owner_token && owner.key == *key).then_some(owner.mode))
    }

    fn abandon(&self, proxy_session_id: &ProxySessionId, owner_token: u64) {
        let mut state = self.lock_state();
        if state.owners.get(proxy_session_id).is_some_and(|owner| owner.owner_token == owner_token) {
            state.owners.remove(proxy_session_id);
        }
    }

    fn handoff_to(
        &self,
        expected_key: &HlsAvailabilityReevaluationOwnerKey,
        owner_token: u64,
        current_key: HlsAvailabilityReevaluationOwnerKey,
    ) -> HlsAvailabilityOwnerHandoffDecision {
        if expected_key.proxy_session_id != current_key.proxy_session_id {
            return HlsAvailabilityOwnerHandoffDecision::Superseded;
        }
        let mut state = self.lock_state();
        let Some(owner) = state.owners.get_mut(&expected_key.proxy_session_id) else {
            return HlsAvailabilityOwnerHandoffDecision::Superseded;
        };
        if owner.owner_token != owner_token || owner.key != *expected_key {
            return HlsAvailabilityOwnerHandoffDecision::Superseded;
        }
        if current_key == *expected_key {
            return HlsAvailabilityOwnerHandoffDecision::AlreadyCurrent;
        }
        if !current_key.supersedes(expected_key) {
            return HlsAvailabilityOwnerHandoffDecision::Superseded;
        }
        owner.key = current_key.clone();
        owner.notify_observers();
        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key: current_key }
    }

    fn finish_cycle(
        &self,
        key: &HlsAvailabilityReevaluationOwnerKey,
        owner_token: u64,
        reason: HlsAvailabilityReevaluationFinishReason,
    ) -> HlsAvailabilityReevaluationFinishDecision {
        let mut state = self.lock_state();
        let Some(owner) = state.owners.get_mut(&key.proxy_session_id) else {
            return HlsAvailabilityReevaluationFinishDecision::Superseded;
        };
        if owner.owner_token != owner_token || owner.key != *key {
            return HlsAvailabilityReevaluationFinishDecision::Superseded;
        }
        if reason.preserves_dirty_demand() && owner.rerun_requested {
            owner.rerun_requested = false;
            owner.notify_observers();
            return HlsAvailabilityReevaluationFinishDecision::StartSuccessor;
        }
        state.owners.remove(&key.proxy_session_id);
        HlsAvailabilityReevaluationFinishDecision::Complete
    }

    fn discard_superseded(&self, key: &HlsAvailabilityReevaluationOwnerKey, owner_token: u64) {
        let mut state = self.lock_state();
        if state
            .owners
            .get(&key.proxy_session_id)
            .is_some_and(|owner| owner.owner_token == owner_token && owner.key == *key)
        {
            state.owners.remove(&key.proxy_session_id);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn owner_count(&self) -> usize { self.lock_state().owners.len() }

    #[cfg(any(test, feature = "test-support"))]
    pub fn available_task_permits_for_test(&self) -> usize { self.task_capacity.available_permits() }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HlsAvailabilityReevaluationState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Request-side observation of one bounded Availability owner.
///
/// The sender is owned by the coordinator entry. Removing, cancelling, or
/// superseding that entry closes the receiver and wakes every request waiter.
pub struct HlsAvailabilityReevaluationObserver {
    receiver: watch::Receiver<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsAvailabilityReevaluationObservation {
    EvidenceChanged,
    OwnerFinished,
}

impl HlsAvailabilityReevaluationObserver {
    pub async fn changed(&mut self) -> HlsAvailabilityReevaluationObservation {
        match self.receiver.changed().await {
            Ok(()) => HlsAvailabilityReevaluationObservation::EvidenceChanged,
            Err(_) => HlsAvailabilityReevaluationObservation::OwnerFinished,
        }
    }
}

/// Cancellation and ownership proof passed only to the registered task.
#[derive(Clone)]
pub struct HlsAvailabilityReevaluationOwnership {
    coordinator: Arc<HlsAvailabilityReevaluationCoordinator>,
    proxy_session_id: ProxySessionId,
    owner_token: u64,
    cancellation: CancellationToken,
    wake: Arc<Notify>,
}

impl HlsAvailabilityReevaluationOwnership {
    pub fn is_current(&self, owner_key: &HlsAvailabilityReevaluationOwnerKey) -> bool {
        !self.cancellation.is_cancelled() && self.coordinator.is_current(owner_key, self.owner_token)
    }

    pub async fn cancelled(&self) { self.cancellation.cancelled().await }

    pub fn current_mode(
        &self,
        owner_key: &HlsAvailabilityReevaluationOwnerKey,
    ) -> Option<HlsAvailabilityReevaluationMode> {
        self.coordinator.current_mode(owner_key, self.owner_token)
    }

    pub async fn wake_requested(&self) { self.wake.notified().await }

    /// Atomically transfers this running task to monotonic evidence for the
    /// same proxy session. No task or semaphore permit is created.
    pub fn handoff_to(
        &self,
        expected_key: &HlsAvailabilityReevaluationOwnerKey,
        current_key: HlsAvailabilityReevaluationOwnerKey,
    ) -> HlsAvailabilityOwnerHandoffDecision {
        self.coordinator.handoff_to(expected_key, self.owner_token, current_key)
    }

    /// Atomically closes one bounded cycle. A non-superseded dirty owner starts
    /// exactly one successor cycle with a fresh budget; supersession discards
    /// stale demand. An identical registration racing this operation either
    /// marks the retained owner dirty or observes the removed slot.
    pub fn finish_cycle(
        &self,
        owner_key: &HlsAvailabilityReevaluationOwnerKey,
        reason: HlsAvailabilityReevaluationFinishReason,
    ) -> HlsAvailabilityReevaluationFinishDecision {
        self.coordinator.finish_cycle(owner_key, self.owner_token, reason)
    }

    /// Discards stale dirty demand without allowing it to start a successor
    /// cycle after this owner's evidence has been superseded.
    pub fn discard_superseded(&self, owner_key: &HlsAvailabilityReevaluationOwnerKey) {
        self.coordinator.discard_superseded(owner_key, self.owner_token);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsAvailabilityReevaluationFinishReason {
    Evaluated,
    CycleBudgetExhausted,
}

impl HlsAvailabilityReevaluationFinishReason {
    fn preserves_dirty_demand(self) -> bool { matches!(self, Self::Evaluated | Self::CycleBudgetExhausted) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsAvailabilityReevaluationFinishDecision {
    Complete,
    StartSuccessor,
    Superseded,
}

struct HlsAvailabilityReevaluationCompletion {
    ownership: HlsAvailabilityReevaluationOwnership,
    _task_permit: OwnedSemaphorePermit,
}

impl Drop for HlsAvailabilityReevaluationCompletion {
    fn drop(&mut self) {
        // Safety net for task cancellation or panic. Normal exits call
        // `finish_cycle` first. A cancelled task cannot safely retain a dirty
        // owner without its request payload, so releasing the slot lets the
        // next real demand install fresh bounded work.
        self.ownership.coordinator.abandon(&self.ownership.proxy_session_id, self.ownership.owner_token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn key(progress: u64, readiness: u64) -> HlsAvailabilityReevaluationOwnerKey {
        HlsAvailabilityReevaluationOwnerKey {
            session_incarnation: HlsSessionIncarnation::for_test(1),
            proxy_session_id: ProxySessionId("availability-owner".to_string()),
            origin_progress_generation: progress,
            media_readiness_generation: readiness,
            availability_evidence_generation: HlsAvailabilityEvidenceGeneration::for_test(1),
        }
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_coalesces_same_generation_pressure_without_lost_wakeup() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(2));
        let owner_key = key(1, 1);
        let evaluated = Arc::new(Notify::new());
        let complete = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let evaluations = Arc::new(AtomicUsize::new(0));
        let recovery_pressure = Arc::new(AtomicBool::new(false));
        let recovery_starts = Arc::new(AtomicUsize::new(0));
        let registration = {
            let evaluated = Arc::clone(&evaluated);
            let complete = Arc::clone(&complete);
            let finished = Arc::clone(&finished);
            let evaluations = Arc::clone(&evaluations);
            let recovery_pressure = Arc::clone(&recovery_pressure);
            let recovery_starts = Arc::clone(&recovery_starts);
            let task_owner_key = owner_key.clone();
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    loop {
                        evaluations.fetch_add(1, Ordering::AcqRel);
                        if recovery_pressure.load(Ordering::Acquire) {
                            recovery_starts.fetch_add(1, Ordering::AcqRel);
                        }
                        evaluated.notify_one();
                        complete.notified().await;
                        match ownership
                            .finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated)
                        {
                            HlsAvailabilityReevaluationFinishDecision::StartSuccessor => {}
                            HlsAvailabilityReevaluationFinishDecision::Complete
                            | HlsAvailabilityReevaluationFinishDecision::Superseded => break,
                        }
                    }
                    finished.notify_one();
                },
            )
        };
        assert_eq!(registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        evaluated.notified().await;
        recovery_pressure.store(true, Ordering::Release);
        for _ in 0..32 {
            assert_eq!(
                coordinator.register(key(1, 1), HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {}),
                HlsAvailabilityReevaluationRegistration::AlreadyOwned
            );
        }
        assert_eq!(coordinator.owner_count(), 1);
        complete.notify_one();
        evaluated.notified().await;
        assert_eq!(evaluations.load(Ordering::Acquire), 2);
        assert_eq!(recovery_starts.load(Ordering::Acquire), 1);
        complete.notify_one();
        finished.notified().await;
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn production_evidence_wakes_existing_owner_without_changing_mode_or_capacity() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let proxy_session_id = owner_key.proxy_session_id.clone();
        let started = Arc::new(Notify::new());
        let task_started = Arc::clone(&started);
        let task_key = owner_key.clone();
        let (mode_tx, mode_rx) = tokio::sync::oneshot::channel();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::PostRefresh(HlsPostRefreshAvailabilityReason::HardManifestFailure,),
                move |ownership| async move {
                    task_started.notify_one();
                    ownership.wake_requested().await;
                    let _ = mode_tx.send(ownership.current_mode(&task_key));
                    let _ = ownership.finish_cycle(&task_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        started.notified().await;

        assert!(coordinator.notify_session_evidence_changed(&proxy_session_id));
        assert_eq!(
            mode_rx.await.expect("owner reports retained mode"),
            Some(HlsAvailabilityReevaluationMode::PostRefresh(HlsPostRefreshAvailabilityReason::HardManifestFailure,))
        );
        tokio::task::yield_now().await;
        assert_eq!(coordinator.owner_count(), 0);
        assert!(!coordinator.notify_session_evidence_changed(&proxy_session_id));
    }

    #[tokio::test]
    async fn availability_owner_evidence_change_wakes_every_request_observer() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let proxy_session_id = owner_key.proxy_session_id.clone();
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let task_release = Arc::clone(&release);
        let task_finished = Arc::clone(&finished);
        let task_key = owner_key.clone();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_release.notified().await;
                    let _ = ownership.finish_cycle(&task_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                    task_finished.notify_one();
                }
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let mut first = coordinator.observe_owner(&proxy_session_id).expect("first owner observation");
        let mut second = coordinator.observe_owner(&proxy_session_id).expect("second owner observation");
        let mut first_change = Box::pin(first.changed());
        let mut second_change = Box::pin(second.changed());
        assert!(matches!(futures::poll!(first_change.as_mut()), std::task::Poll::Pending));
        assert!(matches!(futures::poll!(second_change.as_mut()), std::task::Poll::Pending));

        assert!(coordinator.notify_session_evidence_changed(&proxy_session_id));

        assert_eq!(first_change.await, HlsAvailabilityReevaluationObservation::EvidenceChanged);
        assert_eq!(second_change.await, HlsAvailabilityReevaluationObservation::EvidenceChanged);
        release.notify_one();
        finished.notified().await;
    }

    #[tokio::test]
    async fn availability_owner_completion_after_subscription_cannot_be_missed() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let proxy_session_id = owner_key.proxy_session_id.clone();
        let release = Arc::new(Notify::new());
        let task_release = Arc::clone(&release);
        let task_key = owner_key.clone();
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_release.notified().await;
                    let _ = ownership.finish_cycle(&task_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                }
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        let mut observer = coordinator.observe_owner(&proxy_session_id).expect("owner observation");

        release.notify_one();

        assert_eq!(observer.changed().await, HlsAvailabilityReevaluationObservation::OwnerFinished);
        assert!(coordinator.observe_owner(&proxy_session_id).is_none());
    }

    #[tokio::test]
    // A handoff scenario: two generations, three notifies and the assertions
    // that order them. Splitting it would hide the sequence under test.
    #[allow(clippy::too_many_lines)]
    async fn hls_availability_reevaluation_capacity_one_owner_hands_off_to_current_generation() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let stale_key = key(10, 1);
        let current_key = key(11, 1);
        let started = Arc::new(Notify::new());
        let handoff = Arc::new(Notify::new());
        let handed_off = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let cycles = Arc::new(AtomicUsize::new(0));
        let unexpected_successor_tasks = Arc::new(AtomicUsize::new(0));
        let registration = {
            let task_stale_key = stale_key.clone();
            let task_current_key = current_key.clone();
            let started = Arc::clone(&started);
            let handoff = Arc::clone(&handoff);
            let handed_off = Arc::clone(&handed_off);
            let release = Arc::clone(&release);
            let finished = Arc::clone(&finished);
            let cycles = Arc::clone(&cycles);
            coordinator.register(
                stale_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    cycles.fetch_add(1, Ordering::AcqRel);
                    started.notify_one();
                    handoff.notified().await;
                    assert_eq!(
                        ownership.handoff_to(&task_stale_key, task_current_key.clone()),
                        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key: task_current_key.clone() }
                    );
                    let owner_key = task_current_key;
                    assert!(!ownership.is_current(&task_stale_key));
                    assert!(ownership.is_current(&owner_key));
                    ownership.discard_superseded(&task_stale_key);
                    assert!(ownership.is_current(&owner_key));
                    assert_eq!(
                        ownership.finish_cycle(&task_stale_key, HlsAvailabilityReevaluationFinishReason::Evaluated),
                        HlsAvailabilityReevaluationFinishDecision::Superseded
                    );
                    handed_off.notify_one();
                    release.notified().await;
                    assert_eq!(
                        ownership.finish_cycle(&owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated),
                        HlsAvailabilityReevaluationFinishDecision::StartSuccessor
                    );
                    cycles.fetch_add(1, Ordering::AcqRel);
                    assert_eq!(
                        ownership.finish_cycle(&owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated),
                        HlsAvailabilityReevaluationFinishDecision::Complete
                    );
                    finished.notify_one();
                },
            )
        };
        assert_eq!(registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        started.notified().await;
        let successor_registration = {
            let unexpected_successor_tasks = Arc::clone(&unexpected_successor_tasks);
            coordinator.register(
                current_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |_| async move {
                    unexpected_successor_tasks.fetch_add(1, Ordering::AcqRel);
                },
            )
        };
        assert_eq!(successor_registration, HlsAvailabilityReevaluationRegistration::AlreadyOwned);
        handoff.notify_one();
        handed_off.notified().await;
        assert_eq!(coordinator.owner_count(), 1);
        assert_eq!(unexpected_successor_tasks.load(Ordering::Acquire), 0);

        let mut other_key = key(1, 1);
        other_key.proxy_session_id = ProxySessionId("availability-owner-other".to_string());
        assert_eq!(
            coordinator.register(other_key.clone(), HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {}),
            HlsAvailabilityReevaluationRegistration::CapacityExceeded
        );
        release.notify_one();
        finished.notified().await;
        while coordinator.task_capacity.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
        let other_finished = Arc::new(Notify::new());
        let other_registration = {
            let other_finished = Arc::clone(&other_finished);
            let task_other_key = other_key.clone();
            coordinator.register(
                other_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    assert_eq!(
                        ownership.finish_cycle(&task_other_key, HlsAvailabilityReevaluationFinishReason::Evaluated,),
                        HlsAvailabilityReevaluationFinishDecision::Complete
                    );
                    other_finished.notify_one();
                },
            )
        };
        assert_eq!(other_registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        other_finished.notified().await;
        assert_eq!(cycles.load(Ordering::Acquire), 2);
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_capacity_is_typed_and_bounded() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let release = Arc::new(Notify::new());
        let registration = {
            let release = Arc::clone(&release);
            coordinator.register(key(1, 1), HlsAvailabilityReevaluationMode::RecoveryPressure, move |_| async move {
                release.notified().await;
            })
        };
        assert_eq!(registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        let mut other = key(1, 1);
        other.proxy_session_id = ProxySessionId("availability-owner-other".to_string());
        assert_eq!(
            coordinator.register(other, HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {}),
            HlsAvailabilityReevaluationRegistration::CapacityExceeded
        );
        assert_eq!(coordinator.owner_count(), 1);
        release.notify_waiters();
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_early_exit_releases_owner() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        assert_eq!(
            coordinator.register(key(1, 1), HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {}),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(coordinator.owner_count(), 0);
    }

    async fn assert_dirty_demand_starts_one_successor_cycle(finish_reason: HlsAvailabilityReevaluationFinishReason) {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let first_cycle = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let cycles = Arc::new(AtomicUsize::new(0));
        let registration = {
            let first_cycle = Arc::clone(&first_cycle);
            let finish = Arc::clone(&finish);
            let finished = Arc::clone(&finished);
            let cycles = Arc::clone(&cycles);
            let task_owner_key = owner_key.clone();
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    cycles.fetch_add(1, Ordering::AcqRel);
                    first_cycle.notify_one();
                    finish.notified().await;
                    assert_eq!(
                        ownership.finish_cycle(&task_owner_key, finish_reason),
                        HlsAvailabilityReevaluationFinishDecision::StartSuccessor
                    );
                    cycles.fetch_add(1, Ordering::AcqRel);
                    assert_eq!(
                        ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated,),
                        HlsAvailabilityReevaluationFinishDecision::Complete
                    );
                    finished.notify_one();
                },
            )
        };
        assert_eq!(registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        first_cycle.notified().await;
        for _ in 0..32 {
            assert_eq!(
                coordinator.register(key(1, 1), HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {}),
                HlsAvailabilityReevaluationRegistration::AlreadyOwned
            );
        }
        finish.notify_one();
        finished.notified().await;
        assert_eq!(cycles.load(Ordering::Acquire), 2);
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_dirty_demand_survives_deadline_cycle_exit() {
        assert_dirty_demand_starts_one_successor_cycle(HlsAvailabilityReevaluationFinishReason::CycleBudgetExhausted)
            .await;
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_dirty_demand_survives_attempt_cycle_exit() {
        assert_dirty_demand_starts_one_successor_cycle(HlsAvailabilityReevaluationFinishReason::Evaluated).await;
    }

    async fn assert_rekeyed_availability_owner_is_cancelled(clear_all: bool) {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let stale_key = key(1, 1);
        let mut current_key = key(1, 1);
        current_key.session_incarnation = HlsSessionIncarnation::for_test(2);
        let started = Arc::new(Notify::new());
        let handoff = Arc::new(Notify::new());
        let handed_off = Arc::new(Notify::new());
        let cancelled = Arc::new(Notify::new());
        let registration = {
            let task_stale_key = stale_key.clone();
            let task_current_key = current_key.clone();
            let started = Arc::clone(&started);
            let handoff = Arc::clone(&handoff);
            let handed_off = Arc::clone(&handed_off);
            let cancelled = Arc::clone(&cancelled);
            coordinator.register(
                stale_key,
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    started.notify_one();
                    handoff.notified().await;
                    assert_eq!(
                        ownership.handoff_to(&task_stale_key, task_current_key.clone()),
                        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key: task_current_key.clone() }
                    );
                    handed_off.notify_one();
                    ownership.cancelled().await;
                    assert!(!ownership.is_current(&task_current_key));
                    cancelled.notify_one();
                },
            )
        };
        assert_eq!(registration, HlsAvailabilityReevaluationRegistration::Scheduled);
        started.notified().await;
        handoff.notify_one();
        handed_off.notified().await;
        if clear_all {
            coordinator.clear();
        } else {
            coordinator.cancel_session(&current_key.proxy_session_id);
        }
        cancelled.notified().await;
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn hls_availability_reevaluation_rekeyed_owner_observes_cancel_and_clear() {
        assert_rekeyed_availability_owner_is_cancelled(false).await;
        assert_rekeyed_availability_owner_is_cancelled(true).await;
    }

    #[tokio::test]
    async fn post_refresh_registration_upgrades_existing_recovery_pressure_owner() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let started = Arc::new(Notify::new());
        let upgraded = Arc::new(Notify::new());
        let task_started = Arc::clone(&started);
        let task_upgraded = Arc::clone(&upgraded);
        let task_key = owner_key.clone();
        assert_eq!(
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    task_started.notify_one();
                    ownership.wake_requested().await;
                    assert_eq!(
                        ownership.current_mode(&task_key),
                        Some(HlsAvailabilityReevaluationMode::PostRefresh(
                            HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                        ))
                    );
                    task_upgraded.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        started.notified().await;
        assert_eq!(
            coordinator.register(
                owner_key,
                HlsAvailabilityReevaluationMode::PostRefresh(
                    HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                ),
                |_| async {},
            ),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        upgraded.notified().await;
    }

    #[tokio::test]
    async fn hard_failure_post_refresh_reason_is_not_downgraded_to_recovery_pressure() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let owner_key = key(1, 1);
        let release = Arc::new(Notify::new());
        let task_release = Arc::clone(&release);
        assert_eq!(
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::PostRefresh(HlsPostRefreshAvailabilityReason::HardManifestFailure),
                move |_| async move { task_release.notified().await },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        assert_eq!(
            coordinator.register(owner_key.clone(), HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {},),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        assert_eq!(
            coordinator.register(
                owner_key.clone(),
                HlsAvailabilityReevaluationMode::PostRefresh(
                    HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                ),
                |_| async {},
            ),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        let state = coordinator.lock_state();
        assert_eq!(
            state.owners.get(&owner_key.proxy_session_id).map(|owner| owner.mode),
            Some(HlsAvailabilityReevaluationMode::PostRefresh(HlsPostRefreshAvailabilityReason::HardManifestFailure,))
        );
        drop(state);
        release.notify_one();
    }

    #[tokio::test]
    async fn newer_owner_key_handoff_preserves_post_refresh_mode() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let stale_key = key(1, 1);
        let current_key = key(2, 2);
        let release = Arc::new(Notify::new());
        let task_release = Arc::clone(&release);
        let task_stale_key = stale_key.clone();
        let task_current_key = current_key.clone();
        assert_eq!(
            coordinator.register(
                stale_key.clone(),
                HlsAvailabilityReevaluationMode::RecoveryPressure,
                move |ownership| async move {
                    ownership.wake_requested().await;
                    assert_eq!(
                        ownership.handoff_to(&task_stale_key, task_current_key.clone()),
                        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key: task_current_key.clone() }
                    );
                    assert_eq!(
                        ownership.current_mode(&task_current_key),
                        Some(HlsAvailabilityReevaluationMode::PostRefresh(
                            HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                        ))
                    );
                    task_release.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        assert_eq!(
            coordinator.register(
                current_key,
                HlsAvailabilityReevaluationMode::PostRefresh(
                    HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                ),
                |_| async {},
            ),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        release.notified().await;
    }

    #[tokio::test]
    async fn media_progress_wakes_and_supersedes_sleeping_post_refresh_owner() {
        let coordinator = Arc::new(HlsAvailabilityReevaluationCoordinator::with_capacity(1));
        let stale_key = key(1, 1);
        let current_key = key(2, 2);
        let sleeping = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let task_sleeping = Arc::clone(&sleeping);
        let task_completed = Arc::clone(&completed);
        let task_stale_key = stale_key.clone();
        let task_current_key = current_key.clone();
        assert_eq!(
            coordinator.register(
                stale_key,
                HlsAvailabilityReevaluationMode::PostRefresh(
                    HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
                ),
                move |ownership| async move {
                    task_sleeping.notify_one();
                    ownership.wake_requested().await;
                    assert_eq!(
                        ownership.handoff_to(&task_stale_key, task_current_key.clone()),
                        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key: task_current_key.clone() }
                    );
                    assert_eq!(
                        ownership.finish_cycle(&task_current_key, HlsAvailabilityReevaluationFinishReason::Evaluated,),
                        HlsAvailabilityReevaluationFinishDecision::StartSuccessor
                    );
                    assert_eq!(
                        ownership.finish_cycle(&task_current_key, HlsAvailabilityReevaluationFinishReason::Evaluated,),
                        HlsAvailabilityReevaluationFinishDecision::Complete
                    );
                    task_completed.notify_one();
                },
            ),
            HlsAvailabilityReevaluationRegistration::Scheduled
        );
        sleeping.notified().await;
        assert_eq!(
            coordinator.register(current_key, HlsAvailabilityReevaluationMode::RecoveryPressure, |_| async {},),
            HlsAvailabilityReevaluationRegistration::AlreadyOwned
        );
        completed.notified().await;
        assert_eq!(coordinator.owner_count(), 0);
    }
}
