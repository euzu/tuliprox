use super::{
    evaluate_lease_reserve, manifest_acceptance_episode_status, ready_segment_repair_prewarm_candidates,
    safe_proxy_session_id, safe_session_key, GarbageCollectionPolicy, HlsAccessLease, HlsAccessLeaseActivation,
    HlsAccessLeaseDenialOutcome, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseSessionSnapshot,
    HlsAccessLeaseState, HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsAccessLeaseTouch,
    HlsAvailabilityReevaluationCoordinator, HlsAvailabilityReevaluationOwnerKey, HlsCacheMetrics,
    HlsCriticalHandoffStateAccess, HlsCurrentProxySessionAccess, HlsEstimatedRecoveryCompletionAtMs,
    HlsFiniteTailTrigger, HlsGarbageCollector, HlsLeaseCutoverTiming, HlsLeaseManifestPublicationGuard,
    HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason, HlsLeaseManifestSnapshot,
    HlsLeaseReserveInput, HlsLifecycleManager, HlsManifestAcceptanceEpisodeStatus, HlsManifestAcceptanceGeneration,
    HlsManifestDeliveryMode, HlsMapWorkerPool, HlsOriginSource, HlsPreparedTerminalBundleCache, HlsProxyManager,
    HlsProxyRuntimeConfig, HlsPublishedTransientResourceIds, HlsQosRegistry, HlsRecoveryPressureGuard,
    HlsRecoveryPressureGuardAccess, HlsRecoveryTriggerBudgetMs, HlsRepairPrewarmGuard, HlsRuntimeCustomTailReason,
    HlsRuntimePolicyRevocation, HlsRuntimePolicyRevocationOutcome, HlsSegmentCache, HlsSegmentRepairManager,
    HlsSegmentWorkerPool, HlsSessionHandle, HlsSessionKey, HlsSessionStore, HlsSessionStoreOutcome,
    HlsStandaloneCustomAccessStore, HlsStartupObservability, HlsTerminalCommitAcquisitionBudgetMs,
    HlsTerminalCommitClock, HlsTerminalCommitMediaGuard, HlsTerminalCommitPayload, HlsTerminalCommitRetryCoordinator,
    HlsTerminalCommitWindow, HlsTerminalLeaseDecision, HlsTerminalMediaPreparationKey,
    HlsTerminalMediaPreparationState, HlsTerminalMediaRequirementOrigin, HlsTerminalPendingCoordinator,
    HlsTerminalTailPreparation, HlsTerminalTailPreparationInput, HlsTransitionMarginMs, ProxySessionId,
    SegmentFetchPolicy, TransientManifestLeaseBinding, TransientResourceStore, HLS_STATE_CAS_LOCK_RETRIES,
};
use arc_swap::ArcSwap;
use log::{debug, error, info};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};
use tokio::sync::{RwLock, Semaphore};
use tuliprox_core::{model::HlsCacheConfig, utils::current_time_millis};

