use super::{
    build_proxy_session_id, classify_account_binding_protection,
    manifest_acceptance::{
        HlsManifestAcceptanceEpisode, HlsManifestAcceptanceGeneration, HlsManifestAcceptanceState,
        HlsManifestAcceptanceTrigger,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    master_playlist::{
        estimate_hls_peak_bandwidth_bps, HlsBandwidthPersistenceOutcome, HlsBandwidthPersistenceState,
        HlsBandwidthSample, HLS_BANDWIDTH_PERSISTENCE_RETRY_MS,
    },
    origin_progress::{HlsBoundedRecoverySamples, HlsOriginPathCondition, HlsOriginProgressPhase},
    recovery_timing::HlsAcceptanceEpisodeTiming,
    resource_identity::HlsPublishedResourceHistory,
    terminal_tail::{HlsTerminalKeyBinding, HlsTerminalTailGeneration},
    HlsAccessLeaseId, HlsAccountBindingProtection, HlsAccountOverlapTiming, HlsEffectiveOriginAcquirePolicy,
    HlsEffectiveOriginAcquirePolicyState, HlsFreshManifestRequiredReason, HlsOriginAccountBinding,
    HlsOriginAccountIoLease, HlsOriginAccountRebindState, HlsOriginSource, HlsSessionKey, MapCacheStatus, MapEntry,
    OriginMapKey, OriginRefreshState, OriginSegmentKey, ProxyMapId, ProxySessionId, RenderPolicy, RenderedManifest,
    SegmentCacheStatus, SegmentEntry, SegmentFetchPriority, SegmentPrefetchQueue, TransientPassthroughState,
};
use axum::http::HeaderMap;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;

/// Session mode selected for a shared HLS content session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsSessionMode {
    NormalCacheTimeline,
    TransientPassthrough { reason: TransientPassthroughReason },
}

/// Reason why a session cannot use the normal cache timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientPassthroughReason {
    ExtXKey,
    UnsupportedTag { tag: String },
    ParserUnsupportedFeature { feature: String },
}

