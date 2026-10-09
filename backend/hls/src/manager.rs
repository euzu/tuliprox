use super::{
    availability_reevaluation::{
        HlsAvailabilityReevaluationCoordinator, HlsAvailabilityReevaluationOwnerKey, HlsRecoveryPressureGuard,
        HlsRecoveryPressureGuardAccess,
    },
    build_rewrite_secret_fingerprint,
    cutover::{
        evaluate_terminal_cutover, HlsTerminalCutoverCapability, HlsTerminalCutoverDecision, HlsTerminalCutoverInput,
    },
    hls_ctx::HlsCtx,
    lease::{
        HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsAccessLeaseRemovalPreparation,
        HlsLeaseManifestPublicationGuard, HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason,
        HlsMediaLeaseIdentity, HlsRuntimePolicyRevocation, HlsRuntimePolicyRevocationOutcome,
        HlsTerminalMediaRequirementOrigin, HlsTerminalMediaRequirementSource, HlsTerminalTailPreparation,
        HlsTerminalTailPreparationInput,
    },
    manifest_acceptance::{
        manifest_acceptance_episode_status, HlsManifestAcceptanceEpisodeStatus, HlsManifestAcceptanceGeneration,
    },
    media_reserve::{
        evaluate_lease_reserve, HlsLeaseManifestSnapshot, HlsLeaseReserveInput, HlsLeaseReserveSnapshot,
        HlsManifestDeliveryMode,
    },
    prepared_terminal_bundle::{
        HlsPreparedTerminalBundleCache, HlsPreparedTerminalBundleKey, HlsPreparedTerminalBundleObservation,
        HlsPreparedTerminalBundleState,
    },
    recovery_timing::{
        HlsEstimatedRecoveryCompletionAtMs, HlsLeaseCutoverTiming, HlsRecoveryTriggerBudgetMs,
        HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow, HlsTerminalMediaPreparationKey,
        HlsTerminalMediaPreparationState, HlsTransitionMarginMs,
    },
    runtime_custom_tail::{
        HlsFiniteTailTrigger, HlsRuntimeCustomTailReason, HlsStandaloneCustomAccessEntry,
        HlsStandaloneCustomAccessStore, HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError,
    },
    safe_proxy_session_id, safe_session_key,
    segment_repair::{ready_segment_repair_prewarm_candidates, HlsRepairPrewarmGuard},
    session_store::HlsCurrentProxySessionAccess,
    terminal_commit::{
        next_terminal_commit_retry, spawn_terminal_commit_retry_worker, HlsTerminalAssetRevisionGuard,
        HlsTerminalAssetRevisionValidation, HlsTerminalCommitAttempt, HlsTerminalCommitClock, HlsTerminalCommitCommand,
        HlsTerminalCommitOutcome, HlsTerminalCommitOwnerKey, HlsTerminalCommitOwnerToken,
        HlsTerminalCommitRetryCoordinator, HlsTerminalCommitRetryDecision, HlsTerminalCommitRetryScheduleDecision,
        HlsTerminalCommitSubmissionDecision, HlsTerminalLeaseDecision,
    },
    terminal_pending::HlsTerminalPendingCoordinator,
    terminal_tail::{
        HlsTerminalCommitMediaGuard, HlsTerminalMediaAsset, HlsTerminalTailCompatibility, HlsTerminalTailPlan,
    },
    GarbageCollectionPolicy, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId,
    HlsAccessLeaseLifecycleSnapshot, HlsAccessLeasePendingDeadline, HlsAccessLeaseSessionSnapshot, HlsAccessLeaseState,
    HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsCacheMetrics, HlsExpiredSessionMarker,
    HlsExpiredSessionReason, HlsGarbageCollector, HlsLifecycleEvent, HlsLifecycleEventKey, HlsLifecycleManager,
    HlsMapWorkerPool, HlsOriginSource, HlsPlaybackRequestToken, HlsPublishedTransientResourceIds, HlsQosRegistry,
    HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle, HlsSessionKey, HlsSessionStore,
    HlsSessionStoreOutcome, HlsStartupObservability, HlsTerminalTailProtection, HlsTerminalTailProtectionInstall,
    HlsTerminalTailProtectionRemoval, ProxySessionId, SegmentFetchPolicy, TransientManifestLeaseBinding,
    TransientResourceStore,
};
use arc_swap::ArcSwap;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
};
use tokio::sync::RwLock;