impl HlsTerminalCommitPayload {
    pub(super) fn into_parts(self) -> (HlsTerminalLeaseDecision, Option<HlsTerminalCommitMediaGuard>) {
        match self {
            Self::Tail { plan, media_guard } => (HlsTerminalLeaseDecision::Tail(plan), Some(media_guard)),
            Self::Unavailable(reason) => (HlsTerminalLeaseDecision::Unavailable(reason), None),
            Self::UnavailableAfterOwnerFailure(reason) => {
                (HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason), None)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsRecoveryExecutionState {
    Idle,
    InFlight { estimated_completion_at: HlsEstimatedRecoveryCompletionAtMs },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HlsAcceptanceRecoverySnapshot {
    pub(super) expected_generation: HlsManifestAcceptanceGeneration,
    pub(super) status: HlsManifestAcceptanceEpisodeStatus,
    pub(super) recovery: HlsRecoveryExecutionState,
    pub(super) required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    pub(super) terminal_media_preparation: HlsTerminalMediaPreparationState,
}

pub(super) fn hls_estimated_recovery_completion_at(
    recovery: HlsRecoveryExecutionState,
) -> Option<HlsEstimatedRecoveryCompletionAtMs> {
    match recovery {
        HlsRecoveryExecutionState::Idle => None,
        HlsRecoveryExecutionState::InFlight { estimated_completion_at } => Some(estimated_completion_at),
    }
}

pub(super) fn hls_acceptance_recovery_snapshot(
    session: &super::super::HlsSession,
    now_ms: u64,
) -> HlsAcceptanceRecoverySnapshot {
    let expected_generation = session.origin_control.acceptance_generation;
    let status = manifest_acceptance_episode_status(
        session.origin_control.acceptance_episode.as_ref(),
        expected_generation,
        now_ms,
    );
    let matching_episode =
        session.origin_control.acceptance_episode.as_ref().filter(|episode| episode.generation == expected_generation);
    let recovery = match (status, matching_episode) {
        (HlsManifestAcceptanceEpisodeStatus::InFlight { .. }, Some(episode)) => episode
            .estimated_recovery_completion_at(expected_generation, now_ms)
            .map_or(HlsRecoveryExecutionState::Idle, |estimated_completion_at| HlsRecoveryExecutionState::InFlight {
                estimated_completion_at,
            }),
        (
            HlsManifestAcceptanceEpisodeStatus::Missing
            | HlsManifestAcceptanceEpisodeStatus::Expired { .. }
            | HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. }
            | HlsManifestAcceptanceEpisodeStatus::Committed { .. }
            | HlsManifestAcceptanceEpisodeStatus::Superseded { .. },
            _,
        )
        | (HlsManifestAcceptanceEpisodeStatus::InFlight { .. }, None) => HlsRecoveryExecutionState::Idle,
    };
    let (required_terminal_media_key, terminal_media_preparation) =
        matching_episode.map_or((None, HlsTerminalMediaPreparationState::Failed { key: None }), |episode| {
            let timing = episode.timing();
            (timing.required_terminal_media_key, timing.terminal_media_preparation)
        });
    HlsAcceptanceRecoverySnapshot {
        expected_generation,
        status,
        recovery,
        required_terminal_media_key,
        terminal_media_preparation,
    }
}

fn hls_pending_manifest_follow_up_window_ms(target_duration: Option<u32>) -> u64 {
    let target_duration_secs = u64::from(target_duration.unwrap_or(15)).max(1);
    target_duration_secs.saturating_mul(2_000).max(10_000)
}

fn hls_pending_manifest_follow_up_deadline(now_ms: u64, target_duration: Option<u32>) -> HlsAccessLeasePendingDeadline {
    HlsAccessLeasePendingDeadline::FollowUp {
        deadline_ms: now_ms.saturating_add(hls_pending_manifest_follow_up_window_ms(target_duration)),
    }
}

impl HlsProxyManager {
    pub fn with_hls_cache_config(config: &HlsCacheConfig) -> Self {
        Self::with_hls_cache_config_and_secret(config, &[])
    }

    pub fn with_hls_cache_config_and_secret(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self::with_hls_cache_config_and_secret_enabled(config, rewrite_secret, true)
    }

    pub(super) fn with_hls_cache_config_and_secret_enabled(
        config: &HlsCacheConfig,
        rewrite_secret: &[u8],
        enabled: bool,
    ) -> Self {
        let segment_fetch_policy = SegmentFetchPolicy::from_config(config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&config.cache_path)));
        segment_cache.update_cache_limits(config.cache_bytes.get(), config.cache_bytes_per_session.get());
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(config.segment_repair.clone()));
        let metrics = Arc::new(HlsCacheMetrics::default());
        let qos = Arc::new(HlsQosRegistry::default());
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        let lifecycle = Arc::new(HlsLifecycleManager::new());
        let account_overlap_cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let gc_policy = GarbageCollectionPolicy::from_config(config);
        let runtime_config = HlsProxyRuntimeConfig::from_config_with_enabled(config, rewrite_secret, enabled);
        let gc = Arc::new(HlsGarbageCollector::new_with_metrics(
            Arc::clone(&sessions),
            Arc::clone(&segment_cache),
            gc_policy.clone(),
            runtime_config.rewrite_secret_fingerprint.clone(),
            Arc::clone(&metrics),
        ));
        segment_cache.install_capacity_reclaimer(&gc);
        gc.install_access_leases(&access_leases);
        let availability_reevaluations = Arc::new(HlsAvailabilityReevaluationCoordinator::default());
        Self {
            sessions,
            segment_cache,
            segment_repair,
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::with_global_semaphore_metrics_and_availability(
                segment_fetch_policy.clone(),
                Arc::clone(&global_fetch_semaphore),
                Arc::clone(&access_leases),
                Arc::clone(&metrics),
                Some(Arc::clone(&availability_reevaluations)),
            )),
            map_worker_pool: Arc::new(HlsMapWorkerPool::with_global_semaphore_access_leases_and_availability(
                segment_fetch_policy.clone(),
                global_fetch_semaphore,
                Arc::clone(&access_leases),
                Some(Arc::clone(&availability_reevaluations)),
            )),
            progressive_budget: super::super::ProgressiveBudgetManager::new(runtime_config.startup.clone()),
            revision_store: Arc::new(super::super::SegmentRevisionStore::default()),
            revision_reconciliation_pending: AtomicBool::new(true),
            runtime_config: ArcSwap::from_pointee(runtime_config),
            transient_resources: Arc::new(TransientResourceStore::new()),
            access_leases,
            lifecycle,
            account_overlap_cooldowns,
            metrics,
            qos,
            gc,
            prepared_terminal_bundles: Arc::new(HlsPreparedTerminalBundleCache::new()),
            standalone_custom_access: Arc::new(HlsStandaloneCustomAccessStore::default()),
            terminal_commit_retries: Arc::new(HlsTerminalCommitRetryCoordinator::default()),
            terminal_pending: Arc::new(HlsTerminalPendingCoordinator::default()),
            availability_reevaluations,
            terminal_commit_clock: Arc::new(HlsTerminalCommitClock::default()),
            startup_observability: Arc::new(HlsStartupObservability::default()),
        }
    }

    pub fn sessions(&self) -> &Arc<HlsSessionStore> { &self.sessions }

    pub fn segment_cache(&self) -> &Arc<HlsSegmentCache> { &self.segment_cache }

    pub fn segment_repair(&self) -> &Arc<HlsSegmentRepairManager> { &self.segment_repair }

    pub fn startup_observability(&self) -> &Arc<HlsStartupObservability> { &self.startup_observability }

    pub fn spawn_access_lease_repair_prewarm(
        self: &Arc<Self>,
        session: HlsSessionHandle,
        lease_id: HlsAccessLeaseId,
        snapshot: HlsLeaseManifestSnapshot,
        snapshot_generation: u64,
    ) {
        if snapshot.delivery_mode != HlsManifestDeliveryMode::NormalCacheTimeline
            || snapshot.container != super::super::terminal_tail::HlsMediaContainer::MpegTs
        {
            return;
        }
        let candidate_limit = self.segment_repair.prewarm_candidate_limit();
        if candidate_limit == 0 {
            return;
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            let Some(lease) =
                manager.access_lease_response_snapshot(&lease_id, &proxy_session_id, current_time_millis()).await
            else {
                return;
            };
            let guard = HlsRepairPrewarmGuard::new(
                Arc::clone(&manager.access_leases),
                lease_id.clone(),
                proxy_session_id,
                lease.issued_at_ms,
                snapshot_generation,
            );
            manager.segment_repair.ensure_access_lease_window(lease_id.clone()).await;
            let candidates = {
                let session = session.read().await;
                ready_segment_repair_prewarm_candidates(&session, &lease_id, &snapshot, candidate_limit)
            };
            manager
                .segment_repair
                .spawn_ready_cache_prewarm(Arc::clone(&manager.segment_cache), candidates, guard)
                .await;
        });
    }

    pub fn segment_worker_pool(&self) -> &Arc<HlsSegmentWorkerPool> { &self.segment_worker_pool }

    pub fn map_worker_pool(&self) -> &Arc<HlsMapWorkerPool> { &self.map_worker_pool }

    /// The clock every time-dependent HLS scheduler reads.
    ///
    /// One clock, so a test that controls time controls all of it rather than
    /// some schedulers following the wall clock and others the test clock.
    pub(crate) fn now_ms(&self) -> u64 { self.terminal_commit_clock.now_ms() }

    /// Anchor the scheduling clock to tokio's, for `start_paused` tests.
    #[cfg(test)]
    pub(crate) fn follow_tokio_clock_for_test(&self, base_ms: u64) {
        self.terminal_commit_clock.follow_tokio_clock(base_ms);
    }

    pub fn transient_resources(&self) -> &Arc<TransientResourceStore> { &self.transient_resources }

    pub fn access_leases(&self) -> &Arc<RwLock<HlsAccessLeaseStore>> { &self.access_leases }

    pub fn metrics(&self) -> &Arc<HlsCacheMetrics> { &self.metrics }

    pub fn qos(&self) -> &Arc<HlsQosRegistry> { &self.qos }

    pub fn garbage_collector(&self) -> &Arc<HlsGarbageCollector> { &self.gc }

    pub(super) async fn clear_runtime_cache_state_for_cache_path_change(&self) {
        self.availability_reevaluations.clear();
        self.terminal_pending.clear();
        self.terminal_commit_retries.clear();
        self.sessions.clear().await;
        let removed_leases = self.access_leases.write().await.clear();
        self.standalone_custom_access.clear();
        self.terminal_commit_retries.clear();
        let removed_qos = self.qos.clear().await;
        self.segment_repair.clear_runtime_state().await;
        self.startup_observability.clear();
        debug!(
            "HLS cache runtime state cleared after cache path change: access_leases_removed={removed_leases} qos_access_leases_removed={removed_qos}"
        );
    }

    pub async fn prepare_access_lease(&self, lease: HlsAccessLease) {
        if !self.access_leases.write().await.prepare_access_lease(lease.clone()) {
            error!(
                "HLS access lease preparation rejected: lease={} proxy_session={} reason=availability_evidence_exhausted",
                super::super::safe_hls_access_lease_id(&lease.lease_id),
                safe_proxy_session_id(&lease.proxy_session_id)
            );
            return;
        }
        self.schedule_access_lease_validity(&lease).await;
    }

    pub async fn access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let (lease, still_stored) = {
            let mut access_leases = self.access_leases.write().await;
            let lease = access_leases.access_lease(lease_id, proxy_session_id, now_ms);
            let still_stored = access_leases.lease_state(lease_id, now_ms).is_some();
            (lease, still_stored)
        };
        if lease.is_none() && !still_stored {
            self.segment_repair.remove_access_lease_window(lease_id).await;
            self.startup_observability.remove_access_lease(lease_id);
            self.qos.remove_access_lease(lease_id).await;
        }
        lease
    }

    pub async fn access_lease_response_snapshot(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        self.access_leases.write().await.response_snapshot(lease_id, proxy_session_id, now_ms)
    }

    /// Runs the final Critical-Handoff revalidation and timeline commit under the
    /// established lease-store -> session lock order. No lock is held across an await.
    /// `LockBusy` reports contention only and is not a generation or lease invalidation.
    pub async fn with_critical_handoff_state<T>(
        &self,
        session_handle: &HlsSessionHandle,
        operation: impl FnOnce(&mut HlsAccessLeaseStore, &mut super::super::HlsSession) -> T,
    ) -> HlsCriticalHandoffStateAccess<T> {
        let mut operation = Some(operation);
        for attempt in 0..HLS_STATE_CAS_LOCK_RETRIES {
            let Ok(mut leases) = self.access_leases.try_write() else {
                if attempt.saturating_add(1) < HLS_STATE_CAS_LOCK_RETRIES {
                    tokio::task::yield_now().await;
                }
                continue;
            };
            let Ok(mut session) = session_handle.try_write() else {
                drop(leases);
                if attempt.saturating_add(1) < HLS_STATE_CAS_LOCK_RETRIES {
                    tokio::task::yield_now().await;
                }
                continue;
            };
            if let Some(operation) = operation.take() {
                return HlsCriticalHandoffStateAccess::Acquired(operation(&mut leases, &mut session));
            }
        }
        HlsCriticalHandoffStateAccess::LockBusy
    }

    /// Captures the immutable identity used to deduplicate a later autonomous
    /// availability evaluation. The session read is completed before the
    /// lease-store -> session transaction starts, so no lock order is inverted.
    pub async fn availability_reevaluation_owner_key(
        &self,
        session_handle: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
    ) -> Option<HlsAvailabilityReevaluationOwnerKey> {
        let session_incarnation = self.sessions.session_incarnation(session_handle)?;
        let availability_evidence_generation =
            self.access_leases.read().await.availability_evidence_generation(proxy_session_id);
        let session = session_handle.read().await;
        if session.proxy_session_id != *proxy_session_id || session.is_gc_marked_for_removal() {
            return None;
        }
        Some(HlsAvailabilityReevaluationOwnerKey {
            session_incarnation,
            proxy_session_id: proxy_session_id.clone(),
            origin_progress_generation: session.origin_control.progress_generation,
            media_readiness_generation: session.activity.media_readiness_generation,
            availability_evidence_generation,
        })
    }

    pub async fn availability_reevaluation_session_is_current(
        &self,
        session_handle: &HlsSessionHandle,
        owner_key: &HlsAvailabilityReevaluationOwnerKey,
    ) -> bool {
        if self.sessions.session_incarnation(session_handle) != Some(owner_key.session_incarnation) {
            return false;
        }
        self.sessions
            .get_by_proxy_session_id(&owner_key.proxy_session_id)
            .await
            .is_some_and(|current| Arc::ptr_eq(&current, session_handle))
    }

    /// Runs the refresh-start mutation only while the lease evidence, session
    /// index, concrete handle, incarnation, and recovery-pressure generations
    /// are all current. Cross-store lock order is Session Index -> optional
    /// Retry Owner -> Lease Store -> Session. Every acquisition here is
    /// non-blocking and the closure must not await or perform I/O.
    pub fn with_current_recovery_pressure_session<R>(
        &self,
        session_handle: &HlsSessionHandle,
        guard: &HlsRecoveryPressureGuard,
        operation: impl FnOnce(&mut super::super::HlsSession) -> R,
    ) -> HlsRecoveryPressureGuardAccess<R> {
        if self.sessions.session_incarnation(session_handle) != Some(guard.session_incarnation) {
            return HlsRecoveryPressureGuardAccess::Superseded;
        }
        let access = self.sessions.try_with_current_proxy_session(&guard.proxy_session_id, session_handle, || {
            let Ok(leases) = self.access_leases.try_read() else {
                return HlsRecoveryPressureGuardAccess::LockBusy;
            };
            if leases.availability_evidence_generation(&guard.proxy_session_id)
                != guard.availability_evidence_generation
            {
                return HlsRecoveryPressureGuardAccess::Superseded;
            }
            let Ok(mut session) = session_handle.try_write() else {
                return HlsRecoveryPressureGuardAccess::LockBusy;
            };
            if session.proxy_session_id != guard.proxy_session_id
                || session.origin_control.progress_generation != guard.origin_progress_generation
                || session.activity.media_readiness_generation != guard.media_readiness_generation
                || leases.availability_evidence_generation(&guard.proxy_session_id)
                    != guard.availability_evidence_generation
                || session.is_gc_marked_for_removal()
            {
                return HlsRecoveryPressureGuardAccess::Superseded;
            }
            HlsRecoveryPressureGuardAccess::Acquired(operation(&mut session))
        });
        match access {
            HlsCurrentProxySessionAccess::Acquired(access) => access,
            HlsCurrentProxySessionAccess::Superseded => HlsRecoveryPressureGuardAccess::Superseded,
            HlsCurrentProxySessionAccess::LockBusy => HlsRecoveryPressureGuardAccess::LockBusy,
        }
    }

    pub fn availability_reevaluations(&self) -> Arc<HlsAvailabilityReevaluationCoordinator> {
        Arc::clone(&self.availability_reevaluations)
    }

    pub fn notify_session_evidence_changed(&self, proxy_session_id: &ProxySessionId) -> bool {
        self.availability_reevaluations.notify_session_evidence_changed(proxy_session_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn hold_access_lease_store_for_test(&self) -> tokio::sync::OwnedRwLockWriteGuard<HlsAccessLeaseStore> {
        Arc::clone(&self.access_leases).write_owned().await
    }

    pub async fn prepare_access_lease_manifest_publication(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsLeaseManifestPublicationGuard> {
        self.access_leases.write().await.prepare_manifest_publication(lease_id, proxy_session_id, now_ms)
    }

    pub async fn commit_access_lease_manifest_publication_with_resources(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        snapshot: HlsLeaseManifestSnapshot,
        published_transient_resource_ids: HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        let finalized_manifest_generation = snapshot.finalized_transient_manifest_generation;
        let lease_issued_at_ms = expected.lease_issued_at_ms();
        let Some(finalized_manifest_generation) = finalized_manifest_generation else {
            let session = self.sessions.get_by_proxy_session_id(proxy_session_id).await;
            // The lease-store write lock keeps the previous lease membership stable while the
            // session read produces an immutable merged membership snapshot. The snapshot can then
            // be committed without reopening the lease-store transaction.
            let mut leases = self.access_leases.write().await;
            let published_transient_resource_ids = if let Some(session) = session.as_ref() {
                let session = session.read().await;
                if session.proxy_session_id == *proxy_session_id {
                    leases.published_transient_resource_ids(lease_id).map_or(
                        published_transient_resource_ids.clone(),
                        |previous| {
                            session.transient.merge_current_published_resource_ids(
                                previous,
                                published_transient_resource_ids,
                                now_ms,
                            )
                        },
                    )
                } else {
                    published_transient_resource_ids
                }
            } else {
                published_transient_resource_ids
            };
            return leases.commit_manifest_publication_with_resources(
                lease_id,
                proxy_session_id,
                expected,
                snapshot,
                published_transient_resource_ids,
                now_ms,
            );
        };
        let Some(current_session) = self.sessions.hold_current_proxy_session(proxy_session_id).await else {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::ManifestGenerationUnavailable,
            );
        };
        // The index guard prevents replacement while the established index -> lease store -> session
        // lock order publishes the exact snapshot and binds its finalized source generation.
        let session = Arc::clone(current_session.session());
        let mut leases = self.access_leases.write().await;
        let mut session = session.write().await;
        if session.proxy_session_id != *proxy_session_id || session.is_gc_marked_for_removal() {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::ManifestGenerationUnavailable,
            );
        }
        if !session.transient.has_finalized_manifest_generation(finalized_manifest_generation) {
            return HlsLeaseManifestPublicationOutcome::Rejected(
                HlsLeaseManifestPublicationRejectReason::ManifestGenerationUnavailable,
            );
        }
        let outcome = leases.commit_manifest_publication_with_resources(
            lease_id,
            proxy_session_id,
            expected,
            snapshot,
            published_transient_resource_ids,
            now_ms,
        );
        if outcome.is_committed() {
            let bound = session.transient.bind_finalized_manifest_generation(TransientManifestLeaseBinding::new(
                lease_id.clone(),
                lease_issued_at_ms,
                finalized_manifest_generation,
            ));
            debug_assert!(bound, "validated finalized manifest generation must remain bindable");
        }
        drop(session);
        drop(leases);
        drop(current_session);
        outcome
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn commit_access_lease_manifest_publication(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        self.commit_access_lease_manifest_publication_with_resources(
            lease_id,
            proxy_session_id,
            expected,
            snapshot,
            HlsPublishedTransientResourceIds::default(),
            now_ms,
        )
        .await
    }

    pub async fn prepare_access_lease_runtime_custom_tail(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> Option<HlsTerminalTailPreparation> {
        if reason.trigger_class()
            != super::super::runtime_custom_tail::HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover
        {
            return None;
        }
        let current_session = self.sessions.get_by_proxy_session_id(proxy_session_id).await?;
        if !Arc::ptr_eq(&current_session, session) {
            return None;
        }
        let lease = self.access_lease_response_snapshot(lease_id, proxy_session_id, now_ms).await?;
        if lease.playback_mode != super::super::terminal_tail::HlsLeasePlaybackMode::Live {
            return None;
        }
        let manifest = lease.last_manifest_snapshot.as_ref()?;
        let (
            origin_progress_generation,
            media_readiness_generation,
            origin_epoch,
            last_media_progress_at_ms,
            reserve,
            recovery_snapshot,
        ) = {
            let session = session.read().await;
            if session.proxy_session_id != *proxy_session_id || session.is_gc_marked_for_removal() {
                return None;
            }
            let ready_timeline = session.ready_timeline_snapshot(
                lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
                now_ms,
            );
            let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
                manifest,
                cursor: &lease.playback_cursor,
                ready_timeline: &ready_timeline,
                now_ms,
                playback_rate_guard_milli: super::super::HLS_PLAYBACK_RATE_GUARD_MILLI,
                recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(0),
                origin_path_degraded: true,
                recovery_committed: false,
            });
            (
                session.origin_control.progress_generation,
                session.activity.media_readiness_generation,
                session.origin_control.origin_epoch,
                session.origin_control.last_media_progress_at_ms,
                reserve,
                hls_acceptance_recovery_snapshot(&session, now_ms),
            )
        };
        let technical_commit_budget_ms = self
            .origin_manifest_timeout_ms()
            .max(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
        let cutover_timing = HlsLeaseCutoverTiming::from_reserve(
            now_ms,
            technical_commit_budget_ms,
            HlsTransitionMarginMs::from_millis(0),
            None,
        );
        self.access_leases.read().await.prepare_terminal_tail(
            lease_id,
            proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::RuntimePolicy(reason),
                expected_manifest_snapshot_generation: manifest.snapshot_generation,
                expected_cursor_generation: lease.playback_cursor.cursor_generation,
                origin_progress_generation,
                media_readiness_generation,
                origin_epoch,
                last_media_progress_at_ms,
                expected_acceptance_generation: recovery_snapshot.expected_generation,
                terminal_media_requirement_origin: HlsTerminalMediaRequirementOrigin::CutoverSnapshot,
                cutover_timing,
                commit_window: HlsTerminalCommitWindow::CutoverDue,
                required_terminal_media_key: None,
                terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
                reserve,
            },
        )
    }

    pub async fn begin_runtime_policy_revocation(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> HlsRuntimePolicyRevocationOutcome {
        self.access_leases.write().await.begin_runtime_policy_revocation(lease_id, proxy_session_id, reason, now_ms)
    }

    pub async fn fail_runtime_policy_revocation(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        token: &HlsRuntimePolicyRevocation,
    ) -> HlsAccessLeaseDenialOutcome {
        let outcome =
            self.access_leases.write().await.fail_runtime_policy_revocation(lease_id, proxy_session_id, token);
        if matches!(outcome, HlsAccessLeaseDenialOutcome::Ended { .. }) {
            self.terminal_pending.cancel_lease(lease_id);
            self.terminal_commit_retries.cancel_lease(lease_id);
        }
        outcome
    }

    pub async fn update_access_lease_origin_acquire_policy(
        &self,
        lease_id: &HlsAccessLeaseId,
        connection_kind: tuliprox_session::ConnectionKind,
        priority: i8,
    ) -> Option<HlsAccessLease> {
        self.access_leases.write().await.update_origin_acquire_policy(lease_id, connection_kind, priority)
    }

    pub async fn activate_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> HlsAccessLeaseActivation {
        let activation =
            self.access_leases.write().await.activate_access_lease(lease_id, proxy_session_id, now_ms, timing);
        if let HlsAccessLeaseActivation::Activated { lease, previous_state } = &activation {
            if *previous_state == HlsAccessLeaseState::Pending {
                self.segment_repair.ensure_access_lease_window(lease.lease_id.clone()).await;
            } else if *previous_state == HlsAccessLeaseState::Idle {
                self.segment_repair.start_access_lease_window(lease.lease_id.clone()).await;
            }
            self.schedule_access_lease_activity(lease).await;
            self.schedule_access_lease_validity(lease).await;
        }
        activation
    }

    pub async fn touch_manifest_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        active_timing: Option<HlsAccessLeaseTiming>,
        pending_deadline: Option<HlsAccessLeasePendingDeadline>,
        ttl_ms: u64,
    ) -> HlsAccessLeaseTouch {
        let touch = self.access_leases.write().await.touch_manifest_access_lease(
            lease_id,
            proxy_session_id,
            now_ms,
            active_timing,
            pending_deadline,
            ttl_ms,
        );
        if let HlsAccessLeaseTouch::Touched { lease } = &touch {
            if lease.state == HlsAccessLeaseState::Activated {
                self.schedule_access_lease_activity(lease).await;
            }
            self.schedule_access_lease_validity(lease).await;
        }
        touch
    }

    pub async fn mark_pending_manifest_follow_up_for_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        target_duration: Option<u32>,
    ) -> bool {
        let deadline = hls_pending_manifest_follow_up_deadline(now_ms, target_duration);
        let lease = self.access_leases.write().await.mark_pending_manifest_follow_up_for_lease(
            lease_id,
            proxy_session_id,
            now_ms,
            deadline,
        );
        if let Some(lease) = lease {
            self.schedule_access_lease_validity(&lease).await;
            debug!(
                "HLS pending manifest lease shortened after manifest response: lease={} proxy_session={}",
                super::super::safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id)
            );
            true
        } else {
            false
        }
    }

    pub async fn mark_pending_manifest_follow_up_for_session(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        target_duration: Option<u32>,
    ) -> usize {
        let deadline = hls_pending_manifest_follow_up_deadline(now_ms, target_duration);
        let leases = self.access_leases.write().await.mark_pending_manifest_follow_up_for_session(
            proxy_session_id,
            now_ms,
            deadline,
        );
        for lease in &leases {
            self.schedule_access_lease_validity(lease).await;
        }
        leases.len()
    }

    pub async fn active_access_lease_count_for_session(&self, proxy_session_id: &ProxySessionId, now_ms: u64) -> usize {
        self.access_leases.write().await.active_access_lease_count_for_session(proxy_session_id, now_ms)
    }

    pub async fn has_usable_access_lease_for_session(&self, proxy_session_id: &ProxySessionId, now_ms: u64) -> bool {
        self.access_leases.write().await.has_usable_access_lease_for_session(proxy_session_id, now_ms)
    }

    pub async fn access_lease_session_snapshot(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseSessionSnapshot {
        self.access_leases.write().await.session_snapshot(proxy_session_id, now_ms)
    }

    pub async fn debug_state_summary(&self) -> String {
        let sessions = self.sessions.list_sessions().await;
        let access_leases = self.access_leases.read().await.len();
        let qos_access_leases = self.qos.len().await;
        let repair = self.segment_repair.stats().await;
        let mut segments = 0_usize;
        let mut maps = 0_usize;
        let mut transient_resources = 0_usize;
        let mut transient_objects = 0_usize;
        let mut active_origin_work = 0_usize;
        let mut active_segment_fetches = 0_usize;
        let mut active_map_fetches = 0_usize;
        for session in &sessions {
            let session = session.read().await;
            segments = segments.saturating_add(session.segments.len());
            maps = maps.saturating_add(session.maps.len());
            transient_resources = transient_resources.saturating_add(session.transient.resources.len());
            transient_objects = transient_objects.saturating_add(session.transient.object_cache.len());
            active_origin_work = active_origin_work
                .saturating_add(session.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire));
            active_segment_fetches = active_segment_fetches.saturating_add(session.active_segment_fetches);
            active_map_fetches = active_map_fetches.saturating_add(session.active_map_fetches);
        }
        format!(
            "sessions={} access_leases={} qos_access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_metadata={} repair_object_metadata={} repair_locks={} repair_watchdog_metadata={} repair_watchdog_locks={} segments={} maps={} transient_resources={} transient_objects={} active_origin_work={} active_segment_fetches={} active_map_fetches={}",
            sessions.len(),
            access_leases,
            qos_access_leases,
            repair.windows,
            repair.generations,
            repair.checked_candidates,
            repair.metadata,
            repair.object_metadata,
            repair.locks,
            repair.watchdog_metadata,
            repair.watchdog_locks,
            segments,
            maps,
            transient_resources,
            transient_objects,
            active_origin_work,
            active_segment_fetches,
            active_map_fetches
        )
    }

    pub async fn get_or_create_session(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> HlsSessionHandle {
        self.get_or_create_session_with_outcome(key, reverse_proxy_rewrite_secret, now_ms).await.0
    }

    pub async fn get_or_create_session_with_outcome(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let origin_source = HlsOriginSource::from_session_key(&key);
        self.get_or_create_session_with_source_and_outcome(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
            .await
    }

    pub async fn get_or_create_session_with_source_and_outcome(
        &self,
        key: HlsSessionKey,
        origin_source: HlsOriginSource,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let runtime = self.runtime_config.load();
        let startup = (runtime.startup.mode != shared::model::HlsStartupMode::Conservative
            && origin_source.archive_reference.is_none())
        .then(|| super::super::HlsSessionStartup {
            first_data_timeout_ms: runtime.segment_fetch_policy.origin_segment_timeout_ms,
            worker: Arc::downgrade(&self.segment_worker_pool),
            access_leases: Arc::clone(&self.access_leases),
            config: runtime.startup.clone(),
            revisions: std::collections::BTreeMap::new(),
            policy_fixed: false,
            store: Arc::clone(&self.revision_store),
            budget: Arc::clone(&self.progressive_budget),
        });
        drop(runtime);
        let (session, outcome) = self
            .sessions
            .get_or_create_session_with_startup(key, origin_source, reverse_proxy_rewrite_secret, now_ms, startup)
            .await;
        let (proxy_session_id, session_key) = {
            let session_guard = session.read().await;
            (safe_proxy_session_id(&session_guard.proxy_session_id), safe_session_key(&session_guard.key))
        };
        match outcome {
            HlsSessionStoreOutcome::Created => {
                self.metrics.record_session_created();
                info!("HLS session created: session={session_key} proxy_session={proxy_session_id}");
            }
            HlsSessionStoreOutcome::Reused => {
                self.metrics.record_session_reused();
                debug!("HLS session reused: session={session_key} proxy_session={proxy_session_id}");
            }
        }
        self.schedule_session_idle_for_handle(&session).await;
        session.write().await.configure_segment_prefetch_queue(self.segment_fetch_policy().max_prefetch_queue_depth);
        (session, outcome)
    }
}

impl Default for HlsProxyManager {
    fn default() -> Self { Self::new() }
}
