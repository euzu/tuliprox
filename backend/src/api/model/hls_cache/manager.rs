use super::{
    availability_reevaluation::{
        HlsAvailabilityReevaluationCoordinator, HlsAvailabilityReevaluationOwnerKey,
        HlsRecoveryPressureGuard, HlsRecoveryPressureGuardAccess,
    },
    build_rewrite_secret_fingerprint,
    cutover::{
        evaluate_terminal_cutover, HlsTerminalCutoverCapability, HlsTerminalCutoverDecision, HlsTerminalCutoverInput,
    },
    lease::{
        HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsAccessLeaseRemovalPreparation,
        HlsLeaseManifestPublicationGuard, HlsLeaseManifestPublicationOutcome, HlsMediaLeaseIdentity,
        HlsRuntimePolicyRevocation, HlsRuntimePolicyRevocationOutcome,
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
    segment_repair::{ready_segment_repair_prewarm_candidates, HlsRepairPrewarmGuard},
    safe_proxy_session_id, safe_session_key,
    session_store::HlsCurrentProxySessionAccess,
    terminal_commit::{
        next_terminal_commit_retry, spawn_terminal_commit_retry_worker, HlsTerminalAssetRevisionGuard,
        HlsTerminalAssetRevisionValidation, HlsTerminalCommitClock, HlsTerminalCommitCommand,
        HlsTerminalCommitAttempt, HlsTerminalCommitOutcome, HlsTerminalCommitOwnerKey, HlsTerminalCommitOwnerToken,
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
    HlsMapWorkerPool, HlsOriginSource, HlsPlaybackRequestToken, HlsQosRegistry, HlsSegmentCache,
    HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle, HlsSessionKey, HlsSessionStore,
    HlsSessionStoreOutcome, HlsStartupObservability, HlsTerminalTailProtection, HlsTerminalTailProtectionInstall,
    HlsTerminalTailProtectionRemoval, ProxySessionId, SegmentFetchPolicy, TransientResourceStore,
};
use crate::{
    api::model::{ActiveProviderManager, ActiveUserManager, AppState},
    model::{AppConfig, HlsCacheConfig, HlsManifestRecoveryBurstConfig, StripConfig},
};
use arc_swap::ArcSwap;
use log::{debug, error, info};
use shared::utils::sanitize_sensitive_info;
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

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
}

#[derive(Debug, Clone)]
struct HlsProxyRuntimeConfig {
    enabled: bool,
    segment_fetch_policy: SegmentFetchPolicy,
    cache_duration_seconds: u64,
    strip: StripConfig,
    origin_manifest_timeout_ms: u64,
    manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    transient_resource_ttl_ms: u64,
    gc_policy: GarbageCollectionPolicy,
    rewrite_secret_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HlsAccountOverlapCooldownKey {
    input_name: Arc<str>,
    account_name: Arc<str>,
}

#[derive(Debug, Clone, Copy)]
struct HlsAccountOverlapCooldown {
    until_ms: u64,
}

#[derive(Debug, Clone, Copy)]
enum HlsAccountOverlapCooldownReason {
    ReclaimedByOriginalOwner,
    SpeculativePromoted,
}

pub(crate) struct HlsTerminalTailPreparationRequest<'a> {
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

#[derive(Clone, Copy)]
enum HlsTerminalPreparationPurpose {
    Cutover,
    UnavailableAfterOwnerFailure,
}

/// Generation-bound terminal publication requested against one immutable preparation.
pub(crate) struct HlsTerminalCommitRequest<'a> {
    pub session: &'a HlsSessionHandle,
    pub lease_id: &'a HlsAccessLeaseId,
    pub proxy_session_id: &'a ProxySessionId,
    pub preparation: &'a HlsTerminalTailPreparation,
    pub now_ms: u64,
    pub payload: HlsTerminalCommitPayload,
    pub asset_revision_guard: HlsTerminalAssetRevisionGuard,
}

/// Media and compatibility evidence required for one terminal publication kind.
pub(crate) enum HlsTerminalCommitPayload {
    Tail { plan: Arc<HlsTerminalTailPlan>, media_guard: HlsTerminalCommitMediaGuard },
    Unavailable(HlsTerminalTailCompatibility),
    UnavailableAfterOwnerFailure(HlsTerminalTailCompatibility),
}

impl HlsTerminalCommitPayload {
    fn into_parts(self) -> (HlsTerminalLeaseDecision, Option<HlsTerminalCommitMediaGuard>) {
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
#[must_use]
pub(crate) enum HlsMediaActivityCommitOutcome {
    Committed,
    StaleLeaseIdentity,
    DeferredLockContention,
}

#[derive(Debug, Clone, Copy)]
enum HlsMediaActivityCommitKind {
    Access,
    LiveSegmentCompletion(HlsPlaybackRequestToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsMediaActivityCommitAttempt {
    LockBusy,
    Completed { outcome: HlsMediaActivityCommitOutcome, evidence_changed: bool },
}

const HLS_STATE_CAS_LOCK_RETRIES: usize = 8;
const HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlsCriticalHandoffStateAccess<T> {
    Acquired(T),
    LockBusy,
}

const HLS_SESSION_IDLE_PROTECTION_RETRY_MS: u64 = 1_000;

fn hls_session_idle_protection_retry_at(due_at_ms: u64, now_ms: u64) -> u64 {
    due_at_ms.max(now_ms.saturating_add(HLS_SESSION_IDLE_PROTECTION_RETRY_MS))
}

fn hls_key_readiness_evidence_is_current(valid_until_ms: Option<u64>, now_ms: u64) -> bool {
    valid_until_ms.is_none_or(|valid_until_ms| valid_until_ms >= now_ms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsRecoveryExecutionState {
    Idle,
    InFlight {
        estimated_completion_at: HlsEstimatedRecoveryCompletionAtMs,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsAcceptanceRecoverySnapshot {
    expected_generation: HlsManifestAcceptanceGeneration,
    status: HlsManifestAcceptanceEpisodeStatus,
    recovery: HlsRecoveryExecutionState,
    required_terminal_media_key: Option<HlsTerminalMediaPreparationKey>,
    terminal_media_preparation: HlsTerminalMediaPreparationState,
}

fn hls_estimated_recovery_completion_at(
    recovery: HlsRecoveryExecutionState,
) -> Option<HlsEstimatedRecoveryCompletionAtMs> {
    match recovery {
        HlsRecoveryExecutionState::Idle => None,
        HlsRecoveryExecutionState::InFlight { estimated_completion_at } => Some(estimated_completion_at),
    }
}

fn hls_acceptance_recovery_snapshot(session: &super::HlsSession, now_ms: u64) -> HlsAcceptanceRecoverySnapshot {
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

fn terminal_media_requirement_origin(
    recovery_snapshot: &HlsAcceptanceRecoverySnapshot,
) -> HlsTerminalMediaRequirementOrigin {
    match recovery_snapshot.status {
        HlsManifestAcceptanceEpisodeStatus::Missing => HlsTerminalMediaRequirementOrigin::CutoverSnapshot,
        HlsManifestAcceptanceEpisodeStatus::Committed { .. }
        | HlsManifestAcceptanceEpisodeStatus::InFlight { .. }
        | HlsManifestAcceptanceEpisodeStatus::Expired { .. }
        | HlsManifestAcceptanceEpisodeStatus::FullBurstExhausted { .. }
        | HlsManifestAcceptanceEpisodeStatus::Superseded { .. } => {
            HlsTerminalMediaRequirementOrigin::AcceptanceEpisode {
                generation: recovery_snapshot.expected_generation,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsTerminalCommitAuthorization {
    Authorized { protection_capacity_exceeded: bool },
    Rejected(HlsTerminalCommitOutcome),
}

fn terminal_media_requirement_is_bound_to_preparation(preparation: &HlsTerminalTailPreparation) -> bool {
    match preparation.terminal_media_requirement_source {
        HlsTerminalMediaRequirementSource::AcceptanceEpisode { generation } => {
            generation == preparation.expected_acceptance_generation
        }
        HlsTerminalMediaRequirementSource::CutoverSnapshotPending { decision_generation }
        | HlsTerminalMediaRequirementSource::CutoverSnapshot { decision_generation, .. } => {
            decision_generation == preparation.decision_generation
        }
    }
}

fn evaluate_terminal_commit_authorization(
    session: &super::HlsSession,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    decision: &HlsTerminalLeaseDecision,
    now_ms: u64,
) -> HlsTerminalCommitAuthorization {
    let recovery_snapshot = hls_acceptance_recovery_snapshot(session, now_ms);
    // Acceptance lifecycle generations describe recovery evidence, not media
    // continuity. A later episode must not invalidate a cutover snapshot while
    // the media progress, readiness, epoch, and lease generations guarded below
    // remain unchanged.
    if !terminal_media_requirement_is_bound_to_preparation(preparation) {
        return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::SupersededGeneration);
    }
    let protection_capacity_exceeded = matches!(decision, HlsTerminalLeaseDecision::Tail(_))
        && !session.can_install_terminal_tail_protection(lease_id);
    let (terminal, terminal_preparation) = match (decision, protection_capacity_exceeded) {
        (HlsTerminalLeaseDecision::Tail(plan), false) => {
            let prepared_key = plan.media_preparation_key();
            if preparation.required_terminal_media_key != Some(prepared_key)
                || !preparation
                    .terminal_media_requirement_source
                    .authorizes_tail(preparation.decision_generation, prepared_key)
            {
                return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible);
            }
            (
                HlsTerminalCutoverCapability::TailCompatible { prepared_key },
                HlsTerminalMediaPreparationState::Ready { key: prepared_key },
            )
        }
        (HlsTerminalLeaseDecision::Tail(_), true) => {
            (
                HlsTerminalCutoverCapability::TailUnavailable(
                    HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
                ),
                preparation.terminal_media_preparation,
            )
        }
        (
            HlsTerminalLeaseDecision::Unavailable(reason)
            | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason),
            _,
        ) => (
            HlsTerminalCutoverCapability::TailUnavailable(*reason),
            preparation.terminal_media_preparation,
        ),
    };
    if matches!(decision, HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_)) {
        if !session.origin_control.path_condition.is_degraded() {
            return HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::CutoverNoLongerRequired);
        }
        // This fallback publishes no media bytes. Submission arbitration and
        // the terminal precondition retain the preparation's original
        // exclusive deadline, so this authorization is reachable only while
        // the autonomous owner still has time to commit safely.
        return HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false };
    }
    if preparation.trigger.is_runtime_policy() {
        return HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded };
    }
    let cutover = evaluate_terminal_cutover(&HlsTerminalCutoverInput {
        reserve: preparation.reserve,
        commit_window: preparation.commit_window,
        acceptance: recovery_snapshot.status,
        required_terminal_media_key: preparation.required_terminal_media_key,
        terminal_preparation,
        terminal,
    });
    match (decision, cutover) {
        (HlsTerminalLeaseDecision::Tail(_), HlsTerminalCutoverDecision::CommitTerminalTail)
            if !protection_capacity_exceeded =>
        {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false }
        }
        (
            HlsTerminalLeaseDecision::Tail(_),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(
                HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
            ),
        ) if protection_capacity_exceeded => {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: true }
        }
        (
            HlsTerminalLeaseDecision::Unavailable(expected)
            | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(expected),
            HlsTerminalCutoverDecision::CommitTerminalUnavailable(actual),
        ) if *expected == actual => {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded: false }
        }
        (_, HlsTerminalCutoverDecision::NotRequired) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::CutoverNoLongerRequired)
        }
        (_, HlsTerminalCutoverDecision::RetrySupersededSnapshot) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::SupersededGeneration)
        }
        (_, HlsTerminalCutoverDecision::EvaluateTerminalCapability { .. }) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleNotReady)
        }
        (
            _,
            HlsTerminalCutoverDecision::CommitTerminalTail
            | HlsTerminalCutoverDecision::CommitTerminalUnavailable(_),
        ) => {
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible)
        }
    }
}

impl HlsAccountOverlapCooldownReason {
    fn as_log_reason(self) -> &'static str {
        match self {
            Self::ReclaimedByOriginalOwner => "reclaimed-by-original-owner",
            Self::SpeculativePromoted => "speculative-promoted",
        }
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

impl HlsProxyRuntimeConfig {
    fn from_config(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self::from_config_with_enabled(config, rewrite_secret, true)
    }

    fn from_config_with_enabled(config: &HlsCacheConfig, rewrite_secret: &[u8], enabled: bool) -> Self {
        Self {
            enabled,
            segment_fetch_policy: SegmentFetchPolicy::from_config(config),
            cache_duration_seconds: config.cache_duration,
            strip: config.strip.clone(),
            origin_manifest_timeout_ms: config.origin_manifest_timeout_ms,
            manifest_recovery_burst: config.manifest_recovery_burst.clone(),
            transient_resource_ttl_ms: config.cache_duration.saturating_mul(1_000),
            gc_policy: GarbageCollectionPolicy::from_config(config),
            rewrite_secret_fingerprint: build_rewrite_secret_fingerprint(rewrite_secret),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
struct HlsProxySessionCleanupStats {
    access_leases: usize,
    repair_windows: usize,
    repair_generations: usize,
    repair_candidates: usize,
    repair_object_metadata: usize,
    repair_watchdog_metadata: usize,
    repair_watchdog_locks: usize,
    qos_access_leases: usize,
}

impl HlsProxySessionCleanupStats {
    fn did_cleanup(self) -> bool {
        self.access_leases > 0
            || self.repair_windows > 0
            || self.repair_generations > 0
            || self.repair_candidates > 0
            || self.repair_object_metadata > 0
            || self.repair_watchdog_metadata > 0
            || self.repair_watchdog_locks > 0
            || self.qos_access_leases > 0
    }
}

impl HlsProxyManager {
    pub fn new() -> Self {
        let default_dto = shared::model::HlsCacheConfigDto::default();
        let default_config = HlsCacheConfig::from(&default_dto);
        Self::with_hls_cache_config(&default_config)
    }

    pub fn from_hls_cache_config(config: Option<&HlsCacheConfig>) -> Self {
        Self::from_hls_cache_config_and_secret(config, &[])
    }

    pub fn from_hls_cache_config_and_secret(config: Option<&HlsCacheConfig>, rewrite_secret: &[u8]) -> Self {
        let default_config;
        let (config, enabled) = if let Some(config) = config {
            (config, true)
        } else {
            default_config = HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default());
            (&default_config, false)
        };
        Self::with_hls_cache_config_and_secret_enabled(config, rewrite_secret, enabled)
    }

    pub fn with_cache_settings(cache_path: impl Into<PathBuf>, cache_duration_seconds: u64) -> Self {
        let default_dto = shared::model::HlsCacheConfigDto {
            cache_duration: cache_duration_seconds,
            cache_path: Some(cache_path.into().to_string_lossy().to_string()),
            ..Default::default()
        };
        let default_config = HlsCacheConfig::from(&default_dto);
        let segment_fetch_policy = SegmentFetchPolicy::from_config(&default_config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&default_config.cache_path)));
        segment_cache.update_cache_limits(default_config.cache_bytes, default_config.cache_bytes_per_session);
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(default_config.segment_repair.clone()));
        let metrics = Arc::new(HlsCacheMetrics::default());
        let qos = Arc::new(HlsQosRegistry::default());
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        let lifecycle = Arc::new(HlsLifecycleManager::new());
        let account_overlap_cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let gc_policy = GarbageCollectionPolicy::from_config(&default_config);
        let runtime_config = HlsProxyRuntimeConfig::from_config(&default_config, &[]);
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

    pub fn with_hls_cache_config(config: &HlsCacheConfig) -> Self {
        Self::with_hls_cache_config_and_secret(config, &[])
    }

    pub fn with_hls_cache_config_and_secret(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self::with_hls_cache_config_and_secret_enabled(config, rewrite_secret, true)
    }