/// Root runtime object for the future HLS cache proxy.
pub struct HlsProxyManager {
    sessions: Arc<HlsSessionStore>,
    segment_cache: Arc<HlsSegmentCache>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    segment_worker_pool: Arc<HlsSegmentWorkerPool>,
    map_worker_pool: Arc<HlsMapWorkerPool>,
    runtime_config: ArcSwap<HlsProxyRuntimeConfig>,
    transient_resources: Arc<TransientResourceStore>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    lifecycle: Arc<HlsLifecycleManager>,
    account_overlap_cooldowns: Arc<RwLock<HashMap<HlsAccountOverlapCooldownKey, HlsAccountOverlapCooldown>>>,
    metrics: Arc<HlsCacheMetrics>,
    qos: Arc<HlsQosRegistry>,
    gc: Arc<HlsGarbageCollector>,
    prepared_terminal_bundles: Arc<HlsPreparedTerminalBundleCache>,
    standalone_custom_access: Arc<HlsStandaloneCustomAccessStore>,
    terminal_commit_retries: Arc<HlsTerminalCommitRetryCoordinator>,
    terminal_pending: Arc<HlsTerminalPendingCoordinator>,
    availability_reevaluations: Arc<HlsAvailabilityReevaluationCoordinator>,
    terminal_commit_clock: Arc<HlsTerminalCommitClock>,
    startup_observability: Arc<HlsStartupObservability>,
    revision_store: Arc<super::SegmentRevisionStore>,
    revision_reconciliation_pending: AtomicBool,
    progressive_budget: Arc<super::ProgressiveBudgetManager>,
}

pub struct HlsTerminalTailPreparationRequest<'a> {
    pub lease_id: &'a HlsAccessLeaseId,
    pub proxy_session_id: &'a ProxySessionId,
    pub manifest_snapshot_generation: u64,
    pub cursor_generation: u64,
    pub reserve: HlsLeaseReserveSnapshot,
    pub cutover_timing: HlsLeaseCutoverTiming,
    pub commit_window: HlsTerminalCommitWindow,
    pub now_ms: u64,
    pub origin_progress_generation: u64,
    pub media_readiness_generation: u64,
    pub last_media_progress_at_ms: Option<u64>,
}

/// Generation-bound terminal publication requested against one immutable preparation.
pub struct HlsTerminalCommitRequest<'a> {
    pub session: &'a HlsSessionHandle,
    pub lease_id: &'a HlsAccessLeaseId,
    pub proxy_session_id: &'a ProxySessionId,
    pub preparation: &'a HlsTerminalTailPreparation,
    pub now_ms: u64,
    pub payload: HlsTerminalCommitPayload,
    pub asset_revision_guard: HlsTerminalAssetRevisionGuard,
}

/// Media and compatibility evidence required for one terminal publication kind.
pub enum HlsTerminalCommitPayload {
    Tail { plan: Arc<HlsTerminalTailPlan>, media_guard: HlsTerminalCommitMediaGuard },
    Unavailable(HlsTerminalTailCompatibility),
    UnavailableAfterOwnerFailure(HlsTerminalTailCompatibility),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsMediaActivityCommitOutcome {
    Committed,
    StaleLeaseIdentity,
    DeferredLockContention,
}

const HLS_STATE_CAS_LOCK_RETRIES: usize = 8;

const HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsCriticalHandoffStateAccess<T> {
    Acquired(T),
    LockBusy,
}

const HLS_SESSION_IDLE_PROTECTION_RETRY_MS: u64 = 1_000;

#[cfg(test)]
mod tests;

mod account_overlap;
mod activity;
mod cleanup;
mod config;
mod lifecycle;
mod publication;
mod terminal;

pub use self::lifecycle::exec_hls_lifecycle;
#[cfg(test)]
use self::lifecycle::hls_session_idle_protection_retry_at;
#[cfg(test)]
use self::publication::HlsRecoveryExecutionState;
use self::{
    account_overlap::{HlsAccountOverlapCooldown, HlsAccountOverlapCooldownKey},
    activity::hls_key_readiness_evidence_is_current,
    config::HlsProxyRuntimeConfig,
    publication::{
        hls_acceptance_recovery_snapshot, hls_estimated_recovery_completion_at, HlsAcceptanceRecoverySnapshot,
    },
};