pub const HLS_TERMINAL_TAIL_PROTECTION_CAPACITY: usize = 1_024;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsTerminalTailProtection {
    pub generation: HlsTerminalTailGeneration,
    pub base_proxy_seqs: Arc<[u64]>,
    pub key_bindings: Arc<[HlsTerminalKeyBinding]>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsTerminalTailProtectionInstall {
    Installed,
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsTerminalTailProtectionRemoval {
    Removed,
    Missing,
    RemovedStaleGeneration { actual: HlsTerminalTailGeneration },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HlsSegmentFailureTracker {
    pub consecutive_temporary_failures: u32,
    pub last_failure_at_ms: Option<u64>,
    pub last_failed_object: Option<HlsSegmentFailureObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsSegmentFailureObject {
    Normal { proxy_seq: u64, origin_seq: u64 },
    Transient { resource_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HlsSegmentFailureTransition {
    StillRetryable { failures: u32, threshold: u32 },
    BecamePermanentlyFailed { failures: u32, threshold: u32 },
}

#[derive(Debug, Clone, Default)]
pub struct HlsSessionActivity {
    pub last_authorized_manifest_at_ms: Option<u64>,
    pub last_authorized_media_at_ms: Option<u64>,
    pub active_access_lease_count: usize,
    pub active_origin_work_count: usize,
    pub origin_work_generation: u64,
    pub media_readiness_generation: u64,
}

/// Historical proof that a stored client-visible manifest exposed real origin media.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HlsPublishedLiveOriginBaseline {
    pub evidence_proxy_seq: u64,
    pub origin_epoch: u64,
    pub rendered_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct HlsSessionOriginControl {
    pub progress_phase: HlsOriginProgressPhase,
    pub path_condition: HlsOriginPathCondition,
    pub acceptance_generation: HlsManifestAcceptanceGeneration,
    pub progress_generation: u64,
    pub last_origin_response_at_ms: Option<u64>,
    pub last_media_progress_at_ms: Option<u64>,
    pub target_duration_snapshot_ms: Option<u64>,
    pub pinned_host: Option<String>,
    pub manifest_origin_binding: Option<HlsManifestOriginBinding>,
    pub origin_epoch: u64,
    host_local_highwaters: HashMap<(u64, String), HlsHostLocalHighwaterEntry>,
    host_local_highwater_observation_order: u64,
    pub acceptance_episode: Option<HlsManifestAcceptanceEpisode>,
    pub recovery_samples: HlsBoundedRecoverySamples,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HlsHostLocalHighwaterEntry {
    highwater: u64,
    last_observed_at_ms: u64,
    last_observed_order: u64,
}

impl Default for HlsSessionOriginControl {
    fn default() -> Self {
        Self {
            progress_phase: HlsOriginProgressPhase::Cold,
            path_condition: HlsOriginPathCondition::ProgressExpected,
            acceptance_generation: HlsManifestAcceptanceGeneration(0),
            progress_generation: 0,
            last_origin_response_at_ms: None,
            last_media_progress_at_ms: None,
            target_duration_snapshot_ms: None,
            pinned_host: None,
            manifest_origin_binding: None,
            origin_epoch: 0,
            host_local_highwaters: HashMap::new(),
            host_local_highwater_observation_order: 0,
            acceptance_episode: None,
            recovery_samples: HlsBoundedRecoverySamples::default(),
        }
    }
}

impl HlsSessionOriginControl {
    const MAX_HOST_LOCAL_HIGHWATERS: usize = 32;

    pub fn record_origin_response(&mut self, now_ms: u64) { self.last_origin_response_at_ms = Some(now_ms); }

    pub fn record_media_progress(&mut self, now_ms: u64, target_duration_ms: u64) {
        self.last_origin_response_at_ms = Some(now_ms);
        self.last_media_progress_at_ms = Some(now_ms);
        self.target_duration_snapshot_ms = Some(target_duration_ms);
        self.path_condition = HlsOriginPathCondition::ProgressExpected;
        self.progress_phase = HlsOriginProgressPhase::Fresh;
        self.progress_generation = self.progress_generation.saturating_add(1);
    }

    pub fn begin_acceptance_episode(
        &mut self,
        now_ms: u64,
        plan: shared::model::HlsManifestRecoveryBurstPlan,
        trigger: HlsManifestAcceptanceTrigger,
        timing: &HlsAcceptanceEpisodeTiming,
    ) -> HlsManifestAcceptanceGeneration {
        let previous_held_alternative = self
            .acceptance_episode
            .as_ref()
            .filter(|episode| episode.state == super::manifest_acceptance::HlsManifestAcceptanceState::Holding)
            .and_then(|episode| episode.held_alternative.clone());
        self.acceptance_generation = HlsManifestAcceptanceGeneration(self.acceptance_generation.0.saturating_add(1));
        let generation = self.acceptance_generation;
        let mut episode = HlsManifestAcceptanceEpisode::new(generation, now_ms, plan, trigger, timing);
        episode.held_alternative = previous_held_alternative;
        self.acceptance_episode = Some(episode);
        self.progress_phase = HlsOriginProgressPhase::Recovering;
        generation
    }

    pub fn record_host_local_highwater(
        &mut self,
        origin_epoch: u64,
        effective_host: String,
        highwater: u64,
        observed_at_ms: u64,
    ) {
        self.host_local_highwater_observation_order = self.host_local_highwater_observation_order.saturating_add(1);
        let observation_order = self.host_local_highwater_observation_order;
        self.host_local_highwaters
            .entry((origin_epoch, effective_host))
            .and_modify(|current| {
                current.highwater = current.highwater.max(highwater);
                current.last_observed_at_ms = current.last_observed_at_ms.max(observed_at_ms);
                current.last_observed_order = observation_order;
            })
            .or_insert(HlsHostLocalHighwaterEntry {
                highwater,
                last_observed_at_ms: observed_at_ms,
                last_observed_order: observation_order,
            });
        while self.host_local_highwaters.len() > Self::MAX_HOST_LOCAL_HIGHWATERS {
            let Some(eviction) = self
                .host_local_highwaters
                .iter()
                .min_by(|(left_key, left_entry), (right_key, right_entry)| {
                    self.highwater_protection_rank(left_key)
                        .cmp(&self.highwater_protection_rank(right_key))
                        .then_with(|| left_entry.last_observed_order.cmp(&right_entry.last_observed_order))
                        .then_with(|| left_entry.last_observed_at_ms.cmp(&right_entry.last_observed_at_ms))
                        .then_with(|| left_key.0.cmp(&right_key.0))
                        .then_with(|| {
                            let left_hash = blake3::hash(left_key.1.as_bytes());
                            let right_hash = blake3::hash(right_key.1.as_bytes());
                            left_hash.as_bytes().cmp(right_hash.as_bytes())
                        })
                })
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.host_local_highwaters.remove(&eviction);
        }
    }

    fn highwater_protection_rank(&self, key: &(u64, String)) -> u8 {
        if key.0 == self.origin_epoch && self.pinned_host.as_deref() == Some(key.1.as_str()) {
            return 2;
        }
        let episode_host_is_relevant = self.acceptance_episode.as_ref().is_some_and(|episode| {
            episode.state != HlsManifestAcceptanceState::Completed
                && (episode.held_alternative.as_ref().is_some_and(|cohort| cohort.identity.effective_host == key.1)
                    || episode.observed_landscape.as_ref().is_some_and(|landscape| {
                        landscape.alternatives.iter().any(|(identity, _)| identity.effective_host == key.1)
                    }))
        });
        u8::from(key.0 == self.origin_epoch && episode_host_is_relevant)
    }
}

/// Shared runtime state for one stable HLS origin entry.
pub struct HlsSession {
    pub key: HlsSessionKey,
    pub proxy_session_id: ProxySessionId,
    pub origin_source: HlsOriginSource,
    pub origin_account_binding: Option<HlsOriginAccountBinding>,
    pub origin_account_io_lease: Option<HlsOriginAccountIoLease>,
    pub origin_account_rebind: HlsOriginAccountRebindState,
    pub effective_origin_acquire_policy: Option<HlsEffectiveOriginAcquirePolicyState>,
    pub mode: HlsSessionMode,
    pub transient: TransientPassthroughState,
    pub last_client_access_at_ms: u64,
    pub last_effective_manifest_host: Option<String>,
    pub origin_refresh: OriginRefreshState,
    pub origin_control: HlsSessionOriginControl,
    pub render_policy: RenderPolicy,
    pub last_rendered_manifest: Option<RenderedManifest>,
    pub published_live_origin_baseline: Option<HlsPublishedLiveOriginBaseline>,
    pub longest_rendered_playlist_duration_ms: u64,
    pub initial_prefetch_gap_segments: usize,
    pub segment_prefetch_queue: SegmentPrefetchQueue,
    pub active_segment_fetches: usize,
    pub segment_fetch_notifiers: HashMap<u64, Arc<Notify>>,
    pub origin_request_headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub activity: HlsSessionActivity,
    pub origin_epoch: u64,
    pub origin_epoch_effective_host_id: Option<u64>,
    pub origin_epoch_sequence_base: Option<u64>,
    pub origin_seq_highwater: Option<u64>,
    pub proxy_next_seq: Option<u64>,
    pub origin_to_proxy: HashMap<OriginSegmentKey, u64>,
    pub published_resource_history: HlsPublishedResourceHistory,
    pub discontinuity_sequence: u64,
    pub transient_discontinuity_sequence: Option<u64>,
    pub pending_handoff_discontinuity_sequence: Option<u64>,
    pub pending_origin_epoch_handoff: bool,
    pub segments: BTreeMap<u64, SegmentEntry>,
    pub active_map_fetches: usize,
    pub maps: BTreeMap<ProxyMapId, MapEntry>,
    pub origin_map_to_proxy: HashMap<OriginMapKey, ProxyMapId>,
    pub next_proxy_map_id: u64,
    pub publishable_origin_head_proxy_seq: Option<u64>,
    pub publishable_origin_tail_proxy_seq: Option<u64>,
    pub origin_version: Option<u16>,
    pub target_duration: Option<u32>,
    pub bandwidth_persistence: HlsBandwidthPersistenceState,
    pub independent_segments: bool,
    pub fresh_manifest_commit_required: Option<HlsFreshManifestRequiredReason>,
    fresh_manifest_commit_requirement_generation: u64,
    pub segment_failure_tracker: HlsSegmentFailureTracker,
    terminal_tail_protections: HashMap<HlsAccessLeaseId, HlsTerminalTailProtection>,
    gc_marked_for_removal: bool,
}

impl HlsSession {
    pub fn new(key: HlsSessionKey, reverse_proxy_rewrite_secret: &[u8], now_ms: u64) -> Self {
        let origin_source = HlsOriginSource::from_session_key(&key);
        Self::new_with_origin_source(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
    }

    pub fn new_with_origin_source(
        key: HlsSessionKey,
        origin_source: HlsOriginSource,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> Self {
        let proxy_session_id = build_proxy_session_id(&key, reverse_proxy_rewrite_secret);
        Self {
            key,
            proxy_session_id,
            origin_source,
            origin_account_binding: None,
            origin_account_io_lease: None,
            origin_account_rebind: HlsOriginAccountRebindState::default(),
            effective_origin_acquire_policy: None,
            mode: HlsSessionMode::NormalCacheTimeline,
            transient: TransientPassthroughState::default(),
            last_client_access_at_ms: now_ms,
            last_effective_manifest_host: None,
            origin_refresh: OriginRefreshState::default(),
            origin_control: HlsSessionOriginControl::default(),
            render_policy: RenderPolicy::default(),
            last_rendered_manifest: None,
            published_live_origin_baseline: None,
            longest_rendered_playlist_duration_ms: 0,
            initial_prefetch_gap_segments: 0,
            segment_prefetch_queue: SegmentPrefetchQueue::default(),
            active_segment_fetches: 0,
            segment_fetch_notifiers: HashMap::new(),
            origin_request_headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            activity: HlsSessionActivity::default(),
            origin_epoch: 0,
            origin_epoch_effective_host_id: None,
            origin_epoch_sequence_base: None,
            origin_seq_highwater: None,
            proxy_next_seq: None,
            origin_to_proxy: HashMap::new(),
            published_resource_history: HlsPublishedResourceHistory::default(),
            discontinuity_sequence: 0,
            transient_discontinuity_sequence: None,
            pending_handoff_discontinuity_sequence: None,
            pending_origin_epoch_handoff: false,
            active_map_fetches: 0,
            segments: BTreeMap::new(),
            maps: BTreeMap::new(),
            origin_map_to_proxy: HashMap::new(),
            next_proxy_map_id: 0,
            publishable_origin_head_proxy_seq: None,
            publishable_origin_tail_proxy_seq: None,
            origin_version: None,
            target_duration: None,
            bandwidth_persistence: HlsBandwidthPersistenceState::default(),
            independent_segments: false,
            fresh_manifest_commit_required: None,
            fresh_manifest_commit_requirement_generation: 0,
            segment_failure_tracker: HlsSegmentFailureTracker::default(),
            terminal_tail_protections: HashMap::new(),
            gc_marked_for_removal: false,
        }
    }

    pub fn established_manifest_recovery_binding(&self) -> Option<HlsManifestOriginBinding> {
        if !matches!(self.mode, HlsSessionMode::NormalCacheTimeline)
            || self.origin_control.manifest_origin_binding.is_none()
            || self.origin_control.pinned_host.is_none()
            || self.origin_seq_highwater.is_none()
            || self.last_rendered_manifest.is_none()
            || self.published_live_origin_baseline.is_none()
            || matches!(
                self.origin_control.progress_phase,
                HlsOriginProgressPhase::Cold
                    | HlsOriginProgressPhase::TerminalPartial
                    | HlsOriginProgressPhase::Terminal
            )
        {
            return None;
        }
        self.origin_control.manifest_origin_binding.clone()
    }

    pub fn mark_pending_handoff_discontinuity(&mut self, discontinuity_sequence: u64) {
        self.pending_handoff_discontinuity_sequence = Some(discontinuity_sequence);
    }

    pub fn mark_pending_origin_epoch_handoff_discontinuity(&mut self, discontinuity_sequence: u64) {
        if self.pending_handoff_discontinuity_sequence.is_none() {
            self.pending_handoff_discontinuity_sequence = Some(discontinuity_sequence);
        }
        self.pending_origin_epoch_handoff = true;
    }

    pub fn take_pending_handoff_discontinuity_sequence(&mut self) -> Option<u64> {
        self.pending_handoff_discontinuity_sequence.take()
    }

    pub fn mark_for_gc_removal(&mut self) { self.gc_marked_for_removal = true; }

    pub fn clear_gc_removal_mark(&mut self) { self.gc_marked_for_removal = false; }

    pub fn is_gc_marked_for_removal(&self) -> bool { self.gc_marked_for_removal }

    pub fn replace_origin_account_binding(&mut self, binding: Option<HlsOriginAccountBinding>) {
        let binding_changed = match (&self.origin_account_binding, &binding) {
            (Some(current), Some(next)) => {
                current.input_name != next.input_name
                    || current.account_name != next.account_name
                    || current.session_owner != next.session_owner
                    || current.generation != next.generation
            }
            (None, None) => false,
            _ => true,
        };
        if binding_changed {
            self.origin_provider_session_headers.clear();
        }
        self.origin_account_binding = binding;
    }

    pub fn record_successful_segment_fetch(&mut self) -> Option<u32> {
        if self.segment_failure_tracker.consecutive_temporary_failures == 0 {
            return None;
        }
        let previous = self.segment_failure_tracker.consecutive_temporary_failures;
        self.segment_failure_tracker = HlsSegmentFailureTracker::default();
        Some(previous)
    }

    pub fn record_temporary_segment_fetch_failure(
        &mut self,
        now_ms: u64,
        object: HlsSegmentFailureObject,
        threshold: u32,
    ) -> HlsSegmentFailureTransition {
        let failures = self.segment_failure_tracker.consecutive_temporary_failures.saturating_add(1);
        self.segment_failure_tracker.consecutive_temporary_failures = failures;
        self.segment_failure_tracker.last_failure_at_ms = Some(now_ms);
        self.segment_failure_tracker.last_failed_object = Some(object);

        if failures >= threshold {
            HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold }
        } else {
            HlsSegmentFailureTransition::StillRetryable { failures, threshold }
        }
    }

    pub fn require_fresh_manifest_commit(&mut self, reason: HlsFreshManifestRequiredReason) {
        self.fresh_manifest_commit_requirement_generation =
            self.fresh_manifest_commit_requirement_generation.saturating_add(1);
        self.fresh_manifest_commit_required = Some(reason);
    }

    pub fn fresh_manifest_commit_requirement_generation(&self, reason: HlsFreshManifestRequiredReason) -> Option<u64> {
        (self.fresh_manifest_commit_required == Some(reason))
            .then_some(self.fresh_manifest_commit_requirement_generation)
    }

    pub fn clear_fresh_manifest_commit_requirement_if_current(
        &mut self,
        reason: HlsFreshManifestRequiredReason,
        generation: u64,
    ) {
        if self.fresh_manifest_commit_required != Some(reason)
            || self.fresh_manifest_commit_requirement_generation != generation
        {
            return;
        }
        self.fresh_manifest_commit_required = None;
    }

    pub fn account_overlap_timing(&self) -> HlsAccountOverlapTiming {
        HlsAccountOverlapTiming::from_target_duration_secs(self.target_duration.map(u64::from))
    }

    pub fn account_binding_protection(&self, now_ms: u64) -> HlsAccountBindingProtection {
        classify_account_binding_protection(
            self.activity.last_authorized_media_at_ms,
            now_ms,
            self.account_overlap_timing(),
        )
    }

    pub fn should_refresh_origin_reservation(&self, now_ms: u64) -> bool {
        !matches!(self.account_binding_protection(now_ms), HlsAccountBindingProtection::Expired)
            || self.activity.active_origin_work_count > 0
    }

    pub fn reconcile_effective_origin_acquire_policy(
        &mut self,
        candidate: Option<HlsEffectiveOriginAcquirePolicy>,
        now_ms: u64,
    ) {
        let Some(candidate) = candidate.map(|policy| policy.with_updated_at(now_ms)) else {
            if self.effective_origin_policy_downgrade_allowed(now_ms) {
                self.effective_origin_acquire_policy = None;
            }
            return;
        };

        let Some(state) = self.effective_origin_acquire_policy else {
            self.effective_origin_acquire_policy = Some(HlsEffectiveOriginAcquirePolicyState::new(candidate, now_ms));
            return;
        };

        if candidate.has_same_rank_as(state.current_policy) {
            self.effective_origin_acquire_policy = Some(HlsEffectiveOriginAcquirePolicyState {
                current_policy: state.current_policy,
                last_supported_at_ms: now_ms,
            });
        } else if candidate.is_better_than(state.current_policy)
            || self.effective_origin_policy_downgrade_allowed(now_ms)
        {
            self.effective_origin_acquire_policy = Some(HlsEffectiveOriginAcquirePolicyState::new(candidate, now_ms));
        }
    }

    fn effective_origin_policy_downgrade_allowed(&self, now_ms: u64) -> bool {
        let Some(state) = self.effective_origin_acquire_policy else {
            return true;
        };
        self.target_duration.is_none_or(|target_duration| {
            let guard_ms = u64::from(target_duration).saturating_mul(1_000);
            now_ms.saturating_sub(state.last_supported_at_ms) >= guard_ms
        })
    }

    pub fn effective_origin_acquire_policy_or_default(&self) -> HlsEffectiveOriginAcquirePolicy {
        self.effective_origin_acquire_policy
            .map_or_else(HlsEffectiveOriginAcquirePolicy::fallback, |state| state.current_policy)
    }

    pub fn mark_authorized_manifest_access(&mut self, now_ms: u64) {
        self.activity.last_authorized_manifest_at_ms = Some(now_ms);
        self.last_client_access_at_ms = now_ms;
    }

    pub fn mark_authorized_media_access(&mut self, now_ms: u64) {
        self.activity.last_authorized_media_at_ms = Some(now_ms);
        self.last_client_access_at_ms = now_ms;
    }

    pub fn idle_expiry_due_at_ms(&self, session_idle_timeout_ms: u64) -> u64 {
        self.last_client_access_at_ms.saturating_add(session_idle_timeout_ms)
    }

    pub fn can_expire_idle_session(&self, now_ms: u64, session_idle_timeout_ms: u64) -> bool {
        if self.idle_expiry_due_at_ms(session_idle_timeout_ms) > now_ms {
            return false;
        }
        self.activity.active_origin_work_count == 0
            && self.active_segment_fetches == 0
            && self.active_map_fetches == 0
            && !self.origin_refresh.in_flight
            && self.terminal_tail_protections.is_empty()
            && self.segment_prefetch_queue.is_empty()
            && !self.segments.values().any(|segment| {
                segment.access.active_readers() > 0 || matches!(segment.status, SegmentCacheStatus::Fetching { .. })
            })
            && !self.maps.values().any(|map| {
                map.access.active_readers() > 0
                    || matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. })
            })
            && !self.transient.has_active_resource_readers()
    }

    pub fn install_terminal_tail_protection(
        &mut self,
        lease_id: HlsAccessLeaseId,
        protection: HlsTerminalTailProtection,
    ) -> HlsTerminalTailProtectionInstall {
        if !self.terminal_tail_protections.contains_key(&lease_id)
            && self.terminal_tail_protections.len() >= HLS_TERMINAL_TAIL_PROTECTION_CAPACITY
        {
            return HlsTerminalTailProtectionInstall::CapacityExceeded;
        }
        self.terminal_tail_protections.insert(lease_id, protection);
        HlsTerminalTailProtectionInstall::Installed
    }

    pub fn can_install_terminal_tail_protection(&self, lease_id: &HlsAccessLeaseId) -> bool {
        self.terminal_tail_protections.contains_key(lease_id)
            || self.terminal_tail_protections.len() < HLS_TERMINAL_TAIL_PROTECTION_CAPACITY
    }

    pub fn remove_terminal_tail_protection(
        &mut self,
        lease_id: &HlsAccessLeaseId,
    ) -> Option<HlsTerminalTailProtection> {
        self.terminal_tail_protections.remove(lease_id)
    }

    /// Removes the protection after the lease has atomically entered `Ended`.
    /// A different generation is stale because ended leases cannot reactivate.
    pub fn remove_terminal_tail_protection_after_lease_end(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        expected_generation: HlsTerminalTailGeneration,
    ) -> HlsTerminalTailProtectionRemoval {
        match self.terminal_tail_protections.remove(lease_id) {
            Some(protection) if protection.generation == expected_generation => {
                HlsTerminalTailProtectionRemoval::Removed
            }
            Some(protection) => {
                HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual: protection.generation }
            }
            None => HlsTerminalTailProtectionRemoval::Missing,
        }
    }

    /// Restores the exact pre-CAS protection state after a lease commit loses
    /// its generation race. Removing the speculative entry first makes this
    /// rollback capacity-neutral and therefore infallible.
    pub fn rollback_terminal_tail_protection(
        &mut self,
        lease_id: HlsAccessLeaseId,
        previous: Option<HlsTerminalTailProtection>,
    ) {
        self.terminal_tail_protections.remove(&lease_id);
        if let Some(previous) = previous {
            self.terminal_tail_protections.insert(lease_id, previous);
        }
    }

    pub fn clear_terminal_tail_protections(&mut self) { self.terminal_tail_protections.clear(); }

    #[cfg(any(test, feature = "test-support"))]
    pub fn terminal_tail_protection(&self, lease_id: &HlsAccessLeaseId) -> Option<&HlsTerminalTailProtection> {
        self.terminal_tail_protections.get(lease_id)
    }

    pub fn terminal_tail_protections(&self) -> impl Iterator<Item = (&HlsAccessLeaseId, &HlsTerminalTailProtection)> {
        self.terminal_tail_protections.iter()
    }

    pub fn terminal_key_binding_is_current(
        &self,
        lease_id: &HlsAccessLeaseId,
        generation: HlsTerminalTailGeneration,
        binding: &HlsTerminalKeyBinding,
    ) -> bool {
        self.terminal_tail_protections.get(lease_id).is_some_and(|protection| {
            protection.generation == generation && protection.key_bindings.iter().any(|current| current == binding)
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn terminal_tail_protection_count(&self) -> usize { self.terminal_tail_protections.len() }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_terminal_tail_protections(&self) -> bool { !self.terminal_tail_protections.is_empty() }

    pub fn initial_manifest_commit_work_pending(&self) -> bool {
        self.origin_refresh.in_flight
            || self.active_segment_fetches > 0
            || self.active_map_fetches > 0
            || self.segments.values().any(|segment| {
                matches!(
                    segment.status,
                    SegmentCacheStatus::Queued {
                        priority: SegmentFetchPriority::Demand | SegmentFetchPriority::RenderWindow,
                        ..
                    } | SegmentCacheStatus::Fetching {
                        priority: SegmentFetchPriority::Demand | SegmentFetchPriority::RenderWindow,
                        ..
                    }
                )
            })
            || self
                .maps
                .values()
                .any(|map| matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. }))
    }

    pub fn begin_bandwidth_persistence(&mut self, now_ms: u64) -> Option<u32> {
        if self.mode != HlsSessionMode::NormalCacheTimeline {
            return None;
        }
        match self.bandwidth_persistence {
            HlsBandwidthPersistenceState::InFlight { .. }
            | HlsBandwidthPersistenceState::Persisted { .. }
            | HlsBandwidthPersistenceState::PermanentlyInapplicable { .. } => {
                return None;
            }
            HlsBandwidthPersistenceState::RetryAfter { retry_at_ms } if now_ms < retry_at_ms => return None,
            HlsBandwidthPersistenceState::Idle | HlsBandwidthPersistenceState::RetryAfter { .. } => {}
        }

        let target_duration_ms = u64::from(self.target_duration?).saturating_mul(1_000);
        let samples = self.segments.values().filter_map(HlsBandwidthSample::from_segment).collect::<Vec<_>>();
        let bitrate_bps = estimate_hls_peak_bandwidth_bps(&samples, target_duration_ms)?;
        self.bandwidth_persistence = HlsBandwidthPersistenceState::InFlight { bitrate_bps };
        Some(bitrate_bps)
    }

    pub fn finish_bandwidth_persistence(
        &mut self,
        bitrate_bps: u32,
        outcome: HlsBandwidthPersistenceOutcome,
        now_ms: u64,
    ) {
        if self.bandwidth_persistence != (HlsBandwidthPersistenceState::InFlight { bitrate_bps }) {
            return;
        }
        self.bandwidth_persistence = match outcome {
            HlsBandwidthPersistenceOutcome::Persisted => HlsBandwidthPersistenceState::Persisted { bitrate_bps },
            HlsBandwidthPersistenceOutcome::RetryAfter => HlsBandwidthPersistenceState::RetryAfter {
                retry_at_ms: now_ms.saturating_add(HLS_BANDWIDTH_PERSISTENCE_RETRY_MS),
            },
            HlsBandwidthPersistenceOutcome::PermanentlyInapplicable => {
                HlsBandwidthPersistenceState::PermanentlyInapplicable { bitrate_bps }
            }
        };
    }

    pub fn start_origin_work(&mut self) -> u64 {
        self.activity.active_origin_work_count = self.activity.active_origin_work_count.saturating_add(1);
        self.activity.origin_work_generation
    }

    pub fn finish_origin_work(&mut self, started_generation: u64) -> bool {
        self.activity.active_origin_work_count = self.activity.active_origin_work_count.saturating_sub(1);
        started_generation == self.activity.origin_work_generation
    }

    pub fn invalidate_queued_origin_work(&mut self) {
        self.activity.origin_work_generation = self.activity.origin_work_generation.saturating_add(1);
        let queued_segment_seqs = self.segment_prefetch_queue.proxy_seqs();
        for proxy_seq in queued_segment_seqs {
            self.segment_prefetch_queue.remove(proxy_seq);
            if let Some(segment) = self.segments.get_mut(&proxy_seq) {
                if matches!(segment.status, SegmentCacheStatus::Queued { .. }) {
                    segment.status = SegmentCacheStatus::Discovered;
                }
            }
        }
        for map in self.maps.values_mut() {
            if matches!(map.status, MapCacheStatus::Queued { .. }) {
                map.status = MapCacheStatus::Discovered;
            }
        }
    }

    /// Invalidates reserve/terminal decisions that observed an older READY set.
    pub fn advance_media_readiness_generation(&mut self) {
        self.activity.media_readiness_generation = self.activity.media_readiness_generation.saturating_add(1);
    }

    pub fn referenced_map_ids(&self) -> Vec<ProxyMapId> {
        let mut map_ids = self.segments.values().filter_map(|segment| segment.map_ref).collect::<Vec<_>>();
        map_ids.sort_unstable();
        map_ids.dedup();
        map_ids
    }
}

impl fmt::Debug for HlsSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HlsSession")
            .field("key", &self.key)
            .field("proxy_session_id", &self.proxy_session_id)
            .field("origin_source", &self.origin_source)
            .field("origin_account_binding", &self.origin_account_binding)
            .field("origin_account_io_lease", &self.origin_account_io_lease)
            .field("origin_account_rebind", &self.origin_account_rebind)
            .field("effective_origin_acquire_policy", &self.effective_origin_acquire_policy)
            .field("mode", &self.mode)
            .field("transient", &self.transient)
            .field("last_client_access_at_ms", &self.last_client_access_at_ms)
            .field("last_effective_manifest_host", &self.last_effective_manifest_host)
            .field("origin_refresh", &self.origin_refresh)
            .field("render_policy", &self.render_policy)
            .field("last_rendered_manifest", &self.last_rendered_manifest)
            .field("published_live_origin_baseline", &self.published_live_origin_baseline)
            .field("longest_rendered_playlist_duration_ms", &self.longest_rendered_playlist_duration_ms)
            .field("initial_prefetch_gap_segments", &self.initial_prefetch_gap_segments)
            .field("segment_prefetch_queue_len", &self.segment_prefetch_queue.len())
            .field("active_segment_fetches", &self.active_segment_fetches)
            .field("segment_fetch_notifiers_len", &self.segment_fetch_notifiers.len())
            .field("origin_request_headers_len", &self.origin_request_headers.len())
            .field("origin_provider_session_headers_len", &self.origin_provider_session_headers.len())
            .field("activity", &self.activity)
            .field("origin_control", &self.origin_control)
            .field("origin_epoch", &self.origin_epoch)
            .field("origin_epoch_host_bound", &self.origin_epoch_effective_host_id.is_some())
            .field("origin_epoch_sequence_base", &self.origin_epoch_sequence_base)
            .field("origin_seq_highwater", &self.origin_seq_highwater)
            .field("proxy_next_seq", &self.proxy_next_seq)
            .field("origin_to_proxy_len", &self.origin_to_proxy.len())
            .field("published_resource_history", &self.published_resource_history.generation())
            .field("discontinuity_sequence", &self.discontinuity_sequence)
            .field("transient_discontinuity_sequence", &self.transient_discontinuity_sequence)
            .field("pending_handoff_discontinuity_sequence", &self.pending_handoff_discontinuity_sequence)
            .field("pending_origin_epoch_handoff", &self.pending_origin_epoch_handoff)
            .field("active_map_fetches", &self.active_map_fetches)
            .field("segments_len", &self.segments.len())
            .field("maps_len", &self.maps.len())
            .field("origin_map_to_proxy_len", &self.origin_map_to_proxy.len())
            .field("next_proxy_map_id", &self.next_proxy_map_id)
            .field("publishable_origin_head_proxy_seq", &self.publishable_origin_head_proxy_seq)
            .field("publishable_origin_tail_proxy_seq", &self.publishable_origin_tail_proxy_seq)
            .field("origin_version", &self.origin_version)
            .field("target_duration", &self.target_duration)
            .field("bandwidth_persistence", &self.bandwidth_persistence)
            .field("independent_segments", &self.independent_segments)
            .field("fresh_manifest_commit_required", &self.fresh_manifest_commit_required)
            .field("fresh_manifest_commit_requirement_generation", &self.fresh_manifest_commit_requirement_generation)
            .field("segment_failure_tracker", &self.segment_failure_tracker)
            .field("terminal_tail_protections_len", &self.terminal_tail_protections.len())
            .field("gc_marked_for_removal", &self.gc_marked_for_removal)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::master_playlist::{HlsBandwidthPersistenceOutcome, HlsBandwidthPersistenceState},
        HlsSession, HlsSessionMode, HlsTerminalTailProtection, HlsTerminalTailProtectionInstall,
        HlsTerminalTailProtectionRemoval, TransientPassthroughReason, HLS_TERMINAL_TAIL_PROTECTION_CAPACITY,
    };
    use crate::{
        manifest_acceptance::{
            HlsAlternativeOriginCohort, HlsAlternativeOriginCohortIdentity, HlsAlternativeOriginWindow,
            HlsCrossHostAcceptanceEvidence, HlsManifestAcceptanceLandscape, HlsManifestTechnicalSignature,
            HlsManifestTimelineFingerprint, HlsPinnedOriginObservationState,
        },
        manifest_origin_binding::HlsManifestOriginBinding,
        origin_progress::HlsOriginProgressPhase,
        recovery_timing::{
            HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsObservedRecoveryLatency,
            HlsOperationTimeoutMs, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload,
            HlsTerminalMediaPreparationState, HlsTransitionMarginMs,
        },
        timeline::{HLS_PROVISIONING_GAP_ORIGIN_EPOCH, HLS_PROVISIONING_ORIGIN_EPOCH},
        CacheAccessState, HlsAccessLeaseId, HlsAccountBindingProtection, HlsEffectiveOriginAcquirePolicy,
        HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSessionKey, HlsTerminalTailGeneration,
        OriginSegmentKey, RenderedManifest, RenderedManifestStoreOutcome, SegmentCacheKey, SegmentCacheStatus,
        SegmentEntry,
    };
    use shared::model::HlsManifestRecoveryBurstPlan;
    use std::sync::Arc;
    use tuliprox_session::ConnectionKind;
    use url::Url;

    fn origin_policy(connection_kind: ConnectionKind, priority: i8) -> HlsEffectiveOriginAcquirePolicy {
        HlsEffectiveOriginAcquirePolicy::new(connection_kind, priority, 0)
    }

    fn ready_segment(
        session: &HlsSession,
        proxy_seq: u64,
        origin_epoch: u64,
        duration_ms: u64,
        content_length: u64,
    ) -> SegmentEntry {
        SegmentEntry {
            origin_key: OriginSegmentKey {
                origin_epoch,
                effective_host_id: 1,
                host_local_sequence: proxy_seq,
                host_local_index: u32::try_from(proxy_seq).unwrap_or(u32::MAX),
            },
            proxy_seq,
            duration_ms,
            proxy_file_ext: "ts".to_string(),
            content_type: "video/mp2t".to_string(),
            cache_key: SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts"),
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            encryption: None,
            origin_fetch_ref: None,
            status: SegmentCacheStatus::Ready { content_length, ready_at_ms: 1 },
            last_rendered_at_ms: Some(1),
            access: Arc::new(CacheAccessState::new()),
        }
    }

    fn recovery_gate_session() -> HlsSession {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "recovery-binding"), b"secret", 0);
        session.origin_control.manifest_origin_binding = Some(
            HlsManifestOriginBinding::new(
                Url::parse("https://origin.example/live/index.m3u8?token=test").expect("binding URL"),
                Some(1),
            )
            .expect("binding"),
        );
        session.origin_control.pinned_host = Some("origin.example".to_string());
        session.origin_seq_highwater = Some(10);
        session.origin_control.record_media_progress(1, 4_000);
        session
    }

    fn single_segment_manifest(proxy_seq: u64, rendered_at_ms: u64) -> RenderedManifest {
        RenderedManifest {
            body: "#EXTM3U\n".to_string(),
            first_proxy_seq: proxy_seq,
            last_proxy_seq: proxy_seq,
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 4_000,
            valid_until_ms: 10_000,
            render_gap_segments: 0,
            rendered_at_ms,
            segment_proxy_seqs: vec![proxy_seq],
        }
    }

    fn recovery_eligible_session() -> HlsSession {
        let mut session = recovery_gate_session();
        session.segments.insert(0, ready_segment(&session, 0, 1, 4_000, 100));
        assert_eq!(
            session.store_rendered_manifest(single_segment_manifest(0, 1)),
            RenderedManifestStoreOutcome::Stored
        );
        session
    }

    #[test]
    fn established_manifest_recovery_binding_requires_complete_live_baseline() {
        let eligible = recovery_eligible_session();
        let binding = eligible.established_manifest_recovery_binding().expect("complete live baseline is eligible");
        assert_eq!(binding.provider_url_index(), Some(1));

        let mut missing_binding = recovery_eligible_session();
        missing_binding.origin_control.manifest_origin_binding = None;
        assert!(missing_binding.established_manifest_recovery_binding().is_none());

        let mut missing_pin = recovery_eligible_session();
        missing_pin.origin_control.pinned_host = None;
        assert!(missing_pin.established_manifest_recovery_binding().is_none());

        let mut missing_highwater = recovery_eligible_session();
        missing_highwater.origin_seq_highwater = None;
        assert!(missing_highwater.established_manifest_recovery_binding().is_none());

        let mut unpublished = recovery_eligible_session();
        unpublished.last_rendered_manifest = None;
        assert!(unpublished.established_manifest_recovery_binding().is_none());

        let mut missing_published_live_origin = recovery_eligible_session();
        missing_published_live_origin.published_live_origin_baseline = None;
        assert!(missing_published_live_origin.established_manifest_recovery_binding().is_none());
    }

    #[test]
    fn provisioning_render_plus_origin_commit_is_not_recovery_eligible() {
        let mut session = recovery_gate_session();
        session.segments.insert(0, ready_segment(&session, 0, HLS_PROVISIONING_ORIGIN_EPOCH, 4_000, 100));

        assert_eq!(
            session.store_rendered_manifest(single_segment_manifest(0, 1)),
            RenderedManifestStoreOutcome::Stored
        );
        assert!(session.last_rendered_manifest.is_some());
        assert!(session.origin_control.manifest_origin_binding.is_some());
        assert!(session.origin_control.pinned_host.is_some());
        assert!(session.origin_seq_highwater.is_some());
        assert_eq!(session.origin_control.progress_phase, HlsOriginProgressPhase::Fresh);
        assert!(session.published_live_origin_baseline.is_none());
        assert!(session.established_manifest_recovery_binding().is_none());
    }

    #[test]
    fn new_session_starts_without_published_live_evidence() {
        let session = HlsSession::new(HlsSessionKey::new(1, "new-session"), b"secret", 0);

        assert!(session.published_live_origin_baseline.is_none());
        assert!(session.established_manifest_recovery_binding().is_none());
    }

    #[test]
    fn transient_cold_and_terminal_sessions_are_not_manifest_recovery_eligible() {
        let mut cold = recovery_eligible_session();
        cold.origin_control.progress_phase = HlsOriginProgressPhase::Cold;
        assert!(cold.established_manifest_recovery_binding().is_none());

        let mut transient = recovery_eligible_session();
        transient.mode = HlsSessionMode::TransientPassthrough { reason: TransientPassthroughReason::ExtXKey };
        assert!(transient.established_manifest_recovery_binding().is_none());

        let mut terminal_partial = recovery_eligible_session();
        terminal_partial.origin_control.progress_phase = HlsOriginProgressPhase::TerminalPartial;
        assert!(terminal_partial.established_manifest_recovery_binding().is_none());

        let mut channel_unavailable = recovery_eligible_session();
        channel_unavailable.origin_control.progress_phase = HlsOriginProgressPhase::Terminal;
        assert!(channel_unavailable.established_manifest_recovery_binding().is_none());
    }

    #[test]
    fn hls_bandwidth_persistence_uses_only_three_real_ready_segments() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "runtime-bandwidth"), b"secret", 0);
        session.target_duration = Some(4);
        for (proxy_seq, origin_epoch) in
            [(1, HLS_PROVISIONING_ORIGIN_EPOCH), (2, HLS_PROVISIONING_GAP_ORIGIN_EPOCH), (10, 1), (11, 1)]
        {
            session.segments.insert(proxy_seq, ready_segment(&session, proxy_seq, origin_epoch, 2_000, 250_000));
        }

        assert_eq!(session.begin_bandwidth_persistence(1_000), None);

        let mut third_origin_segment = ready_segment(&session, 12, 1, 2_000, 250_000);
        third_origin_segment.status = SegmentCacheStatus::Discovered;
        session.segments.insert(12, third_origin_segment);
        assert_eq!(session.begin_bandwidth_persistence(1_001), None);

        session.segments.get_mut(&12).expect("third origin segment").status =
            SegmentCacheStatus::Ready { content_length: 250_000, ready_at_ms: 2 };
        assert_eq!(session.begin_bandwidth_persistence(1_002), Some(1_000_000));
        assert_eq!(session.bandwidth_persistence, HlsBandwidthPersistenceState::InFlight { bitrate_bps: 1_000_000 });
    }

    #[test]
    fn hls_bandwidth_persistence_deduplicates_and_retries_only_after_deadline() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "runtime-bandwidth-retry"), b"secret", 0);
        session.target_duration = Some(4);
        for proxy_seq in 1..=3 {
            session.segments.insert(proxy_seq, ready_segment(&session, proxy_seq, 1, 2_000, 250_000));
        }

        let bitrate_bps = session.begin_bandwidth_persistence(1_000).expect("persistence candidate");
        assert_eq!(session.begin_bandwidth_persistence(1_001), None);
        session.finish_bandwidth_persistence(
            bitrate_bps.saturating_add(1),
            HlsBandwidthPersistenceOutcome::Persisted,
            1_002,
        );
        assert_eq!(session.bandwidth_persistence, HlsBandwidthPersistenceState::InFlight { bitrate_bps });

        session.finish_bandwidth_persistence(bitrate_bps, HlsBandwidthPersistenceOutcome::RetryAfter, 2_000);
        assert_eq!(session.bandwidth_persistence, HlsBandwidthPersistenceState::RetryAfter { retry_at_ms: 32_000 });
        assert_eq!(session.begin_bandwidth_persistence(31_999), None);
        assert_eq!(session.begin_bandwidth_persistence(32_000), Some(bitrate_bps));

        session.finish_bandwidth_persistence(bitrate_bps, HlsBandwidthPersistenceOutcome::Persisted, 32_001);
        assert_eq!(session.bandwidth_persistence, HlsBandwidthPersistenceState::Persisted { bitrate_bps });
        assert_eq!(session.begin_bandwidth_persistence(u64::MAX), None);
    }

    #[test]
    fn hls_bandwidth_persistence_permanently_inapplicable_is_terminal_without_being_persisted() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "runtime-bandwidth-inapplicable"), b"secret", 0);
        session.target_duration = Some(4);
        for proxy_seq in 1..=3 {
            session.segments.insert(proxy_seq, ready_segment(&session, proxy_seq, 1, 2_000, 250_000));
        }

        let bitrate_bps = session.begin_bandwidth_persistence(1_000).expect("persistence candidate");
        session.finish_bandwidth_persistence(
            bitrate_bps,
            HlsBandwidthPersistenceOutcome::PermanentlyInapplicable,
            1_001,
        );

        assert_eq!(
            session.bandwidth_persistence,
            HlsBandwidthPersistenceState::PermanentlyInapplicable { bitrate_bps }
        );
        assert_eq!(session.begin_bandwidth_persistence(u64::MAX), None);
    }

    fn episode_timing(started_at_ms: u64, burst_plan: HlsManifestRecoveryBurstPlan) -> HlsAcceptanceEpisodeTiming {
        HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms,
            burst_plan,
            target_duration_ms: 4_000,
            transition_margin: HlsTransitionMarginMs::from_millis(1_000),
            workload: HlsRecoveryWorkload::clear_fetch(),
            observed_latency: HlsObservedRecoveryLatency::default(),
            required_terminal_media_key: None,
            terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
            policy: HlsRecoveryTimingPolicy::new(
                HlsOperationTimeoutMs::from_millis(1_000),
                HlsOperationTimeoutMs::from_millis(2_000),
                HlsRecoveryEtaMs::from_millis(300),
                HlsRecoveryEtaMs::from_millis(400),
            ),
        })
    }

    #[test]
    fn hls_recovery_timing_session_begin_stores_exact_episode_snapshot() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let plan = HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 };
        let frozen_timing = episode_timing(1_000, plan);

        let generation = session.origin_control.begin_acceptance_episode(
            1_000,
            plan,
            super::HlsManifestAcceptanceTrigger::Critical,
            &frozen_timing,
        );
        let later_timing = episode_timing(10_000, plan);

        assert_ne!(later_timing.acceptance_deadline, frozen_timing.acceptance_deadline);
        assert_eq!(
            session
                .origin_control
                .acceptance_episode
                .as_ref()
                .map(super::super::manifest_acceptance::HlsManifestAcceptanceEpisode::timing),
            Some(frozen_timing)
        );
        assert_eq!(
            session.origin_control.acceptance_episode.as_ref().map(|episode| episode.generation),
            Some(generation)
        );
    }

    #[test]
    fn hls_cutover_policy_acceptance_lifecycle_does_not_advance_media_progress_generation() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let plan = HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 };
        let initial_progress_generation = session.origin_control.progress_generation;

        let first = session.origin_control.begin_acceptance_episode(
            1_000,
            plan,
            super::HlsManifestAcceptanceTrigger::RecoveryRequired,
            &episode_timing(1_000, plan),
        );
        session.origin_control.acceptance_episode.as_mut().expect("acceptance episode").complete();
        let second = session.origin_control.begin_acceptance_episode(
            2_000,
            plan,
            super::HlsManifestAcceptanceTrigger::RecoveryRequired,
            &episode_timing(2_000, plan),
        );

        assert_eq!(first.0, 1);
        assert_eq!(second.0, 2);
        assert_eq!(session.origin_control.acceptance_generation, second);
        assert_eq!(session.origin_control.progress_generation, initial_progress_generation);
    }

    #[test]
    fn hls_recovery_timing_later_samples_affect_only_a_following_episode_snapshot() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let plan = HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 };
        let frozen_timing = episode_timing(1_000, plan);
        session.origin_control.begin_acceptance_episode(
            1_000,
            plan,
            super::HlsManifestAcceptanceTrigger::RecoveryRequired,
            &frozen_timing,
        );

        session.origin_control.recovery_samples.record(60_000);
        let next_timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
            started_at_ms: 2_000,
            burst_plan: plan,
            target_duration_ms: frozen_timing.target_duration_ms,
            transition_margin: frozen_timing.transition_margin,
            workload: frozen_timing.initial_workload,
            observed_latency: session.origin_control.recovery_samples.latency_snapshot(),
            required_terminal_media_key: frozen_timing.required_terminal_media_key,
            terminal_media_preparation: frozen_timing.terminal_media_preparation,
            policy: HlsRecoveryTimingPolicy::new(
                frozen_timing.manifest_operation_timeout,
                frozen_timing.media_operation_timeout,
                HlsRecoveryEtaMs::from_millis(300),
                frozen_timing.media_object_eta,
            ),
        });

        assert_eq!(
            session
                .origin_control
                .acceptance_episode
                .as_ref()
                .map(super::super::manifest_acceptance::HlsManifestAcceptanceEpisode::timing),
            Some(frozen_timing)
        );
        assert!(next_timing.trigger_budget > frozen_timing.trigger_budget);
        assert!(next_timing.acceptance_deadline > frozen_timing.acceptance_deadline);
    }

    #[test]
    fn manifest_access_updates_only_manifest_activity() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

        session.mark_authorized_manifest_access(1_000);

        assert_eq!(session.activity.last_authorized_manifest_at_ms, Some(1_000));
        assert_eq!(session.activity.last_authorized_media_at_ms, None);
    }

    #[test]
    fn media_access_updates_media_activity_and_protection() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(12);

        session.mark_authorized_media_access(1_000);

        assert_eq!(session.activity.last_authorized_media_at_ms, Some(1_000));
        assert_eq!(
            session.account_binding_protection(5_000),
            HlsAccountBindingProtection::HardActive { until_ms: 13_000 }
        );
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
        assert_eq!(session.account_overlap_timing().hard_active_window_ms, 12_000);
        assert_eq!(session.account_overlap_timing().soft_active_window_ms, 24_000);
    }

    #[test]
    fn missing_target_duration_uses_account_overlap_fallback_windows() {
        let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

        assert_eq!(session.account_overlap_timing().target_duration_ms, 15_000);
        assert_eq!(session.account_overlap_timing().hard_active_window_ms, 15_000);
        assert_eq!(session.account_overlap_timing().soft_active_window_ms, 30_000);
    }

    #[test]
    fn no_media_activity_is_not_expired_protection() {
        let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

        assert_eq!(session.account_binding_protection(1_000), HlsAccountBindingProtection::NoMediaYet);
        assert!(session.should_refresh_origin_reservation(1_000));
    }

    #[test]
    fn idle_session_can_expire_at_exact_idle_boundary() {
        let session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

        assert!(session.can_expire_idle_session(1_000, 1_000));
    }

    #[test]
    fn active_origin_work_blocks_idle_session_expiry() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.start_origin_work();

        assert!(!session.can_expire_idle_session(1_000, 1_000));
    }

    #[test]
    fn terminal_tail_protection_blocks_idle_session_expiry() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.install_terminal_tail_protection(
            HlsAccessLeaseId("terminal-lease".to_string()),
            HlsTerminalTailProtection {
                generation: HlsTerminalTailGeneration(1),
                base_proxy_seqs: Arc::from([41_u64, 42]),
                key_bindings: Arc::from([]),
            },
        );

        assert!(!session.can_expire_idle_session(1_000, 1_000));

        session.clear_terminal_tail_protections();
        assert!(session.can_expire_idle_session(1_000, 1_000));
    }

    #[test]
    fn terminal_tail_protection_capacity_is_exact_and_existing_lease_is_replaceable() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        for index in 0..HLS_TERMINAL_TAIL_PROTECTION_CAPACITY {
            assert_eq!(
                session.install_terminal_tail_protection(
                    HlsAccessLeaseId(format!("lease-{index}")),
                    HlsTerminalTailProtection {
                        generation: HlsTerminalTailGeneration(1),
                        base_proxy_seqs: Arc::from([u64::try_from(index).unwrap_or(u64::MAX)]),
                        key_bindings: Arc::from([]),
                    },
                ),
                HlsTerminalTailProtectionInstall::Installed
            );
        }
        assert_eq!(session.terminal_tail_protection_count(), HLS_TERMINAL_TAIL_PROTECTION_CAPACITY);
        assert_eq!(
            session.install_terminal_tail_protection(
                HlsAccessLeaseId("overflow".to_string()),
                HlsTerminalTailProtection {
                    generation: HlsTerminalTailGeneration(1),
                    base_proxy_seqs: Arc::from([9_999]),
                    key_bindings: Arc::from([]),
                },
            ),
            HlsTerminalTailProtectionInstall::CapacityExceeded
        );
        assert_eq!(
            session.install_terminal_tail_protection(
                HlsAccessLeaseId("lease-0".to_string()),
                HlsTerminalTailProtection {
                    generation: HlsTerminalTailGeneration(2),
                    base_proxy_seqs: Arc::from([42]),
                    key_bindings: Arc::from([]),
                },
            ),
            HlsTerminalTailProtectionInstall::Installed
        );
        assert_eq!(
            session
                .terminal_tail_protection(&HlsAccessLeaseId("lease-0".to_string()))
                .map(|protection| protection.base_proxy_seqs.as_ref()),
            Some([42_u64].as_slice())
        );
    }

    #[test]
    fn terminal_tail_protection_release_removes_stale_generation_after_lease_end() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let lease_id = HlsAccessLeaseId("terminal-lease".to_string());
        let current_generation = HlsTerminalTailGeneration(2);
        assert_eq!(
            session.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: current_generation,
                    base_proxy_seqs: Arc::from([41_u64, 42]),
                    key_bindings: Arc::from([]),
                },
            ),
            HlsTerminalTailProtectionInstall::Installed
        );

        assert_eq!(
            session.remove_terminal_tail_protection_after_lease_end(&lease_id, HlsTerminalTailGeneration(1)),
            HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual: current_generation }
        );
        assert!(session.terminal_tail_protection(&lease_id).is_none());

        assert_eq!(
            session.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: current_generation,
                    base_proxy_seqs: Arc::from([41_u64, 42]),
                    key_bindings: Arc::from([]),
                },
            ),
            HlsTerminalTailProtectionInstall::Installed
        );
        assert_eq!(
            session.remove_terminal_tail_protection_after_lease_end(&lease_id, current_generation),
            HlsTerminalTailProtectionRemoval::Removed
        );
        assert!(session.terminal_tail_protection(&lease_id).is_none());
    }

    #[test]
    fn origin_work_generation_invalidates_queued_work() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let started_generation = session.start_origin_work();

        session.invalidate_queued_origin_work();

        assert!(!session.finish_origin_work(started_generation));
        assert_eq!(session.activity.active_origin_work_count, 0);
        assert_eq!(session.activity.origin_work_generation, 1);
    }

    #[test]
    fn temporary_segment_failures_reach_threshold() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);

        assert_eq!(
            session.record_temporary_segment_fetch_failure(
                1_000,
                HlsSegmentFailureObject::Normal { proxy_seq: 1, origin_seq: 10 },
                2,
            ),
            HlsSegmentFailureTransition::StillRetryable { failures: 1, threshold: 2 }
        );
        assert_eq!(
            session.record_temporary_segment_fetch_failure(
                2_000,
                HlsSegmentFailureObject::Normal { proxy_seq: 2, origin_seq: 11 },
                2,
            ),
            HlsSegmentFailureTransition::BecamePermanentlyFailed { failures: 2, threshold: 2 }
        );
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 2);
        assert_eq!(session.segment_failure_tracker.last_failure_at_ms, Some(2_000));
        assert_eq!(
            session.segment_failure_tracker.last_failed_object,
            Some(HlsSegmentFailureObject::Normal { proxy_seq: 2, origin_seq: 11 })
        );
    }

    #[test]
    fn successful_segment_fetch_resets_temporary_failure_counter() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let transition = session.record_temporary_segment_fetch_failure(
            1_000,
            HlsSegmentFailureObject::Transient { resource_id: "abc".to_string() },
            3,
        );

        assert_eq!(transition, HlsSegmentFailureTransition::StillRetryable { failures: 1, threshold: 3 });
        assert_eq!(session.record_successful_segment_fetch(), Some(1));
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
        assert!(session.segment_failure_tracker.last_failure_at_ms.is_none());
        assert!(session.segment_failure_tracker.last_failed_object.is_none());
    }

    #[test]
    fn host_local_highwater_eviction_uses_observation_recency_not_key_order() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.record_host_local_highwater(1, "zz-old".to_string(), 90, 1);
        for index in 0..31_u64 {
            session.origin_control.record_host_local_highwater(
                1,
                format!("host-{index:02}"),
                index,
                index.saturating_add(2),
            );
        }
        session.origin_control.record_host_local_highwater(1, "00-new".to_string(), 100, 100);

        assert_eq!(session.origin_control.host_local_highwaters.len(), 32);
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(1, "zz-old".to_string())));
        assert!(session.origin_control.host_local_highwaters.contains_key(&(1, "00-new".to_string())));
    }

    #[test]
    fn host_local_highwater_refreshes_recency_without_lowering_value() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.record_host_local_highwater(1, "refreshed".to_string(), 100, 1);
        for index in 0..31_u64 {
            session.origin_control.record_host_local_highwater(
                1,
                format!("host-{index:02}"),
                index,
                index.saturating_add(2),
            );
        }
        session.origin_control.record_host_local_highwater(1, "refreshed".to_string(), 50, 100);
        session.origin_control.record_host_local_highwater(1, "new".to_string(), 200, 101);

        let refreshed = session.origin_control.host_local_highwaters.get(&(1, "refreshed".to_string()));
        assert_eq!(refreshed.map(|entry| entry.highwater), Some(100));
        assert_eq!(refreshed.map(|entry| entry.last_observed_at_ms), Some(100));
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(1, "host-00".to_string())));
    }

    #[test]
    fn host_local_highwater_clock_rollback_keeps_later_observation_recent() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.record_host_local_highwater(1, "host-a".to_string(), 100, 10_000);
        for index in 0..31_u64 {
            session.origin_control.record_host_local_highwater(
                1,
                format!("host-{index:02}"),
                index,
                10_001_u64.saturating_add(index),
            );
        }

        session.origin_control.record_host_local_highwater(1, "host-a".to_string(), 50, 1);
        session.origin_control.record_host_local_highwater(1, "new".to_string(), 200, 2);

        let host_a = session.origin_control.host_local_highwaters.get(&(1, "host-a".to_string()));
        assert_eq!(host_a.map(|entry| entry.highwater), Some(100));
        assert_eq!(host_a.map(|entry| entry.last_observed_at_ms), Some(10_000));
        assert!(session.origin_control.host_local_highwaters.contains_key(&(1, "host-a".to_string())));
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(1, "host-00".to_string())));
    }

    #[test]
    fn host_local_highwater_same_timestamp_evicts_oldest_observation_not_host_name() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.record_host_local_highwater(1, "zz-old".to_string(), 90, 10);
        for index in 0..31_u64 {
            session.origin_control.record_host_local_highwater(1, format!("host-{index:02}"), index, 10);
        }
        session.origin_control.record_host_local_highwater(1, "00-new".to_string(), 100, 10);

        assert_eq!(session.origin_control.host_local_highwaters.len(), 32);
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(1, "zz-old".to_string())));
        assert!(session.origin_control.host_local_highwaters.contains_key(&(1, "00-new".to_string())));
    }

    fn alternative_cohort(effective_host: &str) -> HlsAlternativeOriginCohort {
        HlsAlternativeOriginCohort {
            identity: HlsAlternativeOriginCohortIdentity {
                effective_host: effective_host.to_string(),
                technical_signature: HlsManifestTechnicalSignature {
                    map_and_encryption_hash: [0; 32],
                    container_signature_hash: [0; 32],
                },
            },
            window: HlsAlternativeOriginWindow {
                host_local_media_sequence: 1,
                host_local_highwater: Some(1),
                fingerprint: HlsManifestTimelineFingerprint {
                    segment_count: 1,
                    first_program_date_time_ms: None,
                    last_program_date_time_ms: None,
                    duration_pattern_hash: [0; 32],
                    discontinuity_pattern_hash: [0; 32],
                    normalized_resource_pattern_hash: None,
                    map_and_encryption_hash: [0; 32],
                    container_signature_hash: [0; 32],
                    segment_samples: Vec::new(),
                },
            },
            successful_samples: 1,
            total_samples: 1,
            consecutive_confirmed_full_bursts: 1,
            evidence: HlsCrossHostAcceptanceEvidence::Insufficient,
            best_candidate_index: 0,
        }
    }

    #[test]
    fn host_local_highwater_eviction_protects_pinned_and_current_cohort_hosts() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.origin_epoch = 7;
        session.origin_control.pinned_host = Some("pinned".to_string());
        let plan = HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 };
        let generation = session.origin_control.begin_acceptance_episode(
            1,
            plan,
            super::HlsManifestAcceptanceTrigger::Critical,
            &episode_timing(1, plan),
        );
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        assert_eq!(episode.generation, generation);
        episode.held_alternative = Some(alternative_cohort("candidate"));
        session.origin_control.record_host_local_highwater(7, "pinned".to_string(), 1, 0);
        session.origin_control.record_host_local_highwater(7, "candidate".to_string(), 1, 1);
        for index in 0..30_u64 {
            session.origin_control.record_host_local_highwater(
                7,
                format!("unprotected-{index:02}"),
                index,
                index.saturating_add(2),
            );
        }
        session.origin_control.record_host_local_highwater(7, "new".to_string(), 100, 100);

        assert_eq!(session.origin_control.host_local_highwaters.len(), 32);
        assert!(session.origin_control.host_local_highwaters.contains_key(&(7, "pinned".to_string())));
        assert!(session.origin_control.host_local_highwaters.contains_key(&(7, "candidate".to_string())));
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(7, "unprotected-00".to_string())));
    }

    #[test]
    fn host_local_highwater_eviction_does_not_protect_completed_episode_landscape() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.origin_control.origin_epoch = 7;
        session.origin_control.pinned_host = Some("pinned".to_string());
        let plan = HlsManifestRecoveryBurstPlan { slots: 6, lanes_per_slot: 2 };
        session.origin_control.begin_acceptance_episode(
            1,
            plan,
            super::HlsManifestAcceptanceTrigger::Critical,
            &episode_timing(1, plan),
        );
        let stale = alternative_cohort("stale-candidate");
        let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
        episode.observed_landscape = Some(HlsManifestAcceptanceLandscape {
            pinned_state: HlsPinnedOriginObservationState::Missing,
            alternatives: vec![(stale.identity, stale.window)],
        });
        episode.complete();
        session.origin_control.record_host_local_highwater(7, "pinned".to_string(), 1, 0);
        session.origin_control.record_host_local_highwater(7, "stale-candidate".to_string(), 1, 1);
        for index in 0..30_u64 {
            session.origin_control.record_host_local_highwater(
                7,
                format!("current-{index:02}"),
                index,
                index.saturating_add(2),
            );
        }
        session.origin_control.record_host_local_highwater(7, "new".to_string(), 100, 100);

        assert_eq!(session.origin_control.host_local_highwaters.len(), 32);
        assert!(session.origin_control.host_local_highwaters.contains_key(&(7, "pinned".to_string())));
        assert!(!session.origin_control.host_local_highwaters.contains_key(&(7, "stale-candidate".to_string())));
    }

    #[test]
    fn effective_origin_policy_upgrade_is_applied_immediately() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(12);

        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Soft, -20)), 1_000);
        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Normal, 50)), 1_001);
        let normal_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(normal_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(normal_policy.priority, 50);

        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Normal, -5)), 1_002);
        let upgraded_priority = session.effective_origin_acquire_policy_or_default();
        assert_eq!(upgraded_priority.connection_kind, ConnectionKind::Normal);
        assert_eq!(upgraded_priority.priority, -5);
    }

    #[test]
    fn effective_origin_policy_downgrade_waits_for_session_target_duration() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(12);
        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Normal, -5)), 1_000);

        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Soft, -20)), 5_000);
        let protected_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(protected_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(protected_policy.priority, -5);

        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Soft, -20)), 13_000);
        let downgraded_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(downgraded_policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(downgraded_policy.priority, -20);
    }

    #[test]
    fn effective_origin_policy_downgrades_without_delay_when_target_duration_is_unknown() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Normal, -5)), 1_000);

        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Soft, -20)), 1_001);

        let policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(policy.priority, -20);
    }

    #[test]
    fn effective_origin_policy_clear_waits_for_session_target_duration() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(10);
        session.reconcile_effective_origin_acquire_policy(Some(origin_policy(ConnectionKind::Normal, -5)), 1_000);

        session.reconcile_effective_origin_acquire_policy(None, 5_000);
        assert!(session.effective_origin_acquire_policy.is_some());
        let protected_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(protected_policy.priority, -5);

        session.reconcile_effective_origin_acquire_policy(None, 11_000);
        assert!(session.effective_origin_acquire_policy.is_none());
        let fallback = session.effective_origin_acquire_policy_or_default();
        assert_eq!(fallback.connection_kind, ConnectionKind::Normal);
        assert_eq!(fallback.priority, 0);
    }
}