    fn with_hls_cache_config_and_secret_enabled(config: &HlsCacheConfig, rewrite_secret: &[u8], enabled: bool) -> Self {
        let segment_fetch_policy = SegmentFetchPolicy::from_config(config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&config.cache_path)));
        segment_cache.update_cache_limits(config.cache_bytes, config.cache_bytes_per_session);
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

    pub(crate) fn startup_observability(&self) -> &Arc<HlsStartupObservability> { &self.startup_observability }

    pub(crate) fn spawn_access_lease_repair_prewarm(
        self: &Arc<Self>,
        session: HlsSessionHandle,
        lease_id: HlsAccessLeaseId,
        snapshot: HlsLeaseManifestSnapshot,
        snapshot_generation: u64,
    ) {
        if snapshot.delivery_mode != HlsManifestDeliveryMode::NormalCacheTimeline
            || snapshot.container != super::terminal_tail::HlsMediaContainer::MpegTs
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
            let Some(lease) = manager
                .access_lease_response_snapshot(
                    &lease_id,
                    &proxy_session_id,
                    current_time_millis(),
                )
                .await
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

    pub(crate) fn start_prepared_terminal_bundle(
        &self,
        asset: Arc<HlsTerminalMediaAsset>,
        target_duration_ms: u64,
        segment_count: u16,
    ) -> HlsPreparedTerminalBundleState {
        self.prepared_terminal_bundles.start_preparation(asset, target_duration_ms, segment_count)
    }

    pub(crate) fn prepared_terminal_bundle_state(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        self.prepared_terminal_bundles.state(key)
    }

    #[cfg(test)]
    pub(crate) fn install_controlled_terminal_bundle_flight_for_test(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<super::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionPublisher> {
        self.prepared_terminal_bundles
            .install_controlled_flight_for_test(key)
    }

    pub(crate) fn observe_prepared_terminal_bundle(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> HlsPreparedTerminalBundleObservation {
        self.prepared_terminal_bundles.observe_exact(key)
    }

    pub(crate) fn register_standalone_custom_access(
        &self,
        entry: HlsStandaloneCustomAccessEntry,
        now_ms: u64,
    ) {
        self.standalone_custom_access.register(entry, now_ms);
    }

    pub(crate) fn resolve_standalone_custom_segment(
        &self,
        lease_id: &HlsAccessLeaseId,
        asset_fingerprint: &str,
        index: u16,
        now_ms: u64,
    ) -> Result<HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError> {
        self.standalone_custom_access.resolve(lease_id, asset_fingerprint, index, now_ms)
    }

    pub(crate) fn terminal_pending(&self) -> Arc<HlsTerminalPendingCoordinator> {
        Arc::clone(&self.terminal_pending)
    }

    /// Cancels terminal work frozen before newly committed shared media
    /// progress. Callers must not hold the session or lease-store lock; late
    /// registrations remain protected by the final progress-generation CAS.
    pub(crate) fn cancel_superseded_terminal_work_for_session(&self, proxy_session_id: &ProxySessionId) {
        self.terminal_pending.cancel_session(proxy_session_id);
        self.terminal_commit_retries.cancel_session(proxy_session_id);
    }

    pub(crate) fn terminal_commit_now_ms(&self) -> u64 { self.terminal_commit_clock.now_ms() }

    #[cfg(test)]
    pub(crate) async fn wait_for_prepared_terminal_bundle(
        &self,
        key: HlsPreparedTerminalBundleKey,
    ) -> Option<HlsPreparedTerminalBundleState> {
        self.prepared_terminal_bundles.wait_for_completion(key).await
    }

    pub(crate) fn reserve_switch_segment_cleanup(
        &self,
        key: super::SegmentCacheKey,
    ) -> Option<super::gc::HlsSwitchCacheCleanupReservation> {
        self.gc.reserve_switch_segment_cleanup(key)
    }

    pub(crate) fn reserve_switch_map_cleanup(
        &self,
        key: super::MapCacheKey,
    ) -> Option<super::gc::HlsSwitchCacheCleanupReservation> {
        self.gc.reserve_switch_map_cleanup(key)
    }

    pub(crate) fn has_pending_switch_cleanup(
        &self,
        segment_key: &super::SegmentCacheKey,
        map_key: Option<&super::MapCacheKey>,
    ) -> bool {
        self.gc.has_pending_switch_cleanup(segment_key, map_key)
    }

    #[cfg(test)]
    pub(crate) fn cache_deletion_queue_usage(&self) -> (usize, usize) { self.gc.cache_deletion_queue_usage() }

    pub fn segment_fetch_policy(&self) -> SegmentFetchPolicy { self.runtime_config.load().segment_fetch_policy.clone() }

    pub fn is_enabled(&self) -> bool { self.runtime_config.load().enabled }

    pub fn cache_duration_seconds(&self) -> u64 { self.runtime_config.load().cache_duration_seconds }

    pub fn session_idle_timeout_ms(&self) -> u64 { self.runtime_config.load().gc_policy.session_idle_timeout_ms }

    pub fn strip(&self) -> StripConfig { self.runtime_config.load().strip.clone() }

    pub fn origin_manifest_timeout_ms(&self) -> u64 { self.runtime_config.load().origin_manifest_timeout_ms }

    pub fn manifest_recovery_burst(&self) -> HlsManifestRecoveryBurstConfig {
        self.runtime_config.load().manifest_recovery_burst.clone()
    }

    pub fn transient_resource_ttl_ms(&self) -> u64 { self.runtime_config.load().transient_resource_ttl_ms }

    pub fn transient_resources(&self) -> &Arc<TransientResourceStore> { &self.transient_resources }

    pub fn access_leases(&self) -> &Arc<RwLock<HlsAccessLeaseStore>> { &self.access_leases }

    pub fn lifecycle(&self) -> &Arc<HlsLifecycleManager> { &self.lifecycle }

    pub fn metrics(&self) -> &Arc<HlsCacheMetrics> { &self.metrics }

    pub fn qos(&self) -> &Arc<HlsQosRegistry> { &self.qos }

    pub fn garbage_collector(&self) -> &Arc<HlsGarbageCollector> { &self.gc }

    pub fn gc_policy(&self) -> GarbageCollectionPolicy { self.runtime_config.load().gc_policy.clone() }

    pub fn rewrite_secret_fingerprint(&self) -> String { self.runtime_config.load().rewrite_secret_fingerprint.clone() }

    pub async fn is_account_overlap_cooling_down(
        &self,
        input_name: &Arc<str>,
        account_name: &Arc<str>,
        now_ms: u64,
    ) -> bool {
        let key =
            HlsAccountOverlapCooldownKey { input_name: Arc::clone(input_name), account_name: Arc::clone(account_name) };
        let mut cooldowns = self.account_overlap_cooldowns.write().await;
        let Some(cooldown) = cooldowns.get(&key).copied() else {
            return false;
        };
        if now_ms >= cooldown.until_ms {
            cooldowns.remove(&key);
            return false;
        }
        true
    }

    pub async fn mark_account_overlap_reclaimed_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
    ) {
        self.mark_account_overlap_cooldown(
            input_name,
            account_name,
            now_ms,
            hard_active_window_ms,
            HlsAccountOverlapCooldownReason::ReclaimedByOriginalOwner,
        )
        .await;
    }

    pub async fn mark_account_overlap_promoted_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
    ) {
        self.mark_account_overlap_cooldown(
            input_name,
            account_name,
            now_ms,
            hard_active_window_ms,
            HlsAccountOverlapCooldownReason::SpeculativePromoted,
        )
        .await;
    }

    async fn mark_account_overlap_cooldown(
        &self,
        input_name: Arc<str>,
        account_name: Arc<str>,
        now_ms: u64,
        hard_active_window_ms: u64,
        reason: HlsAccountOverlapCooldownReason,
    ) {
        let until_ms = now_ms.saturating_add(hard_active_window_ms);
        if until_ms <= now_ms {
            return;
        }
        let key = HlsAccountOverlapCooldownKey { input_name, account_name };
        self.account_overlap_cooldowns.write().await.insert(key.clone(), HlsAccountOverlapCooldown { until_ms });
        debug!(
            "HLS account overlap cooldown set for input {} account {} until {} ms after {}",
            sanitize_sensitive_info(key.input_name.as_ref()),
            sanitize_sensitive_info(key.account_name.as_ref()),
            until_ms,
            reason.as_log_reason()
        );
    }

    pub async fn update_config(&self, app_config: &AppConfig) {
        let (hls_config, rewrite_secret, enabled) = {
            let config = app_config.config.load();
            let rewrite_secret = config
                .reverse_proxy
                .as_ref()
                .map_or(app_config.encrypt_secret, |reverse_proxy| reverse_proxy.rewrite_secret);
            let hls_config =
                config.reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.hls_cache.as_ref()).cloned();
            let enabled = hls_config.is_some();
            let hls_config =
                hls_config.unwrap_or_else(|| HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default()));
            (hls_config, rewrite_secret, enabled)
        };
        let runtime_config = HlsProxyRuntimeConfig::from_config_with_enabled(&hls_config, &rewrite_secret, enabled);
        let cache_path_changed = self.gc.update_cache_path(PathBuf::from(&hls_config.cache_path)).await;
        self.segment_cache.update_cache_limits(hls_config.cache_bytes, hls_config.cache_bytes_per_session);
        if cache_path_changed {
            self.clear_runtime_cache_state_for_cache_path_change().await;
        }
        for session in self.sessions.list_sessions().await {
            session
                .write()
                .await
                .configure_segment_prefetch_queue(runtime_config.segment_fetch_policy.max_prefetch_queue_depth);
        }
        self.segment_repair.update_config(hls_config.segment_repair.clone());
        let global_fetch_semaphore =
            Arc::new(Semaphore::new(runtime_config.segment_fetch_policy.max_global_segment_fetches));
        self.segment_worker_pool
            .update_config(runtime_config.segment_fetch_policy.clone(), Arc::clone(&global_fetch_semaphore));
        self.map_worker_pool.update_config(runtime_config.segment_fetch_policy.clone(), global_fetch_semaphore);
        self.gc.update_config(runtime_config.gc_policy.clone(), runtime_config.rewrite_secret_fingerprint.clone());
        self.runtime_config.store(Arc::new(runtime_config));
    }

    async fn clear_runtime_cache_state_for_cache_path_change(&self) {
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
                super::safe_hls_access_lease_id(&lease.lease_id),
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

    pub(crate) async fn active_live_playback_snapshots_for_session(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Vec<HlsAccessLease> {
        self.access_leases.write().await.active_live_playback_snapshots_for_session(proxy_session_id, now_ms)
    }

    /// Runs the final Critical-Handoff revalidation and timeline commit under the
    /// established lease-store -> session lock order. No lock is held across an await.
    /// `LockBusy` reports contention only and is not a generation or lease invalidation.
    pub(crate) async fn with_critical_handoff_state<T>(
        &self,
        session_handle: &HlsSessionHandle,
        operation: impl FnOnce(&mut HlsAccessLeaseStore, &mut super::HlsSession) -> T,
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
    pub(crate) async fn availability_reevaluation_owner_key(
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

    pub(crate) async fn availability_reevaluation_session_is_current(
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
    pub(crate) fn with_current_recovery_pressure_session<R>(
        &self,
        session_handle: &HlsSessionHandle,
        guard: &HlsRecoveryPressureGuard,
        operation: impl FnOnce(&mut super::HlsSession) -> R,
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

    pub(crate) fn availability_reevaluations(&self) -> Arc<HlsAvailabilityReevaluationCoordinator> {
        Arc::clone(&self.availability_reevaluations)
    }

    pub(crate) fn notify_session_evidence_changed(&self, proxy_session_id: &ProxySessionId) -> bool {
        self.availability_reevaluations
            .notify_session_evidence_changed(proxy_session_id)
    }

    #[cfg(test)]
    pub(crate) fn set_terminal_commit_retry_capacity_for_test(&self, capacity: usize) {
        self.terminal_commit_retries.set_capacity_for_test(capacity);
    }

    #[cfg(test)]
    pub(crate) async fn hold_access_lease_store_for_test(
        &self,
    ) -> tokio::sync::OwnedRwLockWriteGuard<HlsAccessLeaseStore> {
        Arc::clone(&self.access_leases).write_owned().await
    }

    pub(crate) async fn prepare_access_lease_manifest_publication(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsLeaseManifestPublicationGuard> {
        self.access_leases.write().await.prepare_manifest_publication(lease_id, proxy_session_id, now_ms)
    }

    pub(crate) async fn commit_access_lease_manifest_publication(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        expected: HlsLeaseManifestPublicationGuard,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> HlsLeaseManifestPublicationOutcome {
        self.access_leases.write().await.commit_manifest_publication(
            lease_id,
            proxy_session_id,
            expected,
            snapshot,
            now_ms,
        )
    }

    pub(crate) async fn record_access_lease_segment_request_started_if_identity_matches(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        proxy_seq: u64,
        requested_at_ms: u64,
    ) -> Option<HlsPlaybackRequestToken> {
        self.access_leases.write().await.record_segment_request_started_if_identity_matches(
            lease_id,
            proxy_session_id,
            lease_identity,
            proxy_seq,
            requested_at_ms,
        )
    }

    pub(crate) async fn record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        token: HlsPlaybackRequestToken,
        completed_at_ms: u64,
    ) -> HlsMediaActivityCommitOutcome {
        self.commit_media_activity_if_identity_matches(
            session,
            lease_id,
            proxy_session_id,
            lease_identity,
            completed_at_ms,
            HlsMediaActivityCommitKind::LiveSegmentCompletion(token),
        )
        .await
    }

    pub(crate) async fn prepare_access_lease_terminal_tail(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
    ) -> Option<HlsTerminalTailPreparation> {
        self.prepare_access_lease_terminal_decision(request, HlsTerminalPreparationPurpose::Cutover).await
    }

    pub(crate) async fn prepare_access_lease_terminal_unavailable_after_owner_failure(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
    ) -> Option<HlsTerminalTailPreparation> {
        self.prepare_access_lease_terminal_decision(
            request,
            HlsTerminalPreparationPurpose::UnavailableAfterOwnerFailure,
        )
        .await
    }

    pub(crate) async fn prepare_access_lease_runtime_custom_tail(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> Option<HlsTerminalTailPreparation> {
        if reason.trigger_class() != super::runtime_custom_tail::HlsRuntimeCustomTailTriggerClass::ImmediatePolicyCutover
        {
            return None;
        }
        let current_session = self.sessions.get_by_proxy_session_id(proxy_session_id).await?;
        if !Arc::ptr_eq(&current_session, session) {
            return None;
        }
        let lease = self.access_lease_response_snapshot(lease_id, proxy_session_id, now_ms).await?;
        if lease.playback_mode != super::terminal_tail::HlsLeasePlaybackMode::Live {
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
                playback_rate_guard_milli: super::HLS_PLAYBACK_RATE_GUARD_MILLI,
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

    pub(crate) async fn begin_runtime_policy_revocation(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        reason: HlsRuntimeCustomTailReason,
        now_ms: u64,
    ) -> HlsRuntimePolicyRevocationOutcome {
        self.access_leases
            .write()
            .await
            .begin_runtime_policy_revocation(lease_id, proxy_session_id, reason, now_ms)
    }

    pub(crate) async fn fail_runtime_policy_revocation(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        token: &HlsRuntimePolicyRevocation,
    ) -> HlsAccessLeaseDenialOutcome {
        let outcome = self
            .access_leases
            .write()
            .await
            .fail_runtime_policy_revocation(lease_id, proxy_session_id, token);
        if matches!(outcome, HlsAccessLeaseDenialOutcome::Ended { .. }) {
            self.terminal_pending.cancel_lease(lease_id);
            self.terminal_commit_retries.cancel_lease(lease_id);
        }
        outcome
    }

    async fn prepare_access_lease_terminal_decision(
        &self,
        request: HlsTerminalTailPreparationRequest<'_>,
        purpose: HlsTerminalPreparationPurpose,
    ) -> Option<HlsTerminalTailPreparation> {
        let HlsTerminalTailPreparationRequest {
            lease_id,
            proxy_session_id,
            manifest_snapshot_generation,
            cursor_generation,
            reserve,
            cutover_timing,
            commit_window,
            now_ms,
            origin_progress_generation: expected_origin_progress_generation,
            media_readiness_generation: expected_media_readiness_generation,
            last_media_progress_at_ms: expected_last_media_progress_at_ms,
        } = request;
        let session = self.sessions.get_by_proxy_session_id(proxy_session_id).await?;
        let (
            origin_progress_generation,
            media_readiness_generation,
            origin_epoch,
            last_media_progress_at_ms,
            origin_path_degraded,
            recovery_snapshot,
        ) = {
            let session = session.read().await;
            let recovery_snapshot = hls_acceptance_recovery_snapshot(&session, now_ms);
            (
                session.origin_control.progress_generation,
                session.activity.media_readiness_generation,
                session.origin_control.origin_epoch,
                session.origin_control.last_media_progress_at_ms,
                session.origin_control.path_condition.is_degraded(),
                recovery_snapshot,
            )
        };
        let expected_cutover_timing = HlsLeaseCutoverTiming::from_reserve(
            now_ms,
            reserve.guaranteed_reserve_ms,
            reserve.transition_margin,
            None,
        );
        if origin_progress_generation != expected_origin_progress_generation
            || media_readiness_generation != expected_media_readiness_generation
            || last_media_progress_at_ms != expected_last_media_progress_at_ms
            || cutover_timing != expected_cutover_timing
            || !hls_key_readiness_evidence_is_current(reserve.key_readiness_valid_until_ms, now_ms)
        {
            return None;
        }
        if matches!(purpose, HlsTerminalPreparationPurpose::UnavailableAfterOwnerFailure)
            && !origin_path_degraded
        {
            return None;
        }
        let cutover_timing = cutover_timing.with_estimated_recovery_completion_at(
            hls_estimated_recovery_completion_at(recovery_snapshot.recovery),
        );
        if matches!(purpose, HlsTerminalPreparationPurpose::Cutover)
            && !matches!(
            evaluate_terminal_cutover(&HlsTerminalCutoverInput {
                reserve,
                commit_window,
                acceptance: recovery_snapshot.status,
                required_terminal_media_key: recovery_snapshot.required_terminal_media_key,
                terminal_preparation: recovery_snapshot.terminal_media_preparation,
                terminal: HlsTerminalCutoverCapability::NotEvaluated,
            }),
            HlsTerminalCutoverDecision::EvaluateTerminalCapability { .. }
        )
        {
            return None;
        }
        let terminal_media_requirement_origin = terminal_media_requirement_origin(&recovery_snapshot);
        self.access_leases.read().await.prepare_terminal_tail(
            lease_id,
            proxy_session_id,
            &HlsTerminalTailPreparationInput {
                trigger: HlsFiniteTailTrigger::AvailabilityReserve,
                expected_manifest_snapshot_generation: manifest_snapshot_generation,
                expected_cursor_generation: cursor_generation,
                origin_progress_generation,
                media_readiness_generation,
                origin_epoch,
                last_media_progress_at_ms,
                expected_acceptance_generation: recovery_snapshot.expected_generation,
                terminal_media_requirement_origin,
                cutover_timing,
                commit_window,
                required_terminal_media_key: recovery_snapshot.required_terminal_media_key,
                terminal_media_preparation: recovery_snapshot.terminal_media_preparation,
                reserve,
            },
        )
    }

    pub(crate) fn commit_access_lease_terminal_if_generation_matches(
        &self,
        request: HlsTerminalCommitRequest<'_>,
    ) -> HlsTerminalCommitOutcome {
        let HlsTerminalCommitRequest {
            session,
            lease_id,
            proxy_session_id,
            preparation,
            now_ms,
            payload,
            asset_revision_guard,
        } = request;
        let (decision, media_guard) = payload.into_parts();
        let key = HlsTerminalCommitOwnerKey::from_preparation(proxy_session_id, lease_id, preparation);
        let Some(session_incarnation) = self.sessions.session_incarnation(session) else {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        };
        let Some((cancellation_epoch, submission_token)) = self.terminal_commit_retries.reserve_submission() else {
            return HlsTerminalCommitOutcome::RetryCapacityExceeded;
        };
        let command = HlsTerminalCommitCommand {
            key,
            session: Arc::clone(session),
            session_incarnation,
            preparation: preparation.clone(),
            decision,
            media_guard,
            asset_revision_guard,
            cancellation_epoch,
            submission_token,
        };
        let attempt_now_ms = self.terminal_commit_clock.initial_attempt_now_ms(now_ms);
        let (authorized_command, owner_token) = match self.terminal_commit_retries.submit(command, attempt_now_ms) {
            HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token } => (command, owner_token),
            HlsTerminalCommitSubmissionDecision::PendingExisting { retry_before_ms } => {
                return HlsTerminalCommitOutcome::LockBusy { retry_before_ms };
            }
            HlsTerminalCommitSubmissionDecision::Failed(outcome) => return outcome,
            HlsTerminalCommitSubmissionDecision::Cancelled => {
                return HlsTerminalCommitOutcome::SupersededGeneration;
            }
            HlsTerminalCommitSubmissionDecision::CapacityExceeded => {
                return HlsTerminalCommitOutcome::RetryCapacityExceeded;
            }
        };
        let authorized_key = authorized_command.key.clone();
        let attempt = match self.sessions.try_with_current_proxy_session(
            &authorized_key.proxy_session_id,
            &authorized_command.session,
            || {
                self.terminal_commit_retries.with_current_owner(
                    &authorized_key,
                    owner_token,
                    |command, latest_safe_terminal_commit_at_ms| {
                        let attempt_now_ms = self.terminal_commit_clock.initial_attempt_now_ms(now_ms);
                        let attempt = Self::try_commit_access_lease_terminal_decision(
                            &self.access_leases,
                            command,
                            attempt_now_ms,
                            latest_safe_terminal_commit_at_ms,
                        );
                        (attempt_now_ms, latest_safe_terminal_commit_at_ms, attempt)
                    },
                )
            },
        ) {
            HlsCurrentProxySessionAccess::Acquired(attempt) => attempt,
            HlsCurrentProxySessionAccess::Superseded => {
                self.terminal_commit_retries.discard_owner(&authorized_key, owner_token);
                None
            }
            HlsCurrentProxySessionAccess::LockBusy => self.terminal_commit_retries.with_current_owner(
                &authorized_key,
                owner_token,
                |_command, latest_safe_terminal_commit_at_ms| {
                    (
                        self.terminal_commit_clock.initial_attempt_now_ms(now_ms),
                        latest_safe_terminal_commit_at_ms,
                        HlsTerminalCommitAttempt::LockBusy,
                    )
                },
            ),
        };
        let Some((attempts_completed, (attempt_now_ms, deadline_ms, attempt))) = attempt else {
            return HlsTerminalCommitOutcome::SupersededGeneration;
        };
        match attempt {
            HlsTerminalCommitAttempt::Completed(outcome) => {
                self.terminal_commit_retries.complete_owner(&authorized_key, owner_token);
                outcome
            }
            HlsTerminalCommitAttempt::LockBusy => self.schedule_terminal_commit_retry(
                &authorized_key,
                owner_token,
                attempts_completed,
                attempt_now_ms,
                deadline_ms,
            ),
        }
    }

    pub(super) fn try_commit_access_lease_terminal_decision(
        access_leases: &Arc<RwLock<HlsAccessLeaseStore>>,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        latest_safe_commit_at_ms: u64,
    ) -> HlsTerminalCommitAttempt {
        // Submission arbitration releases the owner mutex before this path.
        // This follows the cross-store order documented by
        // with_current_recovery_pressure_session: session index -> retry owner
        // -> lease store -> session. The outer two guards prevent replacement
        // or cancellation TOCTOU; all inner locks are non-blocking and no
        // guard crosses an await or I/O boundary.
        let Ok(mut leases) = access_leases.try_write() else {
            return HlsTerminalCommitAttempt::LockBusy;
        };
        let Ok(mut session) = command.session.try_write() else {
            return HlsTerminalCommitAttempt::LockBusy;
        };
        if let Some(outcome) =
            Self::terminal_commit_precondition_outcome(&mut leases, &session, command, now_ms, latest_safe_commit_at_ms)
        {
            return HlsTerminalCommitAttempt::Completed(outcome);
        }
        let lease_id = &command.key.lease_id;
        let preparation = &command.preparation;
        let decision = &command.decision;
        // Asset identity gates only a new tail mutation. An exact terminal
        // replay was handled above and remains immutable. If a prepared tail's
        // asset changed while a LockBusy owner was queued, fail closed in this
        // same generation-bound CAS instead of abandoning the autonomous
        // intent and leaving the lease live.
        match command.asset_revision_guard.validate_current() {
            HlsTerminalAssetRevisionValidation::Current => {}
            HlsTerminalAssetRevisionValidation::Changed { current } => {
                if command.preparation.trigger.is_runtime_policy() {
                    return HlsTerminalCommitAttempt::Completed(HlsTerminalCommitOutcome::SupersededGeneration);
                }
                let reason = current.asset.map_or(HlsTerminalTailCompatibility::MissingAsset, |_| {
                    HlsTerminalTailCompatibility::AssetRevisionMismatch
                });
                return HlsTerminalCommitAttempt::Completed(Self::commit_terminal_unavailable_fallback(
                    &mut leases,
                    &mut session,
                    command,
                    now_ms,
                    reason,
                ));
            }
        }
        let protection_capacity_exceeded = match evaluate_terminal_commit_authorization(
            &session,
            lease_id,
            preparation,
            decision,
            now_ms,
        ) {
            HlsTerminalCommitAuthorization::Authorized { protection_capacity_exceeded } => {
                protection_capacity_exceeded
            }
            HlsTerminalCommitAuthorization::Rejected(HlsTerminalCommitOutcome::BundleIncompatible)
                if matches!(decision, HlsTerminalLeaseDecision::Tail(_)) =>
            {
                return HlsTerminalCommitAttempt::Completed(Self::commit_terminal_unavailable_fallback(
                    &mut leases,
                    &mut session,
                    command,
                    now_ms,
                    HlsTerminalTailCompatibility::TerminalMediaNotReady,
                ));
            }
            HlsTerminalCommitAuthorization::Rejected(outcome) => {
                return HlsTerminalCommitAttempt::Completed(outcome);
            }
        };
        HlsTerminalCommitAttempt::Completed(Self::publish_terminal_commit(
            &mut leases,
            &mut session,
            command,
            now_ms,
            protection_capacity_exceeded,
        ))
    }

    fn terminal_commit_precondition_outcome(
        leases: &mut HlsAccessLeaseStore,
        session: &super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        latest_safe_commit_at_ms: u64,
    ) -> Option<HlsTerminalCommitOutcome> {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let preparation = &command.preparation;
        let replay = match &command.decision {
            HlsTerminalLeaseDecision::Tail(_) => {
                leases.terminal_tail_replay_outcome(lease_id, proxy_session_id, preparation, now_ms)
            }
            HlsTerminalLeaseDecision::Unavailable(_)
            | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_) => {
                leases.terminal_unavailable_replay_outcome(lease_id, proxy_session_id, preparation, now_ms)
            }
        };
        if replay.is_some() {
            return replay;
        }
        if now_ms >= latest_safe_commit_at_ms {
            return Some(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed);
        }
        if preparation.trigger.is_runtime_policy() {
            return None;
        }
        if session.origin_control.progress_generation == preparation.origin_progress_generation
            && session.activity.media_readiness_generation == preparation.media_readiness_generation
            && session.origin_control.origin_epoch == preparation.origin_epoch
            && session.origin_control.last_media_progress_at_ms == preparation.last_media_progress_at_ms
            && hls_key_readiness_evidence_is_current(preparation.reserve.key_readiness_valid_until_ms, now_ms)
        {
            return None;
        }
        Some(
            if session.origin_control.last_media_progress_at_ms == preparation.last_media_progress_at_ms {
                HlsTerminalCommitOutcome::SupersededGeneration
            } else {
                HlsTerminalCommitOutcome::RecoveryCommitted
            },
        )
    }

    fn publish_terminal_commit(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        protection_capacity_exceeded: bool,
    ) -> HlsTerminalCommitOutcome {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let preparation = &command.preparation;
        let decision = &command.decision;
        let protection = match (decision, protection_capacity_exceeded) {
            (HlsTerminalLeaseDecision::Tail(plan), false) => Some(HlsTerminalTailProtection {
                generation: plan.generation,
                base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
                key_bindings: plan.key_bindings(),
            }),
            (HlsTerminalLeaseDecision::Tail(_), true)
            | (
                HlsTerminalLeaseDecision::Unavailable(_)
                | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_),
                _,
            ) => None,
        };
        let previous_protection = session.remove_terminal_tail_protection(lease_id);
        if let Some(protection) = protection {
            if session.install_terminal_tail_protection(lease_id.clone(), protection)
                != HlsTerminalTailProtectionInstall::Installed
            {
                session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
                return Self::commit_terminal_unavailable_fallback(
                    leases,
                    session,
                    command,
                    now_ms,
                    HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
                );
            }
        }
        let outcome = match (decision, protection_capacity_exceeded) {
            (HlsTerminalLeaseDecision::Tail(plan), false) => leases.commit_terminal_tail_if_generation_matches(
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                Arc::clone(plan),
            ),
            (HlsTerminalLeaseDecision::Tail(_), true) => leases.commit_terminal_unavailable_if_generation_matches(
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
            ),
            (
                HlsTerminalLeaseDecision::Unavailable(reason)
                | HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason),
                _,
            ) => leases
                .commit_terminal_unavailable_if_generation_matches(
                    lease_id,
                    proxy_session_id,
                    preparation,
                    now_ms,
                    *reason,
                ),
        };
        if outcome != HlsTerminalCommitOutcome::Committed {
            session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
            if outcome == HlsTerminalCommitOutcome::BundleIncompatible
                && matches!(decision, HlsTerminalLeaseDecision::Tail(_))
            {
                return Self::commit_terminal_unavailable_fallback(
                    leases,
                    session,
                    command,
                    now_ms,
                    HlsTerminalTailCompatibility::TerminalMediaNotReady,
                );
            }
            return outcome;
        }
        Self::finish_terminal_commit(leases, session, proxy_session_id);
        HlsTerminalCommitOutcome::Committed
    }

    fn commit_terminal_unavailable_fallback(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::HlsSession,
        command: &HlsTerminalCommitCommand,
        now_ms: u64,
        reason: HlsTerminalTailCompatibility,
    ) -> HlsTerminalCommitOutcome {
        let lease_id = &command.key.lease_id;
        let proxy_session_id = &command.key.proxy_session_id;
        let decision = if matches!(&command.decision, HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(_)) {
            HlsTerminalLeaseDecision::UnavailableAfterOwnerFailure(reason)
        } else {
            HlsTerminalLeaseDecision::Unavailable(reason)
        };
        match evaluate_terminal_commit_authorization(
            session,
            lease_id,
            &command.preparation,
            &decision,
            now_ms,
        ) {
            HlsTerminalCommitAuthorization::Authorized { .. } => {}
            HlsTerminalCommitAuthorization::Rejected(outcome) => return outcome,
        }
        let previous_protection = session.remove_terminal_tail_protection(lease_id);
        let outcome = leases.commit_terminal_unavailable_if_generation_matches(
            lease_id,
            proxy_session_id,
            &command.preparation,
            now_ms,
            reason,
        );
        if outcome != HlsTerminalCommitOutcome::Committed {
            session.rollback_terminal_tail_protection(lease_id.clone(), previous_protection);
            return outcome;
        }
        Self::finish_terminal_commit(leases, session, proxy_session_id);
        HlsTerminalCommitOutcome::Committed
    }

    fn finish_terminal_commit(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::HlsSession,
        proxy_session_id: &ProxySessionId,
    ) {
        let all_terminal = leases.all_live_leases_terminal_for_session(proxy_session_id);
        session.origin_control.progress_phase = if all_terminal {
            super::origin_progress::HlsOriginProgressPhase::Terminal
        } else {
            super::origin_progress::HlsOriginProgressPhase::TerminalPartial
        };
    }

    fn schedule_terminal_commit_retry(
        &self,
        key: &HlsTerminalCommitOwnerKey,
        owner_token: HlsTerminalCommitOwnerToken,
        attempts_completed: u8,
        last_attempt_at_ms: u64,
        latest_safe_terminal_commit_at_ms: u64,
    ) -> HlsTerminalCommitOutcome {
        let (retry_at_ms, attempts_completed) = match next_terminal_commit_retry(
            attempts_completed,
            last_attempt_at_ms,
            latest_safe_terminal_commit_at_ms,
        ) {
            HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed } => {
                (retry_at_ms, attempts_completed)
            }
            HlsTerminalCommitRetryDecision::AttemptsExhausted => {
                self.terminal_commit_retries.fail_owner(
                    key,
                    owner_token,
                    HlsTerminalCommitOutcome::RetryAttemptsExhausted,
                );
                return HlsTerminalCommitOutcome::RetryAttemptsExhausted;
            }
            HlsTerminalCommitRetryDecision::SafeDeadlineElapsed => {
                self.terminal_commit_retries.fail_owner(
                    key,
                    owner_token,
                    HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
                );
                return HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed;
            }
        };
        let registration_now_ms = self.terminal_commit_clock.now_ms().max(last_attempt_at_ms);
        match self
            .terminal_commit_retries
            .schedule_current(key, owner_token, attempts_completed, retry_at_ms, registration_now_ms)
        {
            HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token } => {
                if let Some(worker_token) = worker_token {
                    spawn_terminal_commit_retry_worker(
                        Arc::clone(&self.sessions),
                        Arc::clone(&self.access_leases),
                        Arc::clone(&self.terminal_commit_retries),
                        Arc::clone(&self.terminal_commit_clock),
                        worker_token,
                        Self::try_commit_access_lease_terminal_decision,
                    );
                }
                HlsTerminalCommitOutcome::LockBusy { retry_before_ms: retry_at_ms }
            }
            HlsTerminalCommitRetryScheduleDecision::Failed(outcome) => outcome,
            HlsTerminalCommitRetryScheduleDecision::Cancelled => {
                HlsTerminalCommitOutcome::SupersededGeneration
            }
            HlsTerminalCommitRetryScheduleDecision::WorkerUnavailable => {
                HlsTerminalCommitOutcome::RetryWorkerUnavailable
            }
        }
    }

    pub async fn update_access_lease_origin_acquire_policy(
        &self,
        lease_id: &HlsAccessLeaseId,
        connection_kind: crate::api::model::ConnectionKind,
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
                super::safe_hls_access_lease_id(lease_id),
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

    pub async fn touch_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> bool {
        let lease = self.access_leases.write().await.touch_access_lease_snapshot(lease_id, now_ms, timing);
        if let Some(lease) = lease {
            self.schedule_access_lease_activity(&lease).await;
            self.schedule_access_lease_validity(&lease).await;
            true
        } else {
            false
        }
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

    async fn access_lease_lifecycle_snapshot(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        self.access_leases.write().await.lifecycle_snapshot(lease_id, now_ms)
    }

    async fn remove_access_lease(&self, lease_id: &HlsAccessLeaseId) {
        self.terminal_pending.cancel_lease(lease_id);
        self.terminal_commit_retries.cancel_lease(lease_id);
        let Some(preparation) = self.access_leases.read().await.prepare_access_lease_removal(lease_id) else {
            return;
        };
        if !self.remove_prepared_access_lease(lease_id, &preparation).await {
            debug!(
                "HLS access lease removal skipped: lease={} proxy_session={} reason=lease_instance_race",
                super::safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(&preparation.proxy_session_id)
            );
        }
    }

    /// Removes one exact lease incarnation before releasing any media
    /// protection owned by it. A stale preparation is intentionally
    /// non-destructive because the replacement lease may reuse the same ID.
    async fn remove_prepared_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsAccessLeaseRemovalPreparation,
    ) -> bool {
        let removed =
            self.access_leases.write().await.remove_access_lease_if_preparation_matches(lease_id, preparation);
        if removed.is_none() {
            return false;
        }
        if let Some(session) = self.sessions.get_by_proxy_session_id(&preparation.proxy_session_id).await {
            if let Some(generation) = preparation.terminal_protection_generation {
                let removal =
                    session.write().await.remove_terminal_tail_protection_after_lease_end(lease_id, generation);
                match removal {
                    HlsTerminalTailProtectionRemoval::Removed => {
                        debug!(
                            "HLS terminal-tail protection released: lease={} proxy_session={} generation={} reason=lease_removed",
                            super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0
                        );
                    }
                    HlsTerminalTailProtectionRemoval::Missing => {}
                    HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual } => {
                        debug!(
                            "HLS stale terminal-tail protection released: lease={} proxy_session={} expected_generation={} actual_generation={} reason=lease_removed",
                            super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0,
                            actual.0
                        );
                    }
                }
            } else if session.write().await.remove_terminal_tail_protection(lease_id).is_some() {
                debug!(
                    "HLS untracked terminal-tail protection released: lease={} proxy_session={} reason=lease_removed",
                    super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&preparation.proxy_session_id)
                );
            }
        }
        self.standalone_custom_access.remove(lease_id);
        self.segment_repair.remove_access_lease_window(lease_id).await;
        self.startup_observability.remove_access_lease(lease_id);
        self.qos.remove_access_lease(lease_id).await;
        true
    }

    async fn cleanup_proxy_session_state(
        &self,
        proxy_session_id: &ProxySessionId,
        reason: &'static str,
    ) -> HlsProxySessionCleanupStats {
        self.availability_reevaluations.cancel_session(proxy_session_id);
        self.terminal_pending.cancel_session(proxy_session_id);
        self.terminal_commit_retries.cancel_session(proxy_session_id);
        let before = self.segment_repair.stats().await;
        let removed_leases = self.access_leases.write().await.remove_access_leases_for_session(proxy_session_id);
        let username = removed_leases.first().map(|lease| lease.username.clone());
        self.sessions.update_expired_session_marker_username(proxy_session_id, username).await;
        let removed_lease_ids = removed_leases.iter().map(|lease| lease.lease_id.clone()).collect::<Vec<_>>();
        for lease_id in &removed_lease_ids {
            self.standalone_custom_access.remove(lease_id);
        }
        self.segment_repair.remove_proxy_session_state(proxy_session_id, &removed_lease_ids).await;
        for lease_id in &removed_lease_ids {
            self.startup_observability.remove_access_lease(lease_id);
        }
        let removed_qos = self.qos.remove_access_leases(&removed_lease_ids).await;
        let removed_qos = removed_qos.saturating_add(self.qos.remove_proxy_session_state(proxy_session_id).await);
        let after = self.segment_repair.stats().await;
        let stats = HlsProxySessionCleanupStats {
            access_leases: removed_lease_ids.len(),
            repair_windows: before.windows.saturating_sub(after.windows),
            repair_generations: before.generations.saturating_sub(after.generations),
            repair_candidates: before.checked_candidates.saturating_sub(after.checked_candidates),
            repair_object_metadata: before.object_metadata.saturating_sub(after.object_metadata),
            repair_watchdog_metadata: before.watchdog_metadata.saturating_sub(after.watchdog_metadata),
            repair_watchdog_locks: before.watchdog_locks.saturating_sub(after.watchdog_locks),
            qos_access_leases: removed_qos,
        };
        if stats.did_cleanup() {
            debug!(
                "HLS proxy session state cleaned: proxy_session={} reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_object_metadata={} repair_watchdog_metadata={} repair_watchdog_locks={} qos_access_leases={}",
                safe_proxy_session_id(proxy_session_id),
                reason,
                stats.access_leases,
                stats.repair_windows,
                stats.repair_generations,
                stats.repair_candidates,
                stats.repair_object_metadata,
                stats.repair_watchdog_metadata,
                stats.repair_watchdog_locks,
                stats.qos_access_leases
            );
        }
        stats
    }

    pub async fn expired_session_marker(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsExpiredSessionMarker> {
        self.sessions
            .expired_session_marker(proxy_session_id, now_ms, self.session_idle_timeout_ms().saturating_mul(2).max(1))
            .await
    }

    async fn cleanup_all_runtime_state(&self, reason: &'static str) {
        self.availability_reevaluations.clear();
        self.terminal_pending.clear();
        self.terminal_commit_retries.clear();
        let removed_access_leases = self.access_leases.write().await.clear();
        self.standalone_custom_access.clear();
        self.terminal_commit_retries.clear();
        for session in self.sessions.list_sessions().await {
            session.write().await.clear_terminal_tail_protections();
        }
        self.account_overlap_cooldowns.write().await.clear();
        let removed_qos = self.qos.clear().await;
        let before = self.segment_repair.stats().await;
        self.segment_repair.clear_runtime_state().await;
        self.startup_observability.clear();
        if removed_access_leases > 0
            || before.windows > 0
            || before.generations > 0
            || before.checked_candidates > 0
            || before.metadata > 0
            || before.object_metadata > 0
            || before.locks > 0
            || before.watchdog_metadata > 0
            || before.watchdog_locks > 0
            || removed_qos > 0
        {
            debug!(
                "HLS runtime state cleaned: reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_metadata={} repair_object_metadata={} repair_locks={} repair_watchdog_metadata={} repair_watchdog_locks={} qos_access_leases={}",
                reason,
                removed_access_leases,
                before.windows,
                before.generations,
                before.checked_candidates,
                before.metadata,
                before.object_metadata,
                before.locks,
                before.watchdog_metadata,
                before.watchdog_locks,
                removed_qos
            );
        }
    }

    async fn cleanup_after_garbage_collection(&self, report: &super::GarbageCollectionReport) {
        if report.secret_cache_invalidated {
            self.cleanup_all_runtime_state("secret-cache-invalidated").await;
            return;
        }
        for proxy_session_id in &report.removed_session_ids {
            self.cleanup_proxy_session_state(proxy_session_id, "gc-session-removed").await;
        }
    }

    async fn schedule_access_lease_activity(&self, lease: &HlsAccessLease) {
        if let Some(active_until_ms) = lease.active_until_ms {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseActive {
                        lease_id: lease.lease_id.clone(),
                        proxy_session_id: lease.proxy_session_id.clone(),
                    },
                    active_until_ms,
                )
                .await;
        }
    }

    async fn schedule_access_lease_validity(&self, lease: &HlsAccessLease) {
        let due_at_ms = if lease.state == HlsAccessLeaseState::Pending {
            lease.pending_deadline_ms().unwrap_or(lease.valid_until_ms)
        } else {
            lease.valid_until_ms
        };
        self.lifecycle
            .schedule(
                HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease.lease_id.clone(),
                    proxy_session_id: lease.proxy_session_id.clone(),
                },
                due_at_ms,
            )
            .await;
    }

    async fn schedule_access_lease_lifecycle_snapshot(&self, snapshot: &HlsAccessLeaseLifecycleSnapshot) {
        if snapshot.state == HlsAccessLeaseState::Activated {
            if let Some(active_until_ms) = snapshot.active_until_ms {
                self.lifecycle
                    .schedule(
                        HlsLifecycleEventKey::AccessLeaseActive {
                            lease_id: snapshot.lease_id.clone(),
                            proxy_session_id: snapshot.proxy_session_id.clone(),
                        },
                        active_until_ms,
                    )
                    .await;
            }
        }
        if snapshot.state != HlsAccessLeaseState::Expired && snapshot.state != HlsAccessLeaseState::Denied {
            let due_at_ms = if snapshot.state == HlsAccessLeaseState::Pending {
                snapshot.pending_deadline.map_or(snapshot.valid_until_ms, HlsAccessLeasePendingDeadline::deadline_ms)
            } else {
                snapshot.valid_until_ms
            };
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: snapshot.lease_id.clone(),
                        proxy_session_id: snapshot.proxy_session_id.clone(),
                    },
                    due_at_ms,
                )
                .await;
        }
    }

    pub async fn schedule_session_idle_for_handle(&self, session: &HlsSessionHandle) {
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (proxy_session_id, due_at_ms) = {
            let session = session.read().await;
            (session.proxy_session_id.clone(), session.idle_expiry_due_at_ms(session_idle_timeout_ms))
        };
        self.lifecycle.schedule(HlsLifecycleEventKey::SessionIdle { proxy_session_id }, due_at_ms).await;
    }

    pub(crate) async fn mark_authorized_media_access_for_lease_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
    ) -> HlsMediaActivityCommitOutcome {
        self.commit_media_activity_if_identity_matches(
            session,
            lease_id,
            proxy_session_id,
            lease_identity,
            now_ms,
            HlsMediaActivityCommitKind::Access,
        )
        .await
    }

    async fn commit_media_activity_if_identity_matches(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitOutcome {
        let Some(current_session) = self.sessions.get_by_proxy_session_id(proxy_session_id).await else {
            return HlsMediaActivityCommitOutcome::StaleLeaseIdentity;
        };
        if !Arc::ptr_eq(&current_session, session) {
            return HlsMediaActivityCommitOutcome::StaleLeaseIdentity;
        }
        for attempt in 0..HLS_STATE_CAS_LOCK_RETRIES {
            match self.try_commit_media_activity(session, lease_id, proxy_session_id, lease_identity, now_ms, kind) {
                HlsMediaActivityCommitAttempt::Completed { outcome, evidence_changed } => {
                    return self
                        .finish_media_activity_commit(session, proxy_session_id, outcome, evidence_changed)
                        .await;
                }
                HlsMediaActivityCommitAttempt::LockBusy => {
                    if attempt.saturating_add(1) < HLS_STATE_CAS_LOCK_RETRIES {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        // Media activity must not be dropped under sustained lock contention.
        // Wait for one contended lock at a time, then retry the established
        // lease-store -> session acquisition without awaiting under either
        // write guard.
        let mut committed = None;
        for attempt in 0..HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES {
            let mut leases = self.access_leases.write().await;
            let Ok(mut session_guard) = session.try_write() else {
                drop(leases);
                let session_wait = session.write().await;
                drop(session_wait);
                if attempt.saturating_add(1) < HLS_MEDIA_ACTIVITY_FALLBACK_LOCK_RETRIES {
                    tokio::task::yield_now().await;
                }
                continue;
            };
            let HlsMediaActivityCommitAttempt::Completed { outcome, evidence_changed } =
                Self::commit_media_activity_locked(
                    &mut leases,
                    &mut session_guard,
                    lease_id,
                    proxy_session_id,
                    lease_identity,
                    now_ms,
                    kind,
                )
            else {
                drop(session_guard);
                drop(leases);
                continue;
            };
            drop(session_guard);
            drop(leases);
            committed = Some((outcome, evidence_changed));
            break;
        }
        let Some((outcome, evidence_changed)) = committed else {
            return HlsMediaActivityCommitOutcome::DeferredLockContention;
        };
        self.finish_media_activity_commit(session, proxy_session_id, outcome, evidence_changed).await
    }

    async fn finish_media_activity_commit(
        &self,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        outcome: HlsMediaActivityCommitOutcome,
        evidence_changed: bool,
    ) -> HlsMediaActivityCommitOutcome {
        match outcome {
            HlsMediaActivityCommitOutcome::Committed => {
                if evidence_changed {
                    self.segment_cache.notify_capacity_protection_changed();
                    self.notify_session_evidence_changed(proxy_session_id);
                }
                self.schedule_session_idle_for_handle(session).await;
            }
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity
            | HlsMediaActivityCommitOutcome::DeferredLockContention => {}
        }
        outcome
    }

    fn try_commit_media_activity(
        &self,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitAttempt {
        // Media-completion lock order matches terminal publication: lease
        // store -> session. Both locks are non-blocking and no async work runs
        // while either guard is held.
        let Ok(mut leases) = self.access_leases.try_write() else {
            return HlsMediaActivityCommitAttempt::LockBusy;
        };
        let Ok(mut session) = session.try_write() else {
            return HlsMediaActivityCommitAttempt::LockBusy;
        };
        let result = Self::commit_media_activity_locked(
            &mut leases,
            &mut session,
            lease_id,
            proxy_session_id,
            lease_identity,
            now_ms,
            kind,
        );
        drop(session);
        drop(leases);
        result
    }

    fn commit_media_activity_locked(
        leases: &mut HlsAccessLeaseStore,
        session: &mut super::HlsSession,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        lease_identity: HlsMediaLeaseIdentity,
        now_ms: u64,
        kind: HlsMediaActivityCommitKind,
    ) -> HlsMediaActivityCommitAttempt {
        if session.proxy_session_id != *proxy_session_id {
            return HlsMediaActivityCommitAttempt::Completed {
                outcome: HlsMediaActivityCommitOutcome::StaleLeaseIdentity,
                evidence_changed: false,
            };
        }
        let (current, capacity_protection_released) = match kind {
            HlsMediaActivityCommitKind::Access => (
                leases.media_identity_is_current(lease_id, proxy_session_id, lease_identity, now_ms),
                false,
            ),
            HlsMediaActivityCommitKind::LiveSegmentCompletion(token) => {
                let completion = leases.record_segment_request_completed_if_identity_matches(
                    lease_id,
                    proxy_session_id,
                    lease_identity,
                    token,
                    now_ms,
                );
                (
                    completion.is_some(),
                    matches!(completion, Some(super::HlsPlaybackCompletionOutcome::Advanced)),
                )
            }
        };
        if !current {
            return HlsMediaActivityCommitAttempt::Completed {
                outcome: HlsMediaActivityCommitOutcome::StaleLeaseIdentity,
                evidence_changed: false,
            };
        }
        session.mark_authorized_media_access(now_ms);
        HlsMediaActivityCommitAttempt::Completed {
            outcome: HlsMediaActivityCommitOutcome::Committed,
            evidence_changed: capacity_protection_released,
        }
    }

    pub async fn handle_lifecycle_event(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        event: HlsLifecycleEvent,
        now_ms: u64,
    ) {
        if !self.is_enabled() {
            return;
        }
        match event.key {
            HlsLifecycleEventKey::AccessLeaseActive { lease_id, proxy_session_id }
            | HlsLifecycleEventKey::AccessLeaseValidity { lease_id, proxy_session_id } => {
                let mut should_sync_session = false;
                if let Some(snapshot) = self.access_lease_lifecycle_snapshot(&lease_id, now_ms).await {
                    should_sync_session = true;
                    if let Some(release) = &snapshot.idle_release {
                        active_users
                            .release_session_streams_and_counted_reservation(
                                &release.username,
                                &release.user_session_token,
                            )
                            .await;
                        debug!(
                            "HLS access lease idled: lease={} proxy_session={} user_session={}",
                            super::safe_hls_access_lease_id(&release.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            super::safe_user_session_token(&release.user_session_token)
                        );
                    }
                    if matches!(snapshot.state, HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied) {
                        self.remove_access_lease(&snapshot.lease_id).await;
                        debug!(
                            "HLS access lease removed: lease={} proxy_session={} state={}",
                            super::safe_hls_access_lease_id(&snapshot.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            snapshot.state.as_log_value()
                        );
                        debug!(
                            "HLS lifecycle state snapshot: trigger=access-lease-removed {}",
                            self.debug_state_summary().await
                        );
                    } else {
                        self.schedule_access_lease_lifecycle_snapshot(&snapshot).await;
                    }
                }
                if should_sync_session {
                    if let Some(session) = self.sessions.get_by_proxy_session_id(&proxy_session_id).await {
                        self.sync_session_access_lease_count_and_detach_if_needed(
                            active_users,
                            active_provider,
                            &session,
                            &proxy_session_id,
                            now_ms,
                        )
                        .await;
                    }
                }
            }
            HlsLifecycleEventKey::SessionIdle { proxy_session_id } => {
                self.handle_session_idle_lifecycle_event(&proxy_session_id, now_ms).await;
            }
        }
    }

    async fn handle_session_idle_lifecycle_event(&self, proxy_session_id: &ProxySessionId, now_ms: u64) {
        let Some(session) = self.sessions.get_by_proxy_session_id(proxy_session_id).await else {
            return;
        };
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (key, due_at_ms, can_remove) = {
            let session = session.read().await;
            (
                session.key.clone(),
                session.idle_expiry_due_at_ms(session_idle_timeout_ms),
                session.can_expire_idle_session(now_ms, session_idle_timeout_ms),
            )
        };
        if !can_remove {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    hls_session_idle_protection_retry_at(due_at_ms, now_ms),
                )
                .await;
            return;
        }
        if self.segment_cache.has_active_temp_files_for_session(proxy_session_id) {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    now_ms.saturating_add(1_000),
                )
                .await;
            return;
        }
        let username = self.access_leases.read().await.first_username_for_session(proxy_session_id);
        if self
            .sessions
            .remove_session_marking_expired(
                &key,
                proxy_session_id,
                now_ms,
                HlsExpiredSessionReason::SessionIdleTimeout,
                username,
            )
            .await
            .is_some()
        {
            self.cleanup_proxy_session_state(proxy_session_id, "lifecycle-session-expired").await;
            if let Err(err) = self.segment_cache.delete_session_dir(proxy_session_id).await {
                error!(
                    "HLS session lifecycle cleanup failed: proxy_session={} error={err}",
                    safe_proxy_session_id(proxy_session_id)
                );
            } else {
                debug!("HLS session lifecycle expired: proxy_session={}", safe_proxy_session_id(proxy_session_id));
                debug!("HLS lifecycle state snapshot: trigger=session-expired {}", self.debug_state_summary().await);
            }
        }
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
            active_origin_work = active_origin_work.saturating_add(session.activity.active_origin_work_count);
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

    pub async fn run_garbage_collection_once(&self, now_ms: u64) -> io::Result<super::GarbageCollectionReport> {
        if !self.is_enabled() {
            return Ok(super::GarbageCollectionReport::default());
        }
        let report = self.gc.run_once(now_ms).await?;
        self.cleanup_after_garbage_collection(&report).await;
        Ok(report)
    }

    pub async fn sync_session_access_lease_count_and_detach_if_needed(
        &self,
        active_users: &Arc<ActiveUserManager>,
        _active_provider: &Arc<ActiveProviderManager>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) {
        let snapshot = self.access_lease_session_snapshot(proxy_session_id, now_ms).await;
        for release in &snapshot.idle_releases {
            active_users
                .release_session_streams_and_counted_reservation(&release.username, &release.user_session_token)
                .await;
            debug!(
                "HLS access lease idled: lease={} proxy_session={} user_session={}",
                super::safe_hls_access_lease_id(&release.lease_id),
                safe_proxy_session_id(proxy_session_id),
                super::safe_user_session_token(&release.user_session_token)
            );
        }
        {
            let mut session = session.write().await;
            session.activity.active_access_lease_count = snapshot.active_count;
            session.reconcile_effective_origin_acquire_policy(snapshot.effective_origin_policy, now_ms);
        }
    }

    pub async fn sync_all_session_access_leases_and_detach_if_needed(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        now_ms: u64,
    ) {
        if !self.is_enabled() {
            return;
        }
        for session in self.sessions.list_sessions().await {
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            self.sync_session_access_lease_count_and_detach_if_needed(
                active_users,
                active_provider,
                &session,
                &proxy_session_id,
                now_ms,
            )
            .await;
        }
    }

    pub(crate) async fn deny_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        mode: HlsAccessLeaseDenialMode,
    ) -> HlsAccessLeaseDenialOutcome {
        let outcome = {
            let mut access_leases = self.access_leases.write().await;
            access_leases.deny_access_lease(lease_id, mode)
        };
        let HlsAccessLeaseDenialOutcome::Ended { terminal_release } = &outcome else {
            return outcome;
        };
        self.terminal_pending.cancel_lease(lease_id);
        self.terminal_commit_retries.cancel_lease(lease_id);
        let Some(terminal_release) = terminal_release else {
            return outcome;
        };
        let removal = if let Some(session) =
            self.sessions.get_by_proxy_session_id(&terminal_release.proxy_session_id).await
        {
            session.write().await.remove_terminal_tail_protection_after_lease_end(lease_id, terminal_release.generation)
        } else {
            HlsTerminalTailProtectionRemoval::Missing
        };
        match removal {
            HlsTerminalTailProtectionRemoval::Removed => {
                debug!(
                    "HLS terminal-tail protection released: lease={} proxy_session={} generation={} reason=lease_denied",
                    super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&terminal_release.proxy_session_id),
                    terminal_release.generation.0
                );
            }
            HlsTerminalTailProtectionRemoval::Missing => {}
            HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual } => {
                debug!(
                    "HLS stale terminal-tail protection released: lease={} proxy_session={} expected_generation={} actual_generation={} reason=lease_denied",
                    super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&terminal_release.proxy_session_id),
                    terminal_release.generation.0,
                    actual.0
                );
            }
        }
        let acknowledged = self.access_leases.write().await.acknowledge_terminal_protection_release(
            lease_id,
            &terminal_release.proxy_session_id,
            terminal_release.generation,
        );
        if !acknowledged {
            debug!(
                "HLS terminal-tail protection release acknowledgement skipped: lease={} proxy_session={} generation={} reason=lease_generation_race",
                super::safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(&terminal_release.proxy_session_id),
                terminal_release.generation.0
            );
        }
        outcome
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
        let (session, outcome) = self
            .sessions
            .get_or_create_session_with_source_and_outcome(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
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

pub fn exec_hls_lifecycle(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let hls_proxy = Arc::clone(&app_state.hls_proxy);
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        while let Some(event) = hls_proxy.lifecycle().next_event(&cancel_token).await {
            let now_ms = current_time_millis();
            hls_proxy.handle_lifecycle_event(&active_users, &active_provider, event, now_ms).await;
        }
    });
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        super::{
            lease::{
                HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome,
                HlsLeaseManifestPublicationOutcome, HlsLeaseManifestPublicationRejectReason,
                HlsTerminalMediaRequirementSource,
            },
            manifest_acceptance::HlsManifestAcceptanceGeneration,
            prepared_terminal_bundle::prepared_terminal_bundle_key,
            recovery_timing::{
                HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsLeaseCutoverTiming,
                HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy,
                HlsRecoveryWorkload, HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow,
                HlsTerminalMediaPreparationState, HlsTransitionMarginMs,
            },
            runtime_custom_tail::HlsRuntimeCustomTailAssetIdentity,
            session::{HlsTerminalTailProtection, HLS_TERMINAL_TAIL_PROTECTION_CAPACITY},
            terminal_commit::{
                HlsTerminalAssetRevisionGuard, HlsTerminalCommitCommand, HlsTerminalCommitOutcome,
                HlsTerminalCommitOwnerKey, HlsTerminalCommitRetryDecision,
                HlsTerminalCommitRetryScheduleDecision, HlsTerminalCommitSubmissionDecision,
                HlsTerminalLeaseDecision,
            },
            terminal_tail::{snapshot_terminal_media_asset, HlsTerminalCommitMediaGuard, HlsTerminalTailPlan},
        },
        hls_acceptance_recovery_snapshot, hls_key_readiness_evidence_is_current, hls_session_idle_protection_retry_at,
        next_terminal_commit_retry, spawn_terminal_commit_retry_worker, HlsCriticalHandoffStateAccess,
        HlsMediaActivityCommitOutcome, HlsProxyManager, HlsRecoveryExecutionState, HlsTerminalCommitPayload,
        HlsTerminalCommitRequest,
        HlsTerminalTailPreparationRequest, HLS_SESSION_IDLE_PROTECTION_RETRY_MS,
    };
    use crate::{
        api::model::{
            build_terminal_tail_plan, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
            HlsAccessLeaseState, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
            HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot, HlsManifestAcceptanceExhaustionReason,
            HlsManifestAcceptanceTrigger, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker, HlsMediaContainer,
            HlsMediaLeaseIdentity, HlsPlaybackFamilyKey, HlsSegmentFailureObject, HlsSessionHandle, HlsSessionKey,
            HlsSessionStoreOutcome, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
            HlsTerminalBaseSegmentAvailability, HlsTerminalTailBuildInput, HlsTerminalTailCompatibility,
            HlsTerminalTailGeneration, ProxySessionId, TransportStreamBuffer, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        },
        model::{AppConfig, Config, HlsCacheConfig, MediaToolCapabilities, ReverseProxyConfig, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{
        ConfigPaths, HlsCacheConfigDto, HlsManifestRecoveryBurstLevel, HlsStripConfigDto, HlsStripMode,
        ReverseProxyConfigDto,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    const TERMINAL_ASSET_BYTES: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/channel_unavailable.ts"));

    #[test]
    fn protected_idle_session_retry_cannot_form_a_millisecond_busy_loop() {
        let now_ms = 50_000_u64;
        assert_eq!(
            hls_session_idle_protection_retry_at(now_ms.saturating_sub(1), now_ms),
            now_ms.saturating_add(HLS_SESSION_IDLE_PROTECTION_RETRY_MS)
        );
        assert_eq!(hls_session_idle_protection_retry_at(60_000, now_ms), 60_000);
    }

    #[test]
    fn key_readiness_evidence_is_valid_at_expiry_and_stale_one_millisecond_later() {
        assert!(hls_key_readiness_evidence_is_current(Some(50_000), 50_000));
        assert!(!hls_key_readiness_evidence_is_current(Some(50_000), 50_001));
        assert!(hls_key_readiness_evidence_is_current(None, u64::MAX));
    }

    fn cutover_reserve() -> HlsLeaseReserveSnapshot {
        let transition_margin = HlsTransitionMarginMs::from_millis(12_000);
        let guaranteed_reserve_ms = transition_margin
            .as_millis()
            .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
        HlsLeaseReserveSnapshot {
            availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
            guaranteed_media_horizon_ms: 12_000_u64.saturating_add(guaranteed_reserve_ms),
            conservative_playback_position_ms: 12_000,
            guaranteed_reserve_ms,
            initial_hidden_ready_duration_ms: 0,
            transition_margin,
            key_readiness_valid_until_ms: None,
            recovery_required: true,
            cutover_required: false,
        }
    }

    fn empty_paths() -> ConfigPaths {
        ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    fn test_app_config(config: Config) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(config)),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(empty_paths())),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [7; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn config_with_hls_cache(hls_cache: HlsCacheConfigDto) -> Config {
        Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                hls_cache: Some(hls_cache),
                ..ReverseProxyConfigDto::default()
            })),
            ..Config::default()
        }
    }

    fn access_lease(lease_id: &str, proxy_session_id: &ProxySessionId) -> HlsAccessLease {
        HlsAccessLease::pending(
            HlsAccessLeaseId(lease_id.to_string()),
            HlsPlaybackFamilyKey::new("alice", "client-a"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "session-a".to_string(),
            1,
            "stream-a".to_string(),
            12345,
            1_000,
            60_000,
        )
    }

    fn manifest_snapshot(source_rendered_at_ms: u64) -> HlsLeaseManifestSnapshot {
        HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(source_rendered_at_ms),
            snapshot_generation: 0,
            delivered_at_ms: 2_000,
            first_proxy_seq: 40,
            last_proxy_seq: 41,
            visible_segments: Arc::from([
                HlsLeaseManifestSegment {
                    proxy_seq: 40,
                    duration_ms: 6_000,
                    uri: "/live/40.ts".to_string(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
                HlsLeaseManifestSegment {
                    proxy_seq: 41,
                    duration_ms: 6_000,
                    uri: "/live/41.ts".to_string(),
                    discontinuity_before: false,
                    map_ref_ready: true,
                    encryption: None,
                },
            ]),
            discontinuity_sequence: 3,
            target_duration_ms: 12_000,
            playlist_duration_ms: 12_000,
            last_visible_media_end_ms: 12_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        }
    }

    async fn publish_manifest_snapshot(
        manager: &HlsProxyManager,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        snapshot: HlsLeaseManifestSnapshot,
        now_ms: u64,
    ) -> bool {
        let Some(guard) = manager.prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms).await
        else {
            return false;
        };
        manager
            .commit_access_lease_manifest_publication(lease_id, proxy_session_id, guard, snapshot, now_ms)
            .await
            .is_committed()
    }

    fn terminal_plan(
        generation: u64,
        proxy_session_id: &ProxySessionId,
        lease_id: &HlsAccessLeaseId,
    ) -> Arc<super::HlsTerminalTailPlan> {
        let mut base_manifest = manifest_snapshot(1);
        base_manifest.snapshot_generation = 1;
        for segment in Arc::make_mut(&mut base_manifest.visible_segments) {
            segment.uri = format!("/hls/shared/live/{}/{}/{}.ts", proxy_session_id.0, lease_id.0, segment.proxy_seq);
        }
        let transport_stream = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let asset =
            super::super::terminal_tail::snapshot_terminal_media_asset(&transport_stream).expect("terminal asset");
        let expected_asset =
            HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(&asset));
        let availability = Arc::from(
            base_manifest
                .visible_proxy_seqs()
                .map(|proxy_seq| HlsTerminalBaseSegmentAvailability {
                    proxy_seq,
                    media_state: HlsTerminalBaseMediaState::Ready,
                    required_map_ready: true,
                    required_key_ready: true,
                    protection: HlsTerminalBaseProtection::Protectable,
                })
                .collect::<Vec<_>>(),
        );
        let base_track_signature = Some(asset.track_signature().clone());
        let anchored_bundle = HlsTerminalTailBuildInput::anchored_bundle_for_test(
            &asset,
            base_manifest.target_duration_ms,
        );
        let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &base_manifest));
        let base_splice_evidence =
            Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
        let terminal_splice_evidence = base_splice_evidence.clone();
        Arc::new(
            build_terminal_tail_plan(HlsTerminalTailBuildInput {
                generation: HlsTerminalTailGeneration(generation),
                created_at_ms: 3_000,
                base_manifest,
                base_availability: availability,
                base_track_signature,
                base_splice_evidence,
                terminal_splice_evidence,
                base_timing,
                base_key_bindings: Arc::from([]),
                expected_asset,
                asset,
                anchored_bundle,
            })
            .expect("compatible terminal plan"),
        )
    }

    fn commit_terminal_plan(
        manager: &HlsProxyManager,
        session: &HlsSessionHandle,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        preparation: &super::super::lease::HlsTerminalTailPreparation,
        now_ms: u64,
        plan: Arc<HlsTerminalTailPlan>,
    ) -> HlsTerminalCommitOutcome {
        let asset_revision_guard = HlsTerminalAssetRevisionGuard::matching_runtime_for_test(plan.asset_identity);
        manager
            .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                session,
                lease_id,
                proxy_session_id,
                preparation,
                now_ms,
                payload: HlsTerminalCommitPayload::Tail {
                    plan,
                    media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
                },
                asset_revision_guard,
            })
    }

    async fn live_media_fixture(
        lease_name: &str,
    ) -> (HlsProxyManager, HlsSessionHandle, ProxySessionId, HlsAccessLeaseId, HlsMediaLeaseIdentity) {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId(lease_name.to_string());
        let lease = access_lease(lease_name, &proxy_session_id);
        let lease_identity = lease.media_identity().expect("live media identity");
        manager.access_leases.write().await.prepare_access_lease(lease);
        (manager, session, proxy_session_id, lease_id, lease_identity)
    }

    #[tokio::test]
    async fn critical_handoff_distinguishes_lock_busy_from_acquired_state() {
        let (manager, session, _, _, _) = live_media_fixture("critical-lock-busy").await;
        let lease_guard = manager.access_leases.write().await;

        let busy = manager.with_critical_handoff_state(&session, |_, _| 7_u8).await;

        assert_eq!(busy, HlsCriticalHandoffStateAccess::LockBusy);
        drop(lease_guard);

        let acquired = manager.with_critical_handoff_state(&session, |_, _| 7_u8).await;

        assert_eq!(acquired, HlsCriticalHandoffStateAccess::Acquired(7));
    }

    #[tokio::test]
    async fn late_live_completion_after_terminal_transition_updates_neither_cursor_nor_session_activity() {
        let (manager, session, proxy_session_id, lease_id, live_identity) = live_media_fixture("late-terminal").await;
        let token = manager
            .record_access_lease_segment_request_started_if_identity_matches(
                &lease_id,
                &proxy_session_id,
                live_identity,
                40,
                2_000,
            )
            .await
            .expect("current live request token");
        let plan = terminal_plan(7, &proxy_session_id, &lease_id);
        {
            let mut leases = manager.access_leases.write().await;
            let mut lease = leases.remove_access_lease(&lease_id).expect("live lease");
            lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(plan);
            leases.prepare_access_lease(lease);
        }

        let outcome = manager
            .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                &session,
                &lease_id,
                &proxy_session_id,
                live_identity,
                token,
                2_100,
            )
            .await;

        assert_eq!(outcome, HlsMediaActivityCommitOutcome::StaleLeaseIdentity);
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);
        let lease =
            manager.access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100).await.expect("terminal lease");
        assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
    }

    #[tokio::test]
    async fn contiguous_live_completion_wakes_capacity_protection_waiters() {
        let (manager, session, proxy_session_id, lease_id, live_identity) = live_media_fixture("capacity-release").await;
        let revision = manager.segment_cache.capacity_revision();
        let mut capacity_wait = Box::pin(manager.segment_cache.wait_for_capacity_change(&revision));
        assert!(matches!(futures::poll!(capacity_wait.as_mut()), std::task::Poll::Pending));
        let token = manager
            .record_access_lease_segment_request_started_if_identity_matches(
                &lease_id,
                &proxy_session_id,
                live_identity,
                40,
                2_000,
            )
            .await
            .expect("current live request token");

        let outcome = manager
            .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                &session,
                &lease_id,
                &proxy_session_id,
                live_identity,
                token,
                2_100,
            )
            .await;

        assert_eq!(outcome, HlsMediaActivityCommitOutcome::Committed);
        assert!(matches!(futures::poll!(capacity_wait.as_mut()), std::task::Poll::Ready(())));
    }

    #[tokio::test]
    async fn expired_and_denied_lease_identities_cannot_extend_shared_session_activity() {
        let (expired_manager, expired_session, expired_proxy, expired_lease_id, expired_identity) =
            live_media_fixture("expired-media").await;
        {
            let mut leases = expired_manager.access_leases.write().await;
            let mut lease = leases.remove_access_lease(&expired_lease_id).expect("expiring lease");
            lease.pending_deadline = Some(HlsAccessLeasePendingDeadline::Bootstrap { deadline_ms: 2_000 });
            lease.valid_until_ms = 2_000;
            leases.prepare_access_lease(lease);
        }
        assert_eq!(
            expired_manager
                .mark_authorized_media_access_for_lease_if_identity_matches(
                    &expired_session,
                    &expired_lease_id,
                    &expired_proxy,
                    expired_identity,
                    2_000,
                )
                .await,
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity
        );
        assert_eq!(expired_session.read().await.activity.last_authorized_media_at_ms, None);

        let (denied_manager, denied_session, denied_proxy, denied_lease_id, denied_identity) =
            live_media_fixture("denied-media").await;
        assert!(matches!(
            denied_manager
                .access_leases
                .write()
                .await
                .deny_access_lease(&denied_lease_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::Ended { terminal_release: None }
        ));
        assert_eq!(
            denied_manager
                .mark_authorized_media_access_for_lease_if_identity_matches(
                    &denied_session,
                    &denied_lease_id,
                    &denied_proxy,
                    denied_identity,
                    2_000,
                )
                .await,
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity
        );
        assert_eq!(denied_session.read().await.activity.last_authorized_media_at_ms, None);
    }

    #[tokio::test]
    async fn current_terminal_identity_waits_for_lock_contention_and_marks_activity() {
        let (manager, session, proxy_session_id, lease_id, _) = live_media_fixture("terminal-media").await;
        let manager = Arc::new(manager);
        let terminal_identity = {
            let mut leases = manager.access_leases.write().await;
            let mut lease = leases.remove_access_lease(&lease_id).expect("live lease");
            lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(terminal_plan(7, &proxy_session_id, &lease_id));
            let identity = lease.media_identity().expect("terminal identity");
            leases.prepare_access_lease(lease);
            identity
        };
        assert_eq!(
            manager
                .mark_authorized_media_access_for_lease_if_identity_matches(
                    &session,
                    &lease_id,
                    &proxy_session_id,
                    terminal_identity,
                    2_000,
                )
                .await,
            HlsMediaActivityCommitOutcome::Committed
        );
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(2_000));

        session.write().await.activity.last_authorized_media_at_ms = None;
        let lease_guard = manager.access_leases.write().await;
        let task_manager = Arc::clone(&manager);
        let task_session = Arc::clone(&session);
        let task_lease_id = lease_id.clone();
        let task_proxy_session_id = proxy_session_id.clone();
        let commit_task = tokio::spawn(async move {
            task_manager
                .mark_authorized_media_access_for_lease_if_identity_matches(
                    &task_session,
                    &task_lease_id,
                    &task_proxy_session_id,
                    terminal_identity,
                    2_100,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!commit_task.is_finished(), "current media activity must wait rather than be dropped");
        drop(lease_guard);
        assert_eq!(commit_task.await.expect("controlled commit task"), HlsMediaActivityCommitOutcome::Committed);
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(2_100));
    }

    #[tokio::test]
    async fn stale_session_handle_cannot_mark_recovered_session_with_same_public_id() {
        let (manager, stale_session, proxy_session_id, lease_id, lease_identity) =
            live_media_fixture("session-recovery").await;
        let session_key = stale_session.read().await.key.clone();
        manager.sessions.remove_session(&session_key, &proxy_session_id).await.expect("remove original session");
        let (recovered_session, outcome) =
            manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
        assert_eq!(outcome, HlsSessionStoreOutcome::Created);
        assert!(!Arc::ptr_eq(&stale_session, &recovered_session));

        assert_eq!(
            manager
                .mark_authorized_media_access_for_lease_if_identity_matches(
                    &stale_session,
                    &lease_id,
                    &proxy_session_id,
                    lease_identity,
                    2_100,
                )
                .await,
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity
        );
        assert_eq!(stale_session.read().await.activity.last_authorized_media_at_ms, None);
        assert_eq!(recovered_session.read().await.activity.last_authorized_media_at_ms, None);
    }

    fn begin_test_acceptance_episode(
        session: &mut super::super::HlsSession,
        now_ms: u64,
    ) -> HlsManifestAcceptanceGeneration {
        let burst_plan = HlsManifestRecoveryBurstLevel::Beast.plan();
        let terminal_buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
        let terminal_asset = snapshot_terminal_media_asset(&terminal_buffer).expect("terminal asset");
        let terminal_key =
            prepared_terminal_bundle_key(&terminal_asset, 12_000, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let required_terminal_media_key = Some(terminal_key);
        let terminal_media_preparation = HlsTerminalMediaPreparationState::Preparing { key: terminal_key };
        let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: now_ms,
            burst_plan,
            target_duration_ms: 12_000,
            transition_margin: HlsTransitionMarginMs::from_millis(12_000),
            workload: HlsRecoveryWorkload::clear_fetch(),
            observed_latency: HlsObservedRecoveryLatency::default(),
            required_terminal_media_key,
            terminal_media_preparation,
            policy: HlsRecoveryTimingPolicy::new(
                HlsOperationTimeoutMs::from_millis(1_000),
                HlsOperationTimeoutMs::from_millis(2_000),
                HlsRecoveryEtaMs::from_millis(300),
                HlsRecoveryEtaMs::from_millis(400),
            ),
        });
        session
            .origin_control
            .begin_acceptance_episode(now_ms, burst_plan, HlsManifestAcceptanceTrigger::RecoveryRequired, &timing)
    }

    fn complete_failed_acceptance_episode(session: &mut super::super::HlsSession, now_ms: u64) -> u64 {
        let generation = begin_test_acceptance_episode(session, now_ms);
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        assert_eq!(episode.generation, generation);
        episode.record_full_burst();
        episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
        episode.hold_after_uncommitted_burst(None, Some(now_ms.saturating_add(1_000)));
        session.origin_control.progress_generation
    }

    async fn prepared_terminal_commit_fixture(
        lease_name: &str,
    ) -> (
        HlsProxyManager,
        HlsSessionHandle,
        ProxySessionId,
        HlsAccessLeaseId,
        super::super::lease::HlsTerminalTailPreparation,
        Arc<HlsTerminalTailPlan>,
    ) {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        manager.terminal_commit_clock.set_fixed_now_ms(2_000);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, lease_name), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId(lease_name.to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(lease_name, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");
        let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
        (manager, session, proxy_session_id, lease_id, preparation, plan)
    }

    #[tokio::test]
    async fn hls_terminal_commit_missing_acceptance_exact_ready_bundle_commits_tail() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        manager.terminal_commit_clock.set_fixed_now_ms(2_000);
        let (session, _) = manager
            .get_or_create_session_with_outcome(HlsSessionKey::new(1, "missing-acceptance"), b"secret", 1_000)
            .await;
        let (proxy_session_id, progress_generation) = {
            let session = session.read().await;
            assert!(session.origin_control.acceptance_episode.is_none());
            (session.proxy_session_id.clone(), session.origin_control.progress_generation)
        };
        let lease_id = HlsAccessLeaseId("missing-acceptance".to_string());
        manager
            .access_leases
            .write()
            .await
            .prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let mut preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("missing acceptance still permits cutover-local terminal preparation");
        assert_eq!(
            preparation.terminal_media_requirement_source,
            HlsTerminalMediaRequirementSource::CutoverSnapshotPending {
                decision_generation: preparation.decision_generation,
            }
        );
        let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
        let prepared_key = plan.media_preparation_key();
        preparation
            .bind_ready_terminal_media_requirement(prepared_key)
            .expect("the exact ready bundle binds to the cutover decision");
        assert_eq!(
            preparation.terminal_media_requirement_source,
            HlsTerminalMediaRequirementSource::CutoverSnapshot {
                decision_generation: preparation.decision_generation,
                asset: prepared_key.asset,
            }
        );

        let outcome = commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_000,
            plan,
        )
        ;

        assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("committed terminal lease");
        assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)));
    }

    async fn wait_for_terminal_commit_owner(manager: &HlsProxyManager) {
        for _ in 0..256 {
            if manager.terminal_commit_retries.owner_count() == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(manager.terminal_commit_retries.owner_count(), 0, "terminal retry owner did not finish");
    }

    #[test]
    fn hls_cutover_policy_manager_uses_only_matching_acceptance_work_as_recovery_evidence() {
        let mut session = super::super::HlsSession::new(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000);
        let generation = begin_test_acceptance_episode(&mut session, 2_000);
        session.origin_refresh.in_flight = false;

        assert!(matches!(
            hls_acceptance_recovery_snapshot(&session, 2_100).recovery,
            HlsRecoveryExecutionState::InFlight { .. }
        ));

        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        episode.record_full_burst();
        episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
        session.origin_refresh.in_flight = true;

        let snapshot = hls_acceptance_recovery_snapshot(&session, 2_100);
        assert_eq!(snapshot.expected_generation, generation);
        assert_eq!(snapshot.recovery, HlsRecoveryExecutionState::Idle);
    }

    fn terminal_preparation_request<'a>(
        lease_id: &'a HlsAccessLeaseId,
        proxy_session_id: &'a ProxySessionId,
        origin_progress_generation: u64,
    ) -> HlsTerminalTailPreparationRequest<'a> {
        let reserve = cutover_reserve();
        let cutover_timing = HlsLeaseCutoverTiming::from_reserve(
            2_000,
            reserve.guaranteed_reserve_ms,
            reserve.transition_margin,
            None,
        );
        HlsTerminalTailPreparationRequest {
            lease_id,
            proxy_session_id,
            manifest_snapshot_generation: 1,
            cursor_generation: 0,
            reserve,
            cutover_timing,
            commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
            now_ms: 2_000,
            origin_progress_generation,
            media_readiness_generation: 0,
            last_media_progress_at_ms: None,
        }
    }

    #[tokio::test]
    async fn manifest_publication_manager_rejects_delayed_source_and_expiry_races() {
        let manager = HlsProxyManager::new();
        let proxy_session_id = ProxySessionId("proxy".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        let older_request = manager
            .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("older request guard");
        let newer_request = manager
            .prepare_access_lease_manifest_publication(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("newer request guard");

        assert_eq!(
            manager
                .commit_access_lease_manifest_publication(
                    &lease_id,
                    &proxy_session_id,
                    newer_request,
                    manifest_snapshot(20),
                    2_100,
                )
                .await,
            HlsLeaseManifestPublicationOutcome::Committed { snapshot_generation: 1 }
        );
        assert_eq!(
            manager
                .commit_access_lease_manifest_publication(
                    &lease_id,
                    &proxy_session_id,
                    older_request,
                    manifest_snapshot(10),
                    2_200,
                )
                .await,
            HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::SourceRegressive)
        );

        let expiry_lease_id = HlsAccessLeaseId("expiry".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&expiry_lease_id.0, &proxy_session_id));
        let expiry_request = manager
            .prepare_access_lease_manifest_publication(&expiry_lease_id, &proxy_session_id, 2_000)
            .await
            .expect("pre-expiry request guard");
        assert_eq!(
            manager
                .commit_access_lease_manifest_publication(
                    &expiry_lease_id,
                    &proxy_session_id,
                    expiry_request,
                    manifest_snapshot(30),
                    61_000,
                )
                .await,
            HlsLeaseManifestPublicationOutcome::Rejected(HlsLeaseManifestPublicationRejectReason::LeaseExpired)
        );
    }

    #[tokio::test]
    async fn update_config_applies_hls_runtime_settings_to_existing_manager() {
        let initial_dto = HlsCacheConfigDto {
            cache_path: Some("/tmp/tuliprox/hls-a".to_string()),
            max_segments_prefetch: 1,
            ..Default::default()
        };
        let initial_config = HlsCacheConfig::from(&initial_dto);
        let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100).await;

        let mut updated_dto = initial_dto.clone();
        updated_dto.max_segments_prefetch = 4;
        updated_dto.max_concurrent_segment_fetches_per_session = 5;
        updated_dto.max_concurrent_segment_fetches_global = 6;
        updated_dto.origin_manifest_timeout_ms = 1_234;
        updated_dto.origin_segment_timeout_ms = 5_678;
        updated_dto.cache_duration = 99;
        updated_dto.session_idle_timeout = 55;
        updated_dto.manifest_recovery_burst.level = shared::model::HlsManifestRecoveryBurstLevel::Balanced;
        updated_dto.strip = HlsStripConfigDto { mode: HlsStripMode::Seconds, value: 7 };
        let app_config = test_app_config(config_with_hls_cache(updated_dto));

        manager.update_config(&app_config).await;

        assert_eq!(manager.segment_fetch_policy().max_prefetch_queue_depth, 4);
        assert_eq!(manager.segment_fetch_policy().max_session_segment_fetches, 5);
        assert_eq!(manager.segment_fetch_policy().max_global_segment_fetches, 6);
        assert_eq!(manager.segment_fetch_policy().origin_segment_timeout_ms, 5_678);
        assert_eq!(manager.origin_manifest_timeout_ms(), 1_234);
        assert_eq!(manager.cache_duration_seconds(), 99);
        assert_eq!(manager.transient_resource_ttl_ms(), 99_000);
        assert_eq!(manager.session_idle_timeout_ms(), 55_000);
        assert_eq!(manager.manifest_recovery_burst().level, HlsManifestRecoveryBurstLevel::Balanced);
        assert_eq!(manager.strip().mode, HlsStripMode::Seconds);
        assert_eq!(manager.strip().value, 7);
        assert_eq!(session.read().await.segment_prefetch_queue.max_prefetch_depth(), 4);
    }

    #[tokio::test]
    async fn optional_hls_config_controls_runtime_enabled_state() {
        let manager = HlsProxyManager::from_hls_cache_config(None);

        assert!(!manager.is_enabled());
        assert_eq!(
            manager.run_garbage_collection_once(1_000).await.expect("disabled gc should no-op"),
            super::super::GarbageCollectionReport::default()
        );

        let app_config = test_app_config(config_with_hls_cache(HlsCacheConfigDto::default()));
        manager.update_config(&app_config).await;

        assert!(manager.is_enabled());
    }

    #[tokio::test]
    async fn update_config_cache_path_change_clears_runtime_cache_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let old_cache = temp_dir.path().join("old");
        let new_cache = temp_dir.path().join("new");
        let initial_dto =
            HlsCacheConfigDto { cache_path: Some(old_cache.to_string_lossy().to_string()), ..Default::default() };
        let initial_config = HlsCacheConfig::from(&initial_dto);
        let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
        let _ = manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100).await;
        assert_eq!(manager.sessions().len().await, 1);

        let mut updated_dto = initial_dto;
        updated_dto.cache_path = Some(new_cache.to_string_lossy().to_string());
        let app_config = test_app_config(config_with_hls_cache(updated_dto));

        manager.update_config(&app_config).await;

        assert!(manager.sessions().is_empty().await);
        assert_eq!(manager.segment_cache().cache_path(), new_cache);
    }

    #[tokio::test]
    async fn repeated_segment_failure_counter_does_not_terminalize_access_lease() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));

        {
            let mut session = session.write().await;
            for failure in 0_u64..100 {
                let _ = session.record_temporary_segment_fetch_failure(
                    2_000_u64.saturating_add(failure),
                    HlsSegmentFailureObject::Normal { proxy_seq: 40, origin_seq: 400 },
                    1,
                );
            }
            assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 100);
        }

        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
            .await
            .expect("failure tracking must not remove the lease");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_cutover_policy_media_progress_supersedes_prepared_plan() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("failed acceptance permits terminal preparation");

        {
            let mut session = session.write().await;
            session.origin_control.record_media_progress(2_100, 12_000);
            assert_eq!(session.origin_control.progress_generation, progress_generation.saturating_add(1));
        }
        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_100,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::RecoveryCommitted
        );

        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_200)
            .await
            .expect("lease remains available");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_cutover_policy_matching_acceptance_commit_without_media_progress_terminalizes_lease() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");

        {
            let mut session = session.write().await;
            session.origin_control.acceptance_episode.as_mut().expect("acceptance episode").complete();
            assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
        }

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_100,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::Committed
        );
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
                .await
                .expect("terminal lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
    }

    #[tokio::test]
    async fn hls_cutover_policy_new_acceptance_episode_without_media_progress_does_not_supersede_cutover() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");

        {
            let mut session = session.write().await;
            let previous_acceptance_generation = session.origin_control.acceptance_generation;
            let current_acceptance_generation = begin_test_acceptance_episode(&mut session, 2_050);
            assert_eq!(
                session.origin_control.acceptance_generation.0,
                previous_acceptance_generation.0.saturating_add(1)
            );
            assert_eq!(current_acceptance_generation, session.origin_control.acceptance_generation);
            assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
        }

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_100,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::Committed
        );
    }

    #[tokio::test]
    async fn hls_terminal_commit_changed_asset_revision_fails_closed_before_tail_mutation() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("asset-revision").await;
        let expected_asset = plan.asset_identity;

        let outcome = manager
            .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                session: &session,
                lease_id: &lease_id,
                proxy_session_id: &proxy_session_id,
                preparation: &preparation,
                now_ms: 2_000,
                payload: HlsTerminalCommitPayload::Tail {
                    plan,
                    media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
                },
                asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, || None),
            });

        assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("asset mismatch terminalizes the lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::MissingAsset,
                ..
            }
        ));
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_retry_asset_change_autonomously_fails_closed() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-asset-change").await;
        let expected_asset = plan.asset_identity;
        let asset_is_current = Arc::new(AtomicBool::new(true));
        let revision_guard = {
            let asset_is_current = Arc::clone(&asset_is_current);
            HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, move || {
                asset_is_current.load(Ordering::Acquire).then_some(expected_asset)
            })
        };
        let session_guard = session.write().await;

        assert!(matches!(
            manager
                .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                    session: &session,
                    lease_id: &lease_id,
                    proxy_session_id: &proxy_session_id,
                    preparation: &preparation,
                    now_ms: 2_000,
                    payload: HlsTerminalCommitPayload::Tail {
                        plan,
                        media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
                    },
                    asset_revision_guard: revision_guard,
                }),
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        asset_is_current.store(false, Ordering::Release);
        drop(session_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("asset change terminalizes without another request")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::MissingAsset,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn hls_terminal_commit_exact_replay_stays_idempotent_after_asset_revision_change() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("asset-replay").await;
        let expected_asset = plan.asset_identity;

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                Arc::clone(&plan),
            )
            ,
            HlsTerminalCommitOutcome::Committed
        );
        let replay = manager
            .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                session: &session,
                lease_id: &lease_id,
                proxy_session_id: &proxy_session_id,
                preparation: &preparation,
                now_ms: 2_000,
                payload: HlsTerminalCommitPayload::Tail {
                    plan,
                    media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
                },
                asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, || None),
            });

        assert_eq!(replay, HlsTerminalCommitOutcome::AlreadyCommitted);
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("terminal replay stays available")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
    }

    #[tokio::test]
    async fn hls_terminal_commit_retry_lock_busy_without_a_second_client_request() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        manager.terminal_commit_clock.set_fixed_now_ms(2_000);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");
        let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);

        let session_guard = session.write().await;
        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                Arc::clone(&plan),
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 2_001 }
        );
        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 2_001 }
        );
        assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
        drop(session_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("autonomous retry keeps the lease available")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
    }

    #[tokio::test]
    async fn hls_terminal_commit_submission_delayed_stale_incoming_preserves_authoritative_tail() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("submission-stale-incoming").await;
        let current_asset = plan.asset_identity;
        let stale_asset = HlsTerminalAssetIdentity { revision: 999, fingerprint: [9; 32] };
        let Some(session_incarnation) = manager.sessions.session_incarnation(&session) else {
            panic!("fixture session has an incarnation");
        };
        let Some((cancellation_epoch, submission_token)) = manager.terminal_commit_retries.reserve_submission() else {
            panic!("fixture can reserve terminal submission");
        };
        let owner_key = HlsTerminalCommitOwnerKey::from_preparation(&proxy_session_id, &lease_id, &preparation);
        let command = HlsTerminalCommitCommand {
            key: owner_key,
            session: Arc::clone(&session),
            session_incarnation,
            preparation: preparation.clone(),
            decision: HlsTerminalLeaseDecision::Tail(Arc::clone(&plan)),
            media_guard: Some(HlsTerminalCommitMediaGuard::empty_for_test()),
            asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_runtime_for_test(current_asset),
            cancellation_epoch,
            submission_token,
        };
        let HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token } =
            manager.terminal_commit_retries.submit(command, 2_000)
        else {
            panic!("authoritative tail owns the initial submission");
        };
        let HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed } =
            next_terminal_commit_retry(
                1,
                2_000,
                preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
            )
        else {
            panic!("fixture has retry budget");
        };
        let HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token: Some(worker_token) } = manager
            .terminal_commit_retries
            .schedule_current(&command.key, owner_token, attempts_completed, retry_at_ms, 2_000)
        else {
            panic!("authoritative tail schedules one bounded worker");
        };

        let mut stale_preparation = preparation.clone();
        stale_preparation.cutover_timing = HlsLeaseCutoverTiming::from_reserve(
            2_000,
            preparation.reserve.transition_margin.as_millis().saturating_add(2),
            preparation.reserve.transition_margin,
            None,
        );
        let outcome = manager
            .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                session: &session,
                lease_id: &lease_id,
                proxy_session_id: &proxy_session_id,
                preparation: &stale_preparation,
                now_ms: 2_000,
                payload: HlsTerminalCommitPayload::Unavailable(
                    HlsTerminalTailCompatibility::AssetRevisionMismatch,
                ),
                asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(
                    HlsRuntimeCustomTailAssetIdentity::new(current_asset.reason, stale_asset),
                    move || Some(current_asset),
                ),
            });

        assert_eq!(outcome, HlsTerminalCommitOutcome::LockBusy { retry_before_ms: retry_at_ms });
        assert_eq!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("unauthorized incoming keeps lease available")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        );
        spawn_terminal_commit_retry_worker(
            Arc::clone(&manager.sessions),
            Arc::clone(&manager.access_leases),
            Arc::clone(&manager.terminal_commit_retries),
            Arc::clone(&manager.terminal_commit_clock),
            worker_token,
            HlsProxyManager::try_commit_access_lease_terminal_decision,
        );
        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_001)
                .await
                .expect("authorized owner terminalizes the lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
    }

    #[tokio::test]
    async fn hls_terminal_commit_submission_newer_command_replaces_lock_busy_owner() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-owner-replacement").await;
        let stale_asset_identity = plan.asset_identity;
        let session_guard = session.write().await;

        assert!(matches!(
            manager
                .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                    session: &session,
                    lease_id: &lease_id,
                    proxy_session_id: &proxy_session_id,
                    preparation: &preparation,
                    now_ms: 2_000,
                    payload: HlsTerminalCommitPayload::Tail {
                        plan,
                        media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
                    },
                    asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(
                        stale_asset_identity,
                        || None,
                    ),
                }),
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert!(matches!(
            manager
                .commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
                    session: &session,
                    lease_id: &lease_id,
                    proxy_session_id: &proxy_session_id,
                    preparation: &preparation,
                    now_ms: 2_000,
                    payload: HlsTerminalCommitPayload::Unavailable(
                        HlsTerminalTailCompatibility::AssetRevisionMismatch,
                    ),
                    asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_for_test(None),
                }),
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
        drop(session_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("replacement command terminalizes the lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::AssetRevisionMismatch,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn hls_terminal_commit_retry_never_commits_after_the_safe_deadline() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-deadline").await;
        let after_deadline_ms = preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch()
            .saturating_add(1);
        manager.terminal_commit_clock.set_fixed_now_ms(after_deadline_ms);
        let session_guard = session.write().await;

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed
        );
        assert_eq!(manager.terminal_commit_retries.owner_count(), 0);
        drop(session_guard);

        assert_eq!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, after_deadline_ms)
                .await
                .expect("deadline-expired lease remains live")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        );
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_initial_and_retry_attempts_reject_exclusive_safe_deadline() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("initial-exclusive-deadline").await;
        let safe_deadline_ms = preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch();
        manager.terminal_commit_clock.set_fixed_now_ms(safe_deadline_ms);

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed
        );
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, safe_deadline_ms)
                .await
                .expect("exclusive deadline leaves the initial-attempt lease live")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        ));
        assert!(!session.read().await.has_terminal_tail_protections());

        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-exclusive-deadline").await;
        let safe_deadline_ms = preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch();
        let session_guard = session.write().await;
        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
        manager.terminal_commit_clock.set_fixed_now_ms(safe_deadline_ms);
        drop(session_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, safe_deadline_ms)
                .await
                .expect("exclusive deadline leaves the retry lease live")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        ));
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_session_replacement_cancels_the_detached_retry() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-session-replacement").await;
        let session_key = session.read().await.key.clone();
        let lease_guard = manager.access_leases.write().await;

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        manager
            .sessions
            .remove_session(&session_key, &proxy_session_id)
            .await
            .expect("remove prepared session");
        let (replacement, outcome) =
            manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
        assert_eq!(outcome, HlsSessionStoreOutcome::Created);
        assert!(!Arc::ptr_eq(&session, &replacement));
        drop(lease_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert_eq!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("replacement race keeps lease live")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        );
        assert!(!session.read().await.has_terminal_tail_protections());
        assert!(!replacement.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_submission_current_session_replaces_stale_incarnation_owner() {
        let (manager, stale_session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-current-session-replacement").await;
        let session_key = stale_session.read().await.key.clone();
        let lease_guard = manager.access_leases.write().await;

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &stale_session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                Arc::clone(&plan),
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        manager
            .sessions
            .remove_session(&session_key, &proxy_session_id)
            .await
            .expect("remove stale session incarnation");
        let (current_session, outcome) =
            manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
        assert_eq!(outcome, HlsSessionStoreOutcome::Created);
        assert!(!Arc::ptr_eq(&stale_session, &current_session));
        {
            let mut current = current_session.write().await;
            assert_eq!(
                complete_failed_acceptance_episode(&mut current, 2_000),
                preparation.origin_progress_generation
            );
        }

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &current_session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
        drop(lease_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("current session command terminalizes the lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
        assert!(!stale_session.read().await.has_terminal_tail_protections());
        assert!(current_session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_submission_stale_session_cannot_replace_current_owner() {
        let (manager, stale_session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-stale-session-later").await;
        let session_key = stale_session.read().await.key.clone();
        manager
            .sessions
            .remove_session(&session_key, &proxy_session_id)
            .await
            .expect("remove stale session incarnation");
        let (current_session, outcome) =
            manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
        assert_eq!(outcome, HlsSessionStoreOutcome::Created);
        {
            let mut current = current_session.write().await;
            assert_eq!(
                complete_failed_acceptance_episode(&mut current, 2_000),
                preparation.origin_progress_generation
            );
        }
        let lease_guard = manager.access_leases.write().await;
        let index_guard = manager.sessions.hold_index_write_for_test().await;

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &current_session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                Arc::clone(&plan),
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &stale_session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
        drop(index_guard);
        drop(lease_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert!(matches!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("current session owner survives stale later submission")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
        assert!(!stale_session.read().await.has_terminal_tail_protections());
        assert!(current_session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_media_progress_before_retry_cancels_owner_and_keeps_lease_live() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-recovery").await;
        let lease_guard = manager.access_leases.write().await;

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        {
            let mut session = session.write().await;
            assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
            session.origin_control.record_media_progress(2_100, 12_000);
            assert_eq!(
                session.origin_control.progress_generation,
                preparation.origin_progress_generation.saturating_add(1)
            );
        }
        drop(lease_guard);

        wait_for_terminal_commit_owner(&manager).await;
        assert_eq!(
            manager
                .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .await
                .expect("recovered lease")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        );
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_lease_end_before_retry_cancels_owner_without_mutation() {
        let (manager, session, proxy_session_id, lease_id, preparation, plan) =
            prepared_terminal_commit_fixture("retry-lease-end").await;
        let mut lease_guard = manager.access_leases.write().await;

        assert!(matches!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::LockBusy { .. }
        ));
        let _release = lease_guard.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd);
        drop(lease_guard);

        wait_for_terminal_commit_owner(&manager).await;
        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("ended lease remains stored");
        assert_eq!(lease.state, HlsAccessLeaseState::Denied);
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_new_media_generation_supersedes_prepared_plan() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("failed acceptance permits terminal preparation");

        session.write().await.advance_media_readiness_generation();
        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_100,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::SupersededGeneration
        );

        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_200)
            .await
            .expect("lease remains available");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    #[tokio::test]
    async fn hls_terminal_commit_lease_end_prevents_commit_and_gc_protection() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let mut preparation_request =
            terminal_preparation_request(&lease_id, &proxy_session_id, progress_generation);
        preparation_request.reserve.guaranteed_reserve_ms = 100_000;
        preparation_request.reserve.guaranteed_media_horizon_ms = preparation_request
            .reserve
            .conservative_playback_position_ms
            .saturating_add(preparation_request.reserve.guaranteed_reserve_ms);
        preparation_request.cutover_timing = HlsLeaseCutoverTiming::from_reserve(
            preparation_request.now_ms,
            preparation_request.reserve.guaranteed_reserve_ms,
            preparation_request.reserve.transition_margin,
            None,
        );
        let preparation = manager
            .prepare_access_lease_terminal_tail(preparation_request)
            .await
            .expect("failed acceptance permits terminal preparation");

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                61_000,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::LeaseNoLongerEligible
        );

        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 61_000)
            .await
            .expect("expired lease remains stored until lifecycle cleanup");
        assert_eq!(lease.state, HlsAccessLeaseState::Expired);
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
        assert!(!session.read().await.has_terminal_tail_protections());
    }

    async fn assert_terminal_and_live_lease_modes(
        manager: &HlsProxyManager,
        proxy_session_id: &ProxySessionId,
        terminal_lease_id: &HlsAccessLeaseId,
        live_lease_id: &HlsAccessLeaseId,
    ) {
        assert!(matches!(
            manager
                .access_lease_response_snapshot(terminal_lease_id, proxy_session_id, 2_100)
                .await
                .expect("terminal lease")
                .playback_mode,
            HlsLeasePlaybackMode::TerminalTail(_)
        ));
        assert_eq!(
            manager
                .access_lease_response_snapshot(live_lease_id, proxy_session_id, 2_100)
                .await
                .expect("live lease")
                .playback_mode,
            HlsLeasePlaybackMode::Live
        );
    }

    #[tokio::test]
    async fn hls_cutover_policy_terminal_commit_uses_lease_deadline_and_keeps_farther_lease_live() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let terminal_lease_id = HlsAccessLeaseId("terminal".to_string());
        let live_lease_id = HlsAccessLeaseId("live".to_string());
        {
            let mut leases = manager.access_leases.write().await;
            leases.prepare_access_lease(access_lease(&terminal_lease_id.0, &proxy_session_id));
            leases.prepare_access_lease(access_lease(&live_lease_id.0, &proxy_session_id));
        }
        assert!(
            publish_manifest_snapshot(&manager, &terminal_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,)
                .await
        );
        assert!(
            publish_manifest_snapshot(&manager, &live_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,).await
        );
        let progress_generation = {
            let mut session = session.write().await;
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let far_reserve = HlsLeaseReserveSnapshot {
            guaranteed_media_horizon_ms: 36_000,
            conservative_playback_position_ms: 12_000,
            guaranteed_reserve_ms: 24_000,
            cutover_required: false,
            ..cutover_reserve()
        };
        let far_timing = HlsLeaseCutoverTiming::from_reserve(
            2_000,
            far_reserve.guaranteed_reserve_ms,
            far_reserve.transition_margin,
            None,
        );
        assert!(manager
            .prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
                lease_id: &live_lease_id,
                proxy_session_id: &proxy_session_id,
                manifest_snapshot_generation: 1,
                cursor_generation: 0,
                reserve: far_reserve,
                cutover_timing: far_timing,
                commit_window: HlsTerminalCommitWindow::NotDue,
                now_ms: 2_000,
                origin_progress_generation: progress_generation,
                media_readiness_generation: 0,
                last_media_progress_at_ms: None,
            })
            .await
            .is_none());
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &terminal_lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");
        assert_eq!(
            preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
            2_000_u64.saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis())
        );
        assert_eq!(far_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(), 14_000);
        let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &terminal_lease_id);
        let expected_protection = Arc::clone(&plan.protected_base_proxy_seqs);

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &terminal_lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                plan,
            )
            ,
            HlsTerminalCommitOutcome::Committed
        );

        let session = session.read().await;
        assert_eq!(
            session.terminal_tail_protection(&terminal_lease_id).map(|protection| &protection.base_proxy_seqs),
            Some(&expected_protection)
        );
        assert_eq!(
            session.origin_control.progress_phase,
            super::super::origin_progress::HlsOriginProgressPhase::TerminalPartial
        );
        drop(session);
        assert_terminal_and_live_lease_modes(&manager, &proxy_session_id, &terminal_lease_id, &live_lease_id).await;
    }

    #[tokio::test]
    async fn resource_denial_does_not_destroy_existing_terminal_tail() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("terminal".to_string());
        let plan = terminal_plan(7, &proxy_session_id, &lease_id);
        let mut lease = access_lease(&lease_id.0, &proxy_session_id);
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan));
        manager.access_leases.write().await.prepare_access_lease(lease);
        {
            let mut session = session.write().await;
            assert_eq!(
                session.install_terminal_tail_protection(
                    lease_id.clone(),
                    HlsTerminalTailProtection {
                        generation: plan.generation,
                        base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
                        key_bindings: plan.key_bindings(),
                    },
                ),
                super::super::session::HlsTerminalTailProtectionInstall::Installed
            );
        }

        assert_eq!(
            manager
                .deny_access_lease(
                    &lease_id,
                    HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,
                )
                .await,
            HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
        );

        let denied = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("denied lease remains stored until lifecycle cleanup");
        assert_eq!(denied.state, HlsAccessLeaseState::Denied);
        assert_eq!(
            denied.playback_mode,
            HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan))
        );
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());

        assert_eq!(
            manager
                .deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd)
                .await,
            HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
        );
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());

        manager.remove_access_lease(&lease_id).await;
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
        assert_eq!(
            manager
                .access_leases
                .write()
                .await
                .deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd),
            HlsAccessLeaseDenialOutcome::UnknownLease
        );
    }

    #[tokio::test]
    async fn terminal_tail_protection_is_retained_until_bounded_lease_removal() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("terminal-stale-protection".to_string());
        let plan = terminal_plan(7, &proxy_session_id, &lease_id);
        let stale_generation = HlsTerminalTailGeneration(plan.generation.0.saturating_add(1));
        let mut lease = access_lease(&lease_id.0, &proxy_session_id);
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan));
        manager.access_leases.write().await.prepare_access_lease(lease);
        assert_eq!(
            session.write().await.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: stale_generation,
                    base_proxy_seqs: Arc::from([9_999]),
                    key_bindings: Arc::from([]),
                },
            ),
            super::super::session::HlsTerminalTailProtectionInstall::Installed
        );

        assert_eq!(
            manager
                .deny_access_lease(
                    &lease_id,
                    HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,
                )
                .await,
            HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
        );

        assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());
        manager.remove_access_lease(&lease_id).await;
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
    }

    #[tokio::test]
    async fn stale_lease_removal_preparation_cannot_release_replacement_protection() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("replacement-protection".to_string());
        let old_plan = terminal_plan(7, &proxy_session_id, &lease_id);
        let mut old_lease = access_lease(&lease_id.0, &proxy_session_id);
        old_lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&old_plan));
        manager.access_leases.write().await.prepare_access_lease(old_lease);
        assert_eq!(
            session.write().await.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: old_plan.generation,
                    base_proxy_seqs: Arc::clone(&old_plan.protected_base_proxy_seqs),
                    key_bindings: old_plan.key_bindings(),
                },
            ),
            super::super::session::HlsTerminalTailProtectionInstall::Installed
        );
        let preparation = manager
            .access_leases
            .read()
            .await
            .prepare_access_lease_removal(&lease_id)
            .expect("old lease removal preparation");

        let replacement_plan = terminal_plan(8, &proxy_session_id, &lease_id);
        let mut replacement = access_lease(&lease_id.0, &proxy_session_id);
        replacement.issued_at_ms = 2_000;
        replacement.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&replacement_plan));
        assert!(manager.access_leases.write().await.prepare_access_lease(replacement));
        assert_eq!(
            session.write().await.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: replacement_plan.generation,
                    base_proxy_seqs: Arc::clone(&replacement_plan.protected_base_proxy_seqs),
                    key_bindings: replacement_plan.key_bindings(),
                },
            ),
            super::super::session::HlsTerminalTailProtectionInstall::Installed
        );

        assert!(!manager.remove_prepared_access_lease(&lease_id, &preparation).await);
        assert_eq!(
            manager
                .access_leases
                .write()
                .await
                .response_snapshot(&lease_id, &proxy_session_id, 2_000)
                .map(|lease| lease.issued_at_ms),
            Some(2_000)
        );
        assert_eq!(
            session.read().await.terminal_tail_protection(&lease_id).map(|protection| protection.generation),
            Some(replacement_plan.generation)
        );
    }

    #[tokio::test]
    async fn terminal_unavailable_protection_is_retained_until_bounded_lease_removal() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("terminal-unavailable".to_string());
        let decision_generation = 11;
        let generation = HlsTerminalTailGeneration(decision_generation);
        let mut lease = access_lease(&lease_id.0, &proxy_session_id);
        lease.playback_mode = HlsLeasePlaybackMode::TerminalUnavailable {
            decision_generation,
            reason: HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
        };
        manager.access_leases.write().await.prepare_access_lease(lease);
        assert_eq!(
            session.write().await.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation,
                    base_proxy_seqs: Arc::from([41_u64]),
                    key_bindings: Arc::from([]),
                },
            ),
            super::super::session::HlsTerminalTailProtectionInstall::Installed
        );

        assert_eq!(
            manager
                .deny_access_lease(
                    &lease_id,
                    HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,
                )
                .await,
            HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
        );

        let denied = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("denied lease remains stored until lifecycle cleanup");
        assert_eq!(denied.state, HlsAccessLeaseState::Denied);
        assert!(matches!(
            denied.playback_mode,
            HlsLeasePlaybackMode::TerminalUnavailable { .. }
        ));
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());
        manager.remove_access_lease(&lease_id).await;
        assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
    }

    #[tokio::test]
    async fn hls_terminal_commit_protection_capacity_has_typed_unavailable_outcome() {
        let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
        let manager = HlsProxyManager::with_hls_cache_config(&config);
        let (session, _) =
            manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("overflow".to_string());
        manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
        assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
        let progress_generation = {
            let mut session = session.write().await;
            for index in 0..HLS_TERMINAL_TAIL_PROTECTION_CAPACITY {
                session.install_terminal_tail_protection(
                    HlsAccessLeaseId(format!("occupied-{index}")),
                    HlsTerminalTailProtection {
                        generation: HlsTerminalTailGeneration(1),
                        base_proxy_seqs: Arc::from([u64::try_from(index).unwrap_or(u64::MAX)]),
                        key_bindings: Arc::from([]),
                    },
                );
            }
            complete_failed_acceptance_episode(&mut session, 2_000)
        };
        let preparation = manager
            .prepare_access_lease_terminal_tail(terminal_preparation_request(
                &lease_id,
                &proxy_session_id,
                progress_generation,
            ))
            .await
            .expect("terminal preparation");

        assert_eq!(
            commit_terminal_plan(
                &manager,
                &session,
                &lease_id,
                &proxy_session_id,
                &preparation,
                2_000,
                terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
            )
            ,
            HlsTerminalCommitOutcome::Committed
        );

        let lease = manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
            .await
            .expect("terminal unavailable lease remains stored");
        assert!(matches!(
            lease.playback_mode,
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
                ..
            }
        ));
        let session = session.read().await;
        assert_eq!(session.terminal_tail_protection_count(), HLS_TERMINAL_TAIL_PROTECTION_CAPACITY);
        assert!(session.terminal_tail_protection(&lease_id).is_none());
    }
}
