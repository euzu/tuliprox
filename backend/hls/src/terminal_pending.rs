use super::{
    prepared_terminal_bundle::HlsPreparedTerminalBundleKey,
    runtime_custom_tail::{HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason},
    session_store::HlsSessionIncarnation,
    terminal_commit::HlsTerminalAssetRevisionGuard,
    HlsAccessLeaseId, ProxySessionId,
};
use std::{future::Future, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

const HLS_TERMINAL_PENDING_CAPACITY: usize = 256;

/// Frozen identity of one terminal-media wait. The exact bundle key embeds the
/// asset revision and fingerprint; lease identity is frozen by issued-at,
/// admission, manifest, and cursor generations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsTerminalPendingOwnerKey {
    pub session_incarnation: HlsSessionIncarnation,
    pub proxy_session_id: ProxySessionId,
    pub lease_id: HlsAccessLeaseId,
    pub lease_issued_at_ms: u64,
    pub expected_admission_generation: u64,
    pub manifest_snapshot_generation: u64,
    pub cursor_generation: u64,
    pub decision_generation: u64,
    pub reason: HlsRuntimeCustomTailReason,
    pub bundle_key: HlsPreparedTerminalBundleKey,
    pub latest_safe_commit_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HlsTerminalPendingOwnerSlot {
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
}

impl HlsTerminalPendingOwnerKey {
    fn slot(&self) -> HlsTerminalPendingOwnerSlot {
        HlsTerminalPendingOwnerSlot { proxy_session_id: self.proxy_session_id.clone(), lease_id: self.lease_id.clone() }
    }

    fn supersedes(&self, current: &Self, asset_guard: &HlsTerminalAssetRevisionGuard) -> bool {
        if self.slot() != current.slot() {
            return false;
        }
        if self.reason != current.reason {
            return false;
        }
        if self.session_incarnation != current.session_incarnation {
            return self.session_incarnation > current.session_incarnation;
        }
        if self.lease_issued_at_ms != current.lease_issued_at_ms {
            return self.lease_issued_at_ms > current.lease_issued_at_ms;
        }
        if self.decision_generation != current.decision_generation {
            return self.decision_generation > current.decision_generation
                && self.bundle_asset_is_current_if_changed(current, asset_guard);
        }

        let generations_are_not_older = self.expected_admission_generation >= current.expected_admission_generation
            && self.manifest_snapshot_generation >= current.manifest_snapshot_generation
            && self.cursor_generation >= current.cursor_generation;
        let generation_advanced = self.expected_admission_generation > current.expected_admission_generation
            || self.manifest_snapshot_generation > current.manifest_snapshot_generation
            || self.cursor_generation > current.cursor_generation;
        if generations_are_not_older && generation_advanced {
            return self.bundle_asset_is_current_if_changed(current, asset_guard);
        }

        let generations_match = self.expected_admission_generation == current.expected_admission_generation
            && self.manifest_snapshot_generation == current.manifest_snapshot_generation
            && self.cursor_generation == current.cursor_generation;
        if !generations_match {
            return false;
        }
        if self.bundle_key == current.bundle_key {
            return self.latest_safe_commit_at_ms < current.latest_safe_commit_at_ms;
        }
        let shape_matches = self.bundle_key.target_duration_ms == current.bundle_key.target_duration_ms
            && self.bundle_key.segment_count == current.bundle_key.segment_count;
        shape_matches && self.bundle_asset_is_current_if_changed(current, asset_guard)
    }

    fn bundle_asset_is_current_if_changed(&self, current: &Self, asset_guard: &HlsTerminalAssetRevisionGuard) -> bool {
        if self.bundle_key.asset == current.bundle_key.asset {
            return true;
        }
        asset_guard.authorizes_current_asset(HlsRuntimeCustomTailAssetIdentity {
            reason: self.reason,
            media: self.bundle_key.asset,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsTerminalPendingRegistration {
    Scheduled,
    AlreadyOwned,
    Superseded,
    CapacityExceeded,
    RuntimeUnavailable,
}

struct HlsTerminalPendingEntry {
    key: HlsTerminalPendingOwnerKey,
    owner_token: u64,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct HlsTerminalPendingState {
    owners: std::collections::HashMap<HlsTerminalPendingOwnerSlot, HlsTerminalPendingEntry>,
    next_owner_token: u64,
}

pub struct HlsTerminalPendingCoordinator {
    state: std::sync::Mutex<HlsTerminalPendingState>,
    task_capacity: Arc<Semaphore>,
}

impl Default for HlsTerminalPendingCoordinator {
    fn default() -> Self { Self::with_capacity(HLS_TERMINAL_PENDING_CAPACITY) }
}

impl HlsTerminalPendingCoordinator {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(HlsTerminalPendingState::default()),
            task_capacity: Arc::new(Semaphore::new(capacity)),
        }
    }

    pub fn register<Work, WorkFuture>(
        self: &Arc<Self>,
        mut key: HlsTerminalPendingOwnerKey,
        asset_guard: &HlsTerminalAssetRevisionGuard,
        work: Work,
    ) -> HlsTerminalPendingRegistration
    where
        Work: FnOnce(HlsTerminalPendingOwnership) -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = ()> + Send + 'static,
    {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return HlsTerminalPendingRegistration::RuntimeUnavailable;
        };
        let slot = key.slot();
        let mut state = self.lock_state();
        if let Some(current) = state.owners.get(&slot) {
            if current.key == key {
                return HlsTerminalPendingRegistration::AlreadyOwned;
            }
            if !key.supersedes(&current.key, asset_guard) {
                return HlsTerminalPendingRegistration::Superseded;
            }
            key.latest_safe_commit_at_ms = key.latest_safe_commit_at_ms.min(current.key.latest_safe_commit_at_ms);
        }
        let Ok(task_permit) = Arc::clone(&self.task_capacity).try_acquire_owned() else {
            return HlsTerminalPendingRegistration::CapacityExceeded;
        };
        let Some(owner_token) = state.next_owner_token.checked_add(1) else {
            return HlsTerminalPendingRegistration::CapacityExceeded;
        };
        state.next_owner_token = owner_token;
        if let Some(replaced) = state.owners.remove(&slot) {
            replaced.cancellation.cancel();
        }
        let cancellation = CancellationToken::new();
        state.owners.insert(
            slot,
            HlsTerminalPendingEntry { key: key.clone(), owner_token, cancellation: cancellation.clone() },
        );
        drop(state);

        let ownership = HlsTerminalPendingOwnership { coordinator: Arc::clone(self), key, owner_token, cancellation };
        let completion = HlsTerminalPendingCompletion { ownership: ownership.clone(), _task_permit: task_permit };
        drop(runtime.spawn(async move {
            work(ownership).await;
            drop(completion);
        }));
        HlsTerminalPendingRegistration::Scheduled
    }

    pub fn cancel_lease(&self, lease_id: &HlsAccessLeaseId) {
        let mut state = self.lock_state();
        let mut removed = Vec::new();
        state.owners.retain(|slot, owner| {
            if &slot.lease_id == lease_id {
                removed.push(owner.cancellation.clone());
                false
            } else {
                true
            }
        });
        drop(state);
        for cancellation in removed {
            cancellation.cancel();
        }
    }

    pub fn cancel_session(&self, proxy_session_id: &ProxySessionId) {
        let mut state = self.lock_state();
        let mut removed = Vec::new();
        state.owners.retain(|slot, owner| {
            if &slot.proxy_session_id == proxy_session_id {
                removed.push(owner.cancellation.clone());
                false
            } else {
                true
            }
        });
        drop(state);
        for cancellation in removed {
            cancellation.cancel();
        }
    }

    pub fn clear(&self) {
        let mut state = self.lock_state();
        let owners = std::mem::take(&mut state.owners);
        drop(state);
        for owner in owners.into_values() {
            owner.cancellation.cancel();
        }
    }

    fn is_current(&self, key: &HlsTerminalPendingOwnerKey, owner_token: u64) -> bool {
        self.lock_state()
            .owners
            .get(&key.slot())
            .is_some_and(|owner| owner.owner_token == owner_token && owner.key == *key)
    }

    fn complete(&self, key: &HlsTerminalPendingOwnerKey, owner_token: u64) {
        let slot = key.slot();
        let mut state = self.lock_state();
        if state.owners.get(&slot).is_some_and(|owner| owner.owner_token == owner_token && owner.key == *key) {
            state.owners.remove(&slot);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn owner_count(&self) -> usize { self.lock_state().owners.len() }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, HlsTerminalPendingState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone)]
pub struct HlsTerminalPendingOwnership {
    coordinator: Arc<HlsTerminalPendingCoordinator>,
    key: HlsTerminalPendingOwnerKey,
    owner_token: u64,
    cancellation: CancellationToken,
}

impl HlsTerminalPendingOwnership {
    pub fn is_current(&self) -> bool {
        !self.cancellation.is_cancelled() && self.coordinator.is_current(&self.key, self.owner_token)
    }

    /// Returns the frozen deadline after applying the no-extension rule during
    /// owner supersession. Pending work must use this value instead of a
    /// deadline captured before registration.
    pub fn latest_safe_commit_at_ms(&self) -> u64 { self.key.latest_safe_commit_at_ms }

    pub async fn cancelled(&self) { self.cancellation.cancelled().await }
}

struct HlsTerminalPendingCompletion {
    ownership: HlsTerminalPendingOwnership,
    _task_permit: OwnedSemaphorePermit,
}

impl Drop for HlsTerminalPendingCompletion {
    fn drop(&mut self) { self.ownership.coordinator.complete(&self.ownership.key, self.ownership.owner_token); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_tail::HlsTerminalAssetIdentity;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{oneshot, Notify};

    fn key(
        proxy_session_id: &str,
        lease_id: &str,
        manifest_snapshot_generation: u64,
        latest_safe_commit_at_ms: u64,
    ) -> HlsTerminalPendingOwnerKey {
        let revision = manifest_snapshot_generation.saturating_add(1);
        HlsTerminalPendingOwnerKey {
            session_incarnation: HlsSessionIncarnation::for_test(1),
            proxy_session_id: ProxySessionId(proxy_session_id.to_string()),
            lease_id: HlsAccessLeaseId(lease_id.to_string()),
            lease_issued_at_ms: 10,
            expected_admission_generation: 20,
            manifest_snapshot_generation,
            cursor_generation: 30,
            decision_generation: 40,
            reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
            bundle_key: HlsPreparedTerminalBundleKey {
                asset: HlsTerminalAssetIdentity {
                    revision,
                    fingerprint: [u8::try_from(revision).unwrap_or(u8::MAX); 32],
                },
                target_duration_ms: 6_000,
                segment_count: 2,
            },
            latest_safe_commit_at_ms,
        }
    }

    fn register<Work, WorkFuture>(
        coordinator: &Arc<HlsTerminalPendingCoordinator>,
        key: HlsTerminalPendingOwnerKey,
        work: Work,
    ) -> HlsTerminalPendingRegistration
    where
        Work: FnOnce(HlsTerminalPendingOwnership) -> WorkFuture + Send + 'static,
        WorkFuture: Future<Output = ()> + Send + 'static,
    {
        let asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(key.bundle_key.asset));
        coordinator.register(key, &asset_guard, work)
    }

    async fn wait_for_owner_count(coordinator: &HlsTerminalPendingCoordinator, expected: usize) {
        for _ in 0..32 {
            if coordinator.owner_count() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(coordinator.owner_count(), expected);
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_exact_key_has_exactly_one_owner() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(2));
        let starts = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let (started_tx, started_rx) = oneshot::channel();

        let registration = {
            let release = Arc::clone(&release);
            let starts = Arc::clone(&starts);
            register(&coordinator, key("session", "lease", 1, 100), move |_| async move {
                starts.fetch_add(1, Ordering::AcqRel);
                assert!(started_tx.send(()).is_ok());
                release.notified().await;
            })
        };
        assert_eq!(registration, HlsTerminalPendingRegistration::Scheduled);
        assert!(started_rx.await.is_ok());

        let duplicate_starts = Arc::clone(&starts);
        assert_eq!(
            register(&coordinator, key("session", "lease", 1, 100), move |_| async move {
                duplicate_starts.fetch_add(1, Ordering::AcqRel);
            }),
            HlsTerminalPendingRegistration::AlreadyOwned
        );
        assert_eq!(starts.load(Ordering::Acquire), 1);
        assert_eq!(coordinator.owner_count(), 1);

        release.notify_one();
        wait_for_owner_count(&coordinator, 0).await;
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_capacity_is_bounded_and_typed() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(1));
        let release = Arc::new(Notify::new());
        let (started_tx, started_rx) = oneshot::channel();

        let registration = {
            let release = Arc::clone(&release);
            register(&coordinator, key("session-a", "lease-a", 1, 100), move |_| async move {
                assert!(started_tx.send(()).is_ok());
                release.notified().await;
            })
        };
        assert_eq!(registration, HlsTerminalPendingRegistration::Scheduled);
        assert!(started_rx.await.is_ok());
        assert_eq!(
            register(&coordinator, key("session-b", "lease-b", 1, 100), |_| async {}),
            HlsTerminalPendingRegistration::CapacityExceeded
        );
        assert_eq!(coordinator.owner_count(), 1);

        release.notify_one();
        wait_for_owner_count(&coordinator, 0).await;
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_new_snapshot_cancels_superseded_owner() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(2));
        let (stale_started_tx, stale_started_rx) = oneshot::channel();
        let (stale_cancelled_tx, stale_cancelled_rx) = oneshot::channel();
        let fresh_release = Arc::new(Notify::new());

        assert_eq!(
            register(&coordinator, key("session", "lease", 1, 100), move |ownership| async move {
                assert!(stale_started_tx.send(()).is_ok());
                ownership.cancelled().await;
                assert!(stale_cancelled_tx.send(ownership.is_current()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert!(stale_started_rx.await.is_ok());

        let registration = {
            let fresh_release = Arc::clone(&fresh_release);
            let mut fresh_key = key("session", "lease", 2, 100);
            fresh_key.decision_generation = 41;
            register(&coordinator, fresh_key, move |_| async move {
                fresh_release.notified().await;
            })
        };
        assert_eq!(registration, HlsTerminalPendingRegistration::Scheduled);
        assert_eq!(stale_cancelled_rx.await, Ok(false));
        assert_eq!(coordinator.owner_count(), 1);

        fresh_release.notify_one();
        wait_for_owner_count(&coordinator, 0).await;
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_cancellation_revokes_ownership() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(1));
        let lease_id = HlsAccessLeaseId("lease".to_string());
        let (started_tx, started_rx) = oneshot::channel();
        let (cancelled_tx, cancelled_rx) = oneshot::channel();

        assert_eq!(
            register(&coordinator, key("session", &lease_id.0, 1, 100), move |ownership| async move {
                assert!(started_tx.send(()).is_ok());
                ownership.cancelled().await;
                assert!(cancelled_tx.send(ownership.is_current()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert!(started_rx.await.is_ok());

        coordinator.cancel_lease(&lease_id);
        assert_eq!(cancelled_rx.await, Ok(false));
        assert_eq!(coordinator.owner_count(), 0);
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_completion_releases_owner_and_capacity() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(1));
        let (release_tx, release_rx) = oneshot::channel();

        assert_eq!(
            register(&coordinator, key("session-a", "lease-a", 1, 100), move |_| async move {
                assert!(release_rx.await.is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert_eq!(coordinator.owner_count(), 1);
        assert!(release_tx.send(()).is_ok());
        wait_for_owner_count(&coordinator, 0).await;

        let (second_release_tx, second_release_rx) = oneshot::channel();
        assert_eq!(
            register(&coordinator, key("session-b", "lease-b", 1, 100), move |_| async move {
                assert!(second_release_rx.await.is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert!(second_release_tx.send(()).is_ok());
        wait_for_owner_count(&coordinator, 0).await;
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_supersession_never_extends_safe_deadline() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(2));
        let (stale_started_tx, stale_started_rx) = oneshot::channel();
        let (stale_cancelled_tx, stale_cancelled_rx) = oneshot::channel();

        assert_eq!(
            register(&coordinator, key("session", "lease", 1, 100), move |ownership| async move {
                assert!(stale_started_tx.send(()).is_ok());
                ownership.cancelled().await;
                assert!(stale_cancelled_tx.send(()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert!(stale_started_rx.await.is_ok());

        assert_eq!(
            register(&coordinator, key("session", "lease", 1, 1_000), |_| async {}),
            HlsTerminalPendingRegistration::Superseded
        );

        let (observed_deadline_tx, observed_deadline_rx) = oneshot::channel();
        let (fresh_release_tx, fresh_release_rx) = oneshot::channel();
        assert_eq!(
            register(&coordinator, key("session", "lease", 2, 1_000), move |ownership| async move {
                assert!(observed_deadline_tx.send(ownership.latest_safe_commit_at_ms()).is_ok());
                assert!(fresh_release_rx.await.is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert_eq!(observed_deadline_rx.await, Ok(100));
        assert!(stale_cancelled_rx.await.is_ok());
        assert_eq!(coordinator.owner_count(), 1);

        assert!(fresh_release_tx.send(()).is_ok());
        wait_for_owner_count(&coordinator, 0).await;
    }

    #[tokio::test]
    async fn hls_terminal_commit_pending_current_asset_supersedes_and_delayed_stale_asset_cannot_revert_owner() {
        let coordinator = Arc::new(HlsTerminalPendingCoordinator::with_capacity(2));
        let (stale_started_tx, stale_started_rx) = oneshot::channel();
        let (stale_cancelled_tx, stale_cancelled_rx) = oneshot::channel();

        let mut stale_key = key("session", "lease", 1, 100);
        stale_key.bundle_key.asset = HlsTerminalAssetIdentity { revision: 900, fingerprint: [9; 32] };
        let stale_asset = stale_key.bundle_key.asset;
        assert_eq!(
            register(&coordinator, stale_key.clone(), move |ownership| async move {
                assert!(stale_started_tx.send(()).is_ok());
                ownership.cancelled().await;
                assert!(stale_cancelled_tx.send(ownership.is_current()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        assert!(stale_started_rx.await.is_ok());

        let mut current_key = key("session", "lease", 1, 1_000);
        current_key.bundle_key.asset = HlsTerminalAssetIdentity { revision: 3, fingerprint: [3; 32] };
        let current_asset = current_key.bundle_key.asset;
        let current_asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(current_asset));
        let (observed_deadline_tx, observed_deadline_rx) = oneshot::channel();
        let (current_release_tx, current_release_rx) = oneshot::channel();
        assert_eq!(
            coordinator.register(current_key, &current_asset_guard, move |ownership| async move {
                assert!(observed_deadline_tx.send(ownership.latest_safe_commit_at_ms()).is_ok());
                assert!(current_release_rx.await.is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );

        assert_eq!(observed_deadline_rx.await, Ok(100));
        assert_eq!(stale_cancelled_rx.await, Ok(false));
        assert_eq!(coordinator.owner_count(), 1);

        let delayed_stale_guard = HlsTerminalAssetRevisionGuard::new(Some(stale_asset), move || Some(current_asset));
        assert_eq!(
            coordinator.register(stale_key, &delayed_stale_guard, |_| async {}),
            HlsTerminalPendingRegistration::Superseded
        );
        assert_eq!(coordinator.owner_count(), 1);

        assert!(current_release_tx.send(()).is_ok());
        wait_for_owner_count(&coordinator, 0).await;
    }
}
