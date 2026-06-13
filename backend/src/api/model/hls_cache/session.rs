use super::{
    build_proxy_session_id, classify_account_binding_protection, HlsAccountBindingProtection,
    HlsAccountOverlapTiming, HlsEffectiveOriginAcquirePolicy, HlsEffectiveOriginAcquirePolicyState,
    HlsOriginAccountBinding, HlsOriginAccountIoLease, HlsOriginAccountRebindState, HlsOriginSource,
    HlsSessionKey, MapCacheStatus, MapEntry, OriginMapKey, OriginRefreshState, OriginSegmentKey, ProxyMapId,
    ProxySessionId, RenderPolicy, RenderedManifest, SegmentCacheStatus, SegmentEntry, SegmentPrefetchQueue,
    TransientPassthroughState,
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

#[derive(Debug, Clone, Default)]
pub struct HlsSessionActivity {
    pub last_authorized_manifest_at_ms: Option<u64>,
    pub last_authorized_media_at_ms: Option<u64>,
    pub active_access_lease_count: usize,
    pub active_origin_work_count: usize,
    pub origin_work_generation: u64,
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
    pub last_redirect_host: Option<String>,
    pub last_successful_manifest_target_url: Option<String>,
    pub last_successful_manifest_provider_url_index: Option<usize>,
    pub origin_refresh: OriginRefreshState,
    pub render_policy: RenderPolicy,
    pub last_rendered_manifest: Option<RenderedManifest>,
    pub longest_rendered_playlist_duration_ms: u64,
    pub segment_prefetch_queue: SegmentPrefetchQueue,
    pub active_segment_fetches: usize,
    pub segment_fetch_notifiers: HashMap<u64, Arc<Notify>>,
    pub origin_request_headers: HeaderMap,
    pub activity: HlsSessionActivity,
    pub origin_epoch: u64,
    pub origin_seq_highwater: Option<u64>,
    pub proxy_next_seq: Option<u64>,
    pub origin_to_proxy: HashMap<OriginSegmentKey, u64>,
    pub discontinuity_sequence: u64,
    pub pending_handoff_discontinuity_sequence: Option<u64>,
    pub segments: BTreeMap<u64, SegmentEntry>,
    pub active_map_fetches: usize,
    pub maps: BTreeMap<ProxyMapId, MapEntry>,
    pub origin_map_to_proxy: HashMap<OriginMapKey, ProxyMapId>,
    pub next_proxy_map_id: u64,
    pub publishable_origin_tail_proxy_seq: Option<u64>,
    pub origin_version: Option<u16>,
    pub target_duration: Option<u32>,
    pub independent_segments: bool,
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
            last_redirect_host: None,
            last_successful_manifest_target_url: None,
            last_successful_manifest_provider_url_index: None,
            origin_refresh: OriginRefreshState::default(),
            render_policy: RenderPolicy::default(),
            last_rendered_manifest: None,
            longest_rendered_playlist_duration_ms: 0,
            segment_prefetch_queue: SegmentPrefetchQueue::default(),
            active_segment_fetches: 0,
            segment_fetch_notifiers: HashMap::new(),
            origin_request_headers: HeaderMap::new(),
            activity: HlsSessionActivity::default(),
            origin_epoch: 0,
            origin_seq_highwater: None,
            proxy_next_seq: None,
            origin_to_proxy: HashMap::new(),
            discontinuity_sequence: 0,
            pending_handoff_discontinuity_sequence: None,
            active_map_fetches: 0,
            segments: BTreeMap::new(),
            maps: BTreeMap::new(),
            origin_map_to_proxy: HashMap::new(),
            next_proxy_map_id: 0,
            publishable_origin_tail_proxy_seq: None,
            origin_version: None,
            target_duration: None,
            independent_segments: false,
            gc_marked_for_removal: false,
        }
    }

    pub fn mark_pending_handoff_discontinuity(&mut self, discontinuity_sequence: u64) {
        self.pending_handoff_discontinuity_sequence = Some(discontinuity_sequence);
    }

    pub fn take_pending_handoff_discontinuity_sequence(&mut self) -> Option<u64> {
        self.pending_handoff_discontinuity_sequence.take()
    }

    pub fn mark_for_gc_removal(&mut self) { self.gc_marked_for_removal = true; }

    pub fn clear_gc_removal_mark(&mut self) { self.gc_marked_for_removal = false; }

    pub fn is_gc_marked_for_removal(&self) -> bool { self.gc_marked_for_removal }

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
            .field("last_redirect_host", &self.last_redirect_host)
            .field(
                "last_successful_manifest_target_url",
                &self.last_successful_manifest_target_url.as_ref().map(|_| "<redacted>"),
            )
            .field("last_successful_manifest_provider_url_index", &self.last_successful_manifest_provider_url_index)
            .field("origin_refresh", &self.origin_refresh)
            .field("render_policy", &self.render_policy)
            .field("last_rendered_manifest", &self.last_rendered_manifest)
            .field("longest_rendered_playlist_duration_ms", &self.longest_rendered_playlist_duration_ms)
            .field("segment_prefetch_queue_len", &self.segment_prefetch_queue.len())
            .field("active_segment_fetches", &self.active_segment_fetches)
            .field("segment_fetch_notifiers_len", &self.segment_fetch_notifiers.len())
            .field("origin_request_headers_len", &self.origin_request_headers.len())
            .field("activity", &self.activity)
            .field("origin_epoch", &self.origin_epoch)
            .field("origin_seq_highwater", &self.origin_seq_highwater)
            .field("proxy_next_seq", &self.proxy_next_seq)
            .field("origin_to_proxy_len", &self.origin_to_proxy.len())
            .field("discontinuity_sequence", &self.discontinuity_sequence)
            .field(
                "pending_handoff_discontinuity_sequence",
                &self.pending_handoff_discontinuity_sequence,
            )
            .field("active_map_fetches", &self.active_map_fetches)
            .field("segments_len", &self.segments.len())
            .field("maps_len", &self.maps.len())
            .field("origin_map_to_proxy_len", &self.origin_map_to_proxy.len())
            .field("next_proxy_map_id", &self.next_proxy_map_id)
            .field("publishable_origin_tail_proxy_seq", &self.publishable_origin_tail_proxy_seq)
            .field("origin_version", &self.origin_version)
            .field("target_duration", &self.target_duration)
            .field("independent_segments", &self.independent_segments)
            .field("gc_marked_for_removal", &self.gc_marked_for_removal)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::HlsSession;
    use crate::api::model::{ConnectionKind, HlsAccountBindingProtection, HlsEffectiveOriginAcquirePolicy, HlsSessionKey};

    fn origin_policy(connection_kind: ConnectionKind, priority: i8) -> HlsEffectiveOriginAcquirePolicy {
        HlsEffectiveOriginAcquirePolicy::new(connection_kind, priority, 0)
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
    fn origin_work_generation_invalidates_queued_work() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let started_generation = session.start_origin_work();

        session.invalidate_queued_origin_work();

        assert!(!session.finish_origin_work(started_generation));
        assert_eq!(session.activity.active_origin_work_count, 0);
        assert_eq!(session.activity.origin_work_generation, 1);
    }

    #[test]
    fn effective_origin_policy_upgrade_is_applied_immediately() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(12);

        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Soft, -20)),
            1_000,
        );
        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Normal, 50)),
            1_001,
        );
        let normal_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(normal_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(normal_policy.priority, 50);

        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Normal, -5)),
            1_002,
        );
        let upgraded_priority = session.effective_origin_acquire_policy_or_default();
        assert_eq!(upgraded_priority.connection_kind, ConnectionKind::Normal);
        assert_eq!(upgraded_priority.priority, -5);
    }

    #[test]
    fn effective_origin_policy_downgrade_waits_for_session_target_duration() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(12);
        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Normal, -5)),
            1_000,
        );

        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Soft, -20)),
            5_000,
        );
        let protected_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(protected_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(protected_policy.priority, -5);

        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Soft, -20)),
            13_000,
        );
        let downgraded_policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(downgraded_policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(downgraded_policy.priority, -20);
    }

    #[test]
    fn effective_origin_policy_downgrades_without_delay_when_target_duration_is_unknown() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Normal, -5)),
            1_000,
        );

        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Soft, -20)),
            1_001,
        );

        let policy = session.effective_origin_acquire_policy_or_default();
        assert_eq!(policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(policy.priority, -20);
    }

    #[test]
    fn effective_origin_policy_clear_waits_for_session_target_duration() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        session.target_duration = Some(10);
        session.reconcile_effective_origin_acquire_policy(
            Some(origin_policy(ConnectionKind::Normal, -5)),
            1_000,
        );

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
