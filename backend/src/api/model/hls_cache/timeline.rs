use super::{
    media_reserve::{HlsReadyMediaState, HlsReadyTimelineSnapshot, HlsReadyTimelineUnit},
    resource_identity::{HlsMediaResourceIdentity, HlsMediaResourceSemanticKey, HlsPublishedResourceHistory},
    safe_proxy_session_id, HlsSession, MapEntry, OriginMapKey, ProxyMapId, ProxySessionId, SegmentCacheKey,
    SegmentFetchPriority, TransientResourceId,
};
use crate::processing::parser::hls::origin_manifest::{
    ParsedByteRange, ParsedOriginManifest, ParsedOriginMap, ParsedOriginSegment,
};
use axum::http::StatusCode;
use log::info;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
};
use url::Url;

const SEGMENT_EXTENSIONS: &[&str] = &["ts", "mp4", "m4s", "m4v"];
const MAP_EXTENSIONS: &[&str] = &["mp4", "m4s", "m4v"];
pub const HLS_PROVISIONING_ORIGIN_EPOCH: u64 = u64::MAX;
pub const HLS_PROVISIONING_GAP_ORIGIN_EPOCH: u64 = u64::MAX - 1;
pub const HLS_PROVISIONING_TARGET_DURATION_SECS: u32 = 2;
pub const HLS_PROVISIONING_SEGMENT_DURATION_MS: u64 = 2_000;
const EFFECTIVE_ORIGIN_HOST_ID_DOMAIN: &[u8] = b"tuliprox/hls/effective-origin-host/v1\0";
const UNKNOWN_EFFECTIVE_ORIGIN_HOST_ID: u64 = 0;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct OriginSegmentKey {
    pub origin_epoch: u64,
    pub effective_host_id: u64,
    pub host_local_sequence: u64,
    /// Saturating sequence offset from the first segment committed in this host/epoch namespace.
    pub host_local_index: u32,
}

/// Returns a stable, non-reversible identifier for an effective manifest host.
///
/// The normalized host itself is deliberately not stored in timeline or cache identities. The domain-separated hash
/// is stable across processes and does not depend on Rust's randomized `HashMap` hasher.
pub(crate) fn effective_origin_host_id(effective_host: &str) -> u64 {
    let normalized_host = effective_host.trim().trim_end_matches('.').to_ascii_lowercase();
    let mut hasher = blake3::Hasher::new();
    hasher.update(EFFECTIVE_ORIGIN_HOST_ID_DOMAIN);
    hasher.update(normalized_host.as_bytes());
    let digest = hasher.finalize();
    let mut id_bytes = [0_u8; std::mem::size_of::<u64>()];
    id_bytes.copy_from_slice(&digest.as_bytes()[..std::mem::size_of::<u64>()]);
    let id = u64::from_le_bytes(id_bytes);
    if id == UNKNOWN_EFFECTIVE_ORIGIN_HOST_ID {
        1
    } else {
        id
    }
}

/// Volatile concrete origin URL for one segment download.
///
/// The URL is resolved against the final fetched manifest URL and may include a redirect/CDN host. Use it only as a
/// fetch target or sanitized diagnostics; stable timeline identity is `OriginSegmentKey`, and cache identity is the
/// proxy sequence based `SegmentCacheKey`.
#[derive(Clone, Eq, PartialEq)]
pub struct OriginSegmentFetchRef {
    /// Concrete URL used to refetch the segment object.
    ///
    /// This starts from `ParsedOriginSegment::resolved_origin_url`, which is resolved against the final manifest URL
    /// after redirects. It is a fetch target only; the normal timeline identity remains `OriginSegmentKey`.
    pub resolved_origin_url: String,
    pub byte_range: Option<ParsedByteRange>,
    pub valid_until_ms: Option<u64>,
}

impl OriginSegmentFetchRef {
    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        self.valid_until_ms.is_none_or(|valid_until_ms| now_ms <= valid_until_ms)
    }
}

impl SegmentEntry {
    pub(crate) fn media_resource_identity(&self) -> Option<HlsMediaResourceIdentity> {
        self.origin_fetch_ref.as_ref().map(|fetch_ref| {
            HlsMediaResourceIdentity::from_url(&fetch_ref.resolved_origin_url, fetch_ref.byte_range)
        })
    }
}

impl fmt::Debug for OriginSegmentFetchRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginSegmentFetchRef")
            .field("resolved_origin_url", &"<redacted>")
            .field("byte_range", &self.byte_range)
            .field("valid_until_ms", &self.valid_until_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SegmentCacheStatus {
    Discovered,
    Queued { priority: SegmentFetchPriority, queued_at_ms: u64 },
    Fetching { priority: SegmentFetchPriority, started_at_ms: u64 },
    Ready { content_length: u64, ready_at_ms: u64 },
    CapacityDeferred { priority: SegmentFetchPriority, deferred_at_ms: u64 },
    FailedRetryable { failed_at_ms: u64, retry_after_ms: u64 },
    FailedPermanent { failed_at_ms: u64, status: Option<StatusCode> },
    Expired,
}

impl SegmentCacheStatus {
    /// True only while local capacity owns a concrete revision-bound retry.
    pub(crate) const fn awaits_capacity_recovery(&self) -> bool {
        matches!(self, Self::CapacityDeferred { .. })
    }
}

/// Mutable access counters shared with response streams without holding a session lock.
#[derive(Debug, Default)]
pub struct CacheAccessState {
    active_readers: AtomicU32,
    last_accessed_at_ms: AtomicU64,
}

impl CacheAccessState {
    pub fn new() -> Self { Self::default() }

    pub fn reader_started(&self, now_ms: u64) {
        self.active_readers.fetch_add(1, Ordering::AcqRel);
        self.last_accessed_at_ms.store(now_ms, Ordering::Release);
    }

    pub fn reader_finished(&self) {
        let mut current = self.active_readers.load(Ordering::Acquire);
        while current > 0 {
            match self.active_readers.compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    pub fn active_readers(&self) -> u32 { self.active_readers.load(Ordering::Acquire) }

    pub fn last_accessed_at_ms(&self) -> u64 { self.last_accessed_at_ms.load(Ordering::Acquire) }
}

impl PartialEq for CacheAccessState {
    fn eq(&self, other: &Self) -> bool {
        self.active_readers() == other.active_readers() && self.last_accessed_at_ms() == other.last_accessed_at_ms()
    }
}

impl Eq for CacheAccessState {}

pub fn default_content_type_for_segment_ext(extension: &str) -> &'static str {
    match extension {
        "ts" => "video/mp2t",
        "mp4" | "m4v" | "m4s" => "video/mp4",
        _ => "application/octet-stream",
    }
}

pub fn is_hls_provisioning_segment(entry: &SegmentEntry) -> bool {
    entry.origin_key.origin_epoch == HLS_PROVISIONING_ORIGIN_EPOCH
}

pub fn is_hls_provisioning_gap_segment(entry: &SegmentEntry) -> bool {
    entry.origin_key.origin_epoch == HLS_PROVISIONING_GAP_ORIGIN_EPOCH
}

#[derive(Clone, Eq, PartialEq)]
pub struct SegmentEntry {
    pub origin_key: OriginSegmentKey,
    pub proxy_seq: u64,
    pub duration_ms: u64,
    pub proxy_file_ext: String,
    pub content_type: String,
    pub cache_key: SegmentCacheKey,
    pub discontinuity_before: bool,
    pub program_date_time: Option<String>,
    pub daterange_tags_before: Vec<String>,
    pub origin_byte_range: Option<ParsedByteRange>,
    pub map_ref: Option<ProxyMapId>,
    pub encryption: Option<HlsSegmentEncryption>,
    pub origin_fetch_ref: Option<OriginSegmentFetchRef>,
    pub status: SegmentCacheStatus,
    pub last_rendered_at_ms: Option<u64>,
    pub access: Arc<CacheAccessState>,
}

impl fmt::Debug for SegmentEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentEntry")
            .field("origin_key", &self.origin_key)
            .field("proxy_seq", &self.proxy_seq)
            .field("duration_ms", &self.duration_ms)
            .field("proxy_file_ext", &self.proxy_file_ext)
            .field("content_type", &self.content_type)
            .field("cache_key", &self.cache_key)
            .field("discontinuity_before", &self.discontinuity_before)
            .field("program_date_time", &self.program_date_time)
            .field("daterange_tags_before", &self.daterange_tags_before)
            .field("origin_byte_range", &self.origin_byte_range)
            .field("map_ref", &self.map_ref)
            .field("encryption", &self.encryption)
            .field("origin_fetch_ref", &self.origin_fetch_ref)
            .field("status", &self.status)
            .field("last_rendered_at_ms", &self.last_rendered_at_ms)
            .field("active_readers", &self.access.active_readers())
            .field("last_accessed_at_ms", &self.access.last_accessed_at_ms())
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum TimelineMapError {
    UnsupportedSegmentExtension,
    UnsupportedMapExtension,
    ProxySequenceOverflow,
    ProxyMapIdOverflow,
    MissingKeyResource,
    PublishedResourceReplay {
        previous_proxy_tail: Option<u64>,
        existing_proxy_seq: u64,
        candidate_position: usize,
        candidate_origin_seq: u64,
        resource_key: HlsMediaResourceSemanticKey,
        decision: HlsResourceReplayDecision,
    },
    OriginSequenceResourceConflict { existing_proxy_seq: u64, candidate_origin_seq: u64 },
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum HlsResourceReplayDecision {
    RejectReplayOnly,
    RejectContradictoryOrder,
}

impl HlsResourceReplayDecision {
    pub(crate) const fn as_log_value(self) -> &'static str {
        match self {
            Self::RejectReplayOnly => "reject-replay-only",
            Self::RejectContradictoryOrder => "reject-contradictory-order",
        }
    }
}

/// Lease-materializable encryption state bound to an opaque key-resource route.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsSegmentEncryption {
    pub resource_id: TransientResourceId,
    pub resource_extension: String,
    pub iv: Option<String>,
    pub key_format: Option<String>,
    pub key_format_versions: Option<String>,
}

/// Mutation-free view of the exact entries an explicit origin handoff would append.
///
/// Callers may use these cache keys to stage the first switch segment and its MAP. A later commit is safe only after
/// the caller has revalidated its session/acceptance generation; if the session changed in between, a fresh preview
/// must be created.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct HlsOriginHandoffPreview {
    pub(crate) origin_epoch: u64,
    pub(crate) segments: Vec<SegmentEntry>,
    pub(crate) maps: Vec<MapEntry>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum HlsOriginHandoffPreviewError {
    Timeline(TimelineMapError),
    PreviewInconsistent,
}

impl From<TimelineMapError> for HlsOriginHandoffPreviewError {
    fn from(value: TimelineMapError) -> Self { Self::Timeline(value) }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TimelineApplyMode {
    CurrentOrigin,
    ExplicitHandoff { discontinuity_sequence: u64 },
}

#[derive(Debug, Default)]
struct TimelineApplyResult {
    segment_proxy_seqs: Vec<u64>,
    map_ids: Vec<ProxyMapId>,
}

#[derive(Clone)]
struct TimelineDraft {
    proxy_session_id: ProxySessionId,
    origin_epoch: u64,
    origin_epoch_effective_host_id: Option<u64>,
    origin_epoch_sequence_base: Option<u64>,
    origin_seq_highwater: Option<u64>,
    proxy_next_seq: Option<u64>,
    origin_to_proxy: HashMap<OriginSegmentKey, u64>,
    published_resource_history: HlsPublishedResourceHistory,
    discontinuity_sequence: u64,
    pending_handoff_discontinuity_sequence: Option<u64>,
    pending_origin_epoch_handoff: bool,
    segments: BTreeMap<u64, SegmentEntry>,
    maps: BTreeMap<ProxyMapId, MapEntry>,
    origin_map_to_proxy: HashMap<OriginMapKey, ProxyMapId>,
    next_proxy_map_id: u64,
    publishable_origin_head_proxy_seq: Option<u64>,
    publishable_origin_tail_proxy_seq: Option<u64>,
    origin_version: Option<u16>,
    target_duration: Option<u32>,
    independent_segments: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum OriginEpochTransitionReason {
    Rollover,
    Handoff,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CandidateSegmentDisposition {
    CurrentMapping(u64),
    PublishedOverlap(u64),
    New,
}

#[derive(Debug)]
struct CandidateTimelinePlan<'a> {
    segments: Vec<&'a ParsedOriginSegment>,
    trimmed_published_prefix: bool,
}

impl From<&HlsSession> for TimelineDraft {
    fn from(session: &HlsSession) -> Self {
        Self {
            proxy_session_id: session.proxy_session_id.clone(),
            origin_epoch: session.origin_epoch,
            origin_epoch_effective_host_id: session.origin_epoch_effective_host_id,
            origin_epoch_sequence_base: session.origin_epoch_sequence_base,
            origin_seq_highwater: session.origin_seq_highwater,
            proxy_next_seq: session.proxy_next_seq,
            origin_to_proxy: session.origin_to_proxy.clone(),
            published_resource_history: session.published_resource_history.clone(),
            discontinuity_sequence: session.discontinuity_sequence,
            pending_handoff_discontinuity_sequence: session.pending_handoff_discontinuity_sequence,
            pending_origin_epoch_handoff: session.pending_origin_epoch_handoff,
            segments: session.segments.clone(),
            maps: session.maps.clone(),
            origin_map_to_proxy: session.origin_map_to_proxy.clone(),
            next_proxy_map_id: session.next_proxy_map_id,
            publishable_origin_head_proxy_seq: session.publishable_origin_head_proxy_seq,
            publishable_origin_tail_proxy_seq: session.publishable_origin_tail_proxy_seq,
            origin_version: session.origin_version,
            target_duration: session.target_duration,
            independent_segments: session.independent_segments,
        }
    }
}

impl HlsSession {
    #[cfg(test)]
    pub(crate) fn apply_origin_manifest(&mut self, manifest: &ParsedOriginManifest) -> Result<(), TimelineMapError> {
        let effective_host_id = inferred_effective_origin_host_id(manifest);
        self.apply_origin_manifest_for_host(manifest, effective_host_id)
    }

    /// Applies a manifest in the namespace of its effective manifest host.
    ///
    /// Passing an ID different from the currently committed host starts a new origin epoch before any sequence or
    /// highwater comparison. The caller must derive the ID from the final effective manifest host, not from a provider
    /// URL or an individual absolute segment URI.
    pub(crate) fn apply_origin_manifest_for_host(
        &mut self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
    ) -> Result<(), TimelineMapError> {
        let mut draft = TimelineDraft::from(&*self);
        draft.apply_manifest(manifest, effective_host_id, TimelineApplyMode::CurrentOrigin)?;
        self.commit_timeline_draft(draft);
        Ok(())
    }

    /// Applies a deliberately fresh baseline without reusing sequence evidence from an older epoch.
    ///
    /// A retained timeline is separated with an explicit discontinuity. If only stale baseline metadata remains,
    /// the epoch still advances, but no synthetic discontinuity is added ahead of the first media ever published by
    /// this session.
    pub(crate) fn apply_origin_rebase_manifest(
        &mut self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
    ) -> Result<(), TimelineMapError> {
        let mut draft = TimelineDraft::from(&*self);
        if draft.segments.is_empty() {
            if draft.has_origin_namespace_baseline() {
                draft
                    .start_new_origin_epoch(manifest.origin_manifest_sequence, OriginEpochTransitionReason::Handoff)?;
                if !draft.published_resource_history.is_empty() {
                    draft.pending_handoff_discontinuity_sequence = Some(0);
                }
            }
            draft.apply_manifest(manifest, effective_host_id, TimelineApplyMode::CurrentOrigin)?;
        } else {
            draft.apply_manifest(
                manifest,
                effective_host_id,
                TimelineApplyMode::ExplicitHandoff { discontinuity_sequence: 0 },
            )?;
        }
        self.commit_timeline_draft(draft);
        Ok(())
    }

    /// Previews the exact timeline/cache entries for an explicit handoff without mutating this session.
    pub(crate) fn preview_origin_handoff_manifest(
        &self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
        discontinuity_sequence: u64,
    ) -> Result<HlsOriginHandoffPreview, HlsOriginHandoffPreviewError> {
        let mut draft = TimelineDraft::from(self);
        let result = draft.apply_manifest(
            manifest,
            effective_host_id,
            TimelineApplyMode::ExplicitHandoff { discontinuity_sequence },
        )?;
        handoff_preview_from_draft(&draft, result)
    }

    /// Commits an explicit handoff only if a freshly built draft still exactly matches the previously staged preview.
    ///
    /// The orchestration layer must call this while holding the session write lock and after checking its acceptance
    /// generation. Any intervening timeline mutation changes proxy sequences, MAP IDs, cache keys or entry metadata
    /// and therefore causes `PreviewInconsistent` without modifying the session.
    pub(crate) fn apply_origin_handoff_manifest_if_preview_matches(
        &mut self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
        discontinuity_sequence: u64,
        expected_preview: &HlsOriginHandoffPreview,
    ) -> Result<(), HlsOriginHandoffPreviewError> {
        let mut draft = TimelineDraft::from(&*self);
        let result = draft.apply_manifest(
            manifest,
            effective_host_id,
            TimelineApplyMode::ExplicitHandoff { discontinuity_sequence },
        )?;
        let actual_preview = handoff_preview_from_draft(&draft, result)?;
        if actual_preview != *expected_preview {
            return Err(HlsOriginHandoffPreviewError::PreviewInconsistent);
        }
        self.commit_timeline_draft(draft);
        Ok(())
    }

    fn commit_timeline_draft(&mut self, draft: TimelineDraft) {
        self.origin_epoch = draft.origin_epoch;
        self.origin_epoch_effective_host_id = draft.origin_epoch_effective_host_id;
        self.origin_epoch_sequence_base = draft.origin_epoch_sequence_base;
        self.origin_seq_highwater = draft.origin_seq_highwater;
        self.proxy_next_seq = draft.proxy_next_seq;
        self.origin_to_proxy = draft.origin_to_proxy;
        self.published_resource_history = draft.published_resource_history;
        self.discontinuity_sequence = draft.discontinuity_sequence;
        self.pending_handoff_discontinuity_sequence = draft.pending_handoff_discontinuity_sequence;
        self.pending_origin_epoch_handoff = draft.pending_origin_epoch_handoff;
        self.segments = draft.segments;
        self.maps = draft.maps;
        self.origin_map_to_proxy = draft.origin_map_to_proxy;
        self.next_proxy_map_id = draft.next_proxy_map_id;
        self.publishable_origin_head_proxy_seq = draft.publishable_origin_head_proxy_seq;
        self.publishable_origin_tail_proxy_seq = draft.publishable_origin_tail_proxy_seq;
        self.origin_version = draft.origin_version;
        self.target_duration = draft.target_duration;
        self.independent_segments = draft.independent_segments;
    }

    /// Captures the contiguous timeline beginning at the lease's first advertised sequence.
    ///
    /// The snapshot contains only metadata and is safe to evaluate after releasing the session lock.
    pub(crate) fn ready_timeline_snapshot(&self, first_proxy_seq: u64, now_ms: u64) -> HlsReadyTimelineSnapshot {
        let mut start_ms = 0_u64;
        let mut expected_proxy_seq = first_proxy_seq;
        let mut units = Vec::new();
        for (&proxy_seq, segment) in self.segments.range(first_proxy_seq..) {
            if proxy_seq != expected_proxy_seq {
                break;
            }
            let required_map_ready = segment.map_ref.is_none_or(|map_id| {
                self.maps.get(&map_id).is_some_and(|map| matches!(map.status, super::MapCacheStatus::Ready { .. }))
            });
            let key_ready_valid_until_ms = segment.encryption.as_ref().and_then(|encryption| {
                self.transient.ready_key_object_valid_until_ms(
                    &self.proxy_session_id,
                    &encryption.resource_id,
                    &encryption.resource_extension,
                    now_ms,
                )
            });
            units.push(HlsReadyTimelineUnit {
                proxy_seq,
                start_ms,
                duration_ms: segment.duration_ms,
                state: if matches!(segment.status, SegmentCacheStatus::Ready { .. }) {
                    HlsReadyMediaState::Ready
                } else {
                    HlsReadyMediaState::NotReady
                },
                required_map_ready,
                required_key_ready: segment.encryption.is_none() || key_ready_valid_until_ms.is_some(),
                key_ready_valid_until_ms,
            });
            start_ms = start_ms.saturating_add(segment.duration_ms);
            expected_proxy_seq = expected_proxy_seq.saturating_add(1);
        }
        HlsReadyTimelineSnapshot { units: Arc::from(units) }
    }

    /// Reports whether local capacity is the first blocker at the READY
    /// boundary. Later deferred prefetches and unavailable MAP/key dependencies
    /// cannot suppress terminal handling for an unrelated origin failure.
    pub(crate) fn capacity_recovery_blocks_ready_timeline(&self, timeline: &HlsReadyTimelineSnapshot) -> bool {
        timeline
            .units
            .iter()
            .find(|unit| {
                unit.state != HlsReadyMediaState::Ready || !unit.required_map_ready || !unit.required_key_ready
            })
            .is_some_and(|unit| {
                unit.required_map_ready
                    && unit.required_key_ready
                    && self
                        .segments
                        .get(&unit.proxy_seq)
                        .is_some_and(|segment| segment.status.awaits_capacity_recovery())
            })
    }
}

fn handoff_preview_from_draft(
    draft: &TimelineDraft,
    result: TimelineApplyResult,
) -> Result<HlsOriginHandoffPreview, HlsOriginHandoffPreviewError> {
    let segments = result
        .segment_proxy_seqs
        .into_iter()
        .map(|proxy_seq| {
            draft.segments.get(&proxy_seq).cloned().ok_or(HlsOriginHandoffPreviewError::PreviewInconsistent)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let maps = result
        .map_ids
        .into_iter()
        .map(|map_id| draft.maps.get(&map_id).cloned().ok_or(HlsOriginHandoffPreviewError::PreviewInconsistent))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HlsOriginHandoffPreview { origin_epoch: draft.origin_epoch, segments, maps })
}

impl TimelineDraft {
    fn apply_manifest(
        &mut self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
        mode: TimelineApplyMode,
    ) -> Result<TimelineApplyResult, TimelineMapError> {
        let candidate_plan = self.candidate_timeline_plan(manifest, effective_host_id, mode)?;
        if candidate_plan.segments.is_empty() {
            return Ok(TimelineApplyResult::default());
        }
        if let TimelineApplyMode::ExplicitHandoff { discontinuity_sequence } = mode {
            self.pending_handoff_discontinuity_sequence = Some(discontinuity_sequence);
            self.pending_origin_epoch_handoff = true;
        }
        let handoff_discontinuity_sequence = self.pending_handoff_discontinuity_sequence.take();
        let handoff_publishable_head_proxy_seq =
            handoff_discontinuity_sequence.and(self.publishable_origin_head_proxy_seq);
        let origin_epoch_handoff = self.pending_origin_epoch_handoff;
        self.pending_origin_epoch_handoff = false;
        if self.segments.is_empty() {
            self.discontinuity_sequence = manifest
                .discontinuity_sequence
                .unwrap_or(0)
                .saturating_add(handoff_discontinuity_sequence.unwrap_or(0));
        }
        self.origin_version = manifest.version;
        self.target_duration = manifest.target_duration;
        self.independent_segments = manifest.independent_segments;
        let epoch_transition_discontinuity =
            self.apply_origin_epoch_transition_for_manifest(manifest, effective_host_id, origin_epoch_handoff)?;
        // The transition above resets the highwater before a host handoff. Never compare a candidate host's media
        // sequence with a highwater committed for another effective host.
        self.apply_forward_jump(manifest)?;

        let mut mark_handoff_discontinuity = handoff_discontinuity_sequence.is_some()
            || epoch_transition_discontinuity
            || candidate_plan.trimmed_published_prefix;
        let mut manifest_head_proxy_seq = None;
        let mut manifest_tail_proxy_seq = None;
        let mut result = TimelineApplyResult::default();
        for parsed in candidate_plan.segments {
            let proxy_seq = self.map_origin_segment(parsed, manifest, effective_host_id, mark_handoff_discontinuity)?;
            manifest_head_proxy_seq.get_or_insert(proxy_seq);
            manifest_tail_proxy_seq = Some(proxy_seq);
            result.segment_proxy_seqs.push(proxy_seq);
            if let Some(map_id) = self.segments.get(&proxy_seq).and_then(|segment| segment.map_ref) {
                if !result.map_ids.contains(&map_id) {
                    result.map_ids.push(map_id);
                }
            }
            mark_handoff_discontinuity = false;
        }

        self.publishable_origin_head_proxy_seq = handoff_publishable_head_proxy_seq.or(manifest_head_proxy_seq);
        self.publishable_origin_tail_proxy_seq = manifest_tail_proxy_seq;
        Ok(result)
    }

    fn candidate_timeline_plan<'a>(
        &self,
        manifest: &'a ParsedOriginManifest,
        effective_host_id: u64,
        mode: TimelineApplyMode,
    ) -> Result<CandidateTimelinePlan<'a>, TimelineMapError> {
        let effective_host_changed = self
            .origin_epoch_effective_host_id
            .is_some_and(|committed_host_id| committed_host_id != effective_host_id);
        let explicit_handoff = matches!(mode, TimelineApplyMode::ExplicitHandoff { .. })
            || self.pending_origin_epoch_handoff
            || effective_host_changed
            || self.should_start_new_origin_epoch_for_rollover(manifest);
        let mut segments = Vec::with_capacity(manifest.segments.len());
        let mut trimmed_published_prefix = false;
        let mut first_published_overlap = None;

        for (candidate_position, parsed) in manifest.segments.iter().enumerate() {
            let resource_identity =
                HlsMediaResourceIdentity::from_url(&parsed.resolved_origin_url, parsed.origin_byte_range);
            let disposition = self.classify_candidate_segment(parsed, effective_host_id)?;
            match disposition {
                CandidateSegmentDisposition::CurrentMapping(proxy_seq) if !explicit_handoff => {
                    if trimmed_published_prefix {
                        return Err(TimelineMapError::PublishedResourceReplay {
                            previous_proxy_tail: self.proxy_next_seq.and_then(|next| next.checked_sub(1)),
                            existing_proxy_seq: proxy_seq,
                            candidate_position,
                            candidate_origin_seq: parsed.origin_seq,
                            resource_key: resource_identity.semantic_key(),
                            decision: HlsResourceReplayDecision::RejectContradictoryOrder,
                        });
                    }
                    segments.push(parsed);
                }
                CandidateSegmentDisposition::CurrentMapping(proxy_seq)
                | CandidateSegmentDisposition::PublishedOverlap(proxy_seq) => {
                    if !segments.is_empty() {
                        return Err(TimelineMapError::PublishedResourceReplay {
                            previous_proxy_tail: self.proxy_next_seq.and_then(|next| next.checked_sub(1)),
                            existing_proxy_seq: proxy_seq,
                            candidate_position,
                            candidate_origin_seq: parsed.origin_seq,
                            resource_key: resource_identity.semantic_key(),
                            decision: HlsResourceReplayDecision::RejectContradictoryOrder,
                        });
                    }
                    trimmed_published_prefix = true;
                    first_published_overlap.get_or_insert((
                        proxy_seq,
                        candidate_position,
                        parsed.origin_seq,
                        resource_identity.semantic_key(),
                    ));
                }
                CandidateSegmentDisposition::New => segments.push(parsed),
            }
        }

        if segments.is_empty() && !explicit_handoff && trimmed_published_prefix {
            let (existing_proxy_seq, candidate_position, candidate_origin_seq, resource_key) =
                first_published_overlap.unwrap_or_default();
            return Err(TimelineMapError::PublishedResourceReplay {
                previous_proxy_tail: self.proxy_next_seq.and_then(|next| next.checked_sub(1)),
                existing_proxy_seq,
                candidate_position,
                candidate_origin_seq,
                resource_key,
                decision: HlsResourceReplayDecision::RejectReplayOnly,
            });
        }

        Ok(CandidateTimelinePlan { segments, trimmed_published_prefix })
    }

    fn classify_candidate_segment(
        &self,
        parsed: &ParsedOriginSegment,
        effective_host_id: u64,
    ) -> Result<CandidateSegmentDisposition, TimelineMapError> {
        let identity = HlsMediaResourceIdentity::from_url(&parsed.resolved_origin_url, parsed.origin_byte_range);
        if let Some(current_key) = self.current_origin_key(effective_host_id, parsed.origin_seq) {
            if let Some(proxy_seq) = self.origin_to_proxy.get(&current_key).copied() {
                let current_identity = self.segments.get(&proxy_seq).and_then(SegmentEntry::media_resource_identity);
                if current_identity.is_some_and(|current| current.matches(identity)) {
                    return Ok(CandidateSegmentDisposition::CurrentMapping(proxy_seq));
                }
                if let Some(existing_proxy_seq) = self.proxy_seq_for_resource(identity) {
                    return Ok(CandidateSegmentDisposition::PublishedOverlap(existing_proxy_seq));
                }
                return Err(TimelineMapError::OriginSequenceResourceConflict {
                    existing_proxy_seq: proxy_seq,
                    candidate_origin_seq: parsed.origin_seq,
                });
            }
        }
        Ok(self.proxy_seq_for_resource(identity).map_or(
            CandidateSegmentDisposition::New,
            CandidateSegmentDisposition::PublishedOverlap,
        ))
    }

    fn current_origin_key(&self, effective_host_id: u64, host_local_sequence: u64) -> Option<OriginSegmentKey> {
        let sequence_base = self.origin_epoch_sequence_base?;
        Some(OriginSegmentKey {
            origin_epoch: self.origin_epoch,
            effective_host_id,
            host_local_sequence,
            host_local_index: u32::try_from(host_local_sequence.saturating_sub(sequence_base)).unwrap_or(u32::MAX),
        })
    }

    fn proxy_seq_for_resource(&self, identity: HlsMediaResourceIdentity) -> Option<u64> {
        self.segments
            .values()
            .find_map(|segment| {
                segment
                    .media_resource_identity()
                    .is_some_and(|existing| existing.matches(identity))
                    .then_some(segment.proxy_seq)
            })
            .or_else(|| self.published_resource_history.proxy_seq_for(identity))
    }

    fn has_origin_namespace_baseline(&self) -> bool {
        self.origin_epoch_effective_host_id.is_some()
            || self.origin_epoch_sequence_base.is_some()
            || self.origin_seq_highwater.is_some()
            || !self.origin_to_proxy.is_empty()
            || !self.origin_map_to_proxy.is_empty()
    }

    fn apply_origin_epoch_transition_for_manifest(
        &mut self,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
        origin_epoch_handoff: bool,
    ) -> Result<bool, TimelineMapError> {
        let effective_host_changed =
            self.origin_epoch_effective_host_id.is_some_and(|committed_host_id| committed_host_id != effective_host_id);
        if self.has_origin_namespace_baseline() && (origin_epoch_handoff || effective_host_changed) {
            let next_origin_seq =
                manifest.segments.first().map_or(manifest.origin_manifest_sequence, |segment| segment.origin_seq);
            self.start_new_origin_epoch(next_origin_seq, OriginEpochTransitionReason::Handoff)?;
            self.origin_epoch_effective_host_id = Some(effective_host_id);
            return Ok(true);
        }

        if self.should_start_new_origin_epoch_for_rollover(manifest) {
            let next_origin_seq =
                manifest.segments.first().map_or(manifest.origin_manifest_sequence, |segment| segment.origin_seq);
            self.start_new_origin_epoch(next_origin_seq, OriginEpochTransitionReason::Rollover)?;
            self.origin_epoch_effective_host_id = Some(effective_host_id);
            return Ok(true);
        }

        self.origin_epoch_effective_host_id = Some(effective_host_id);
        Ok(false)
    }

    fn apply_forward_jump(&mut self, manifest: &ParsedOriginManifest) -> Result<(), TimelineMapError> {
        let Some(highwater) = self.origin_seq_highwater else {
            return Ok(());
        };
        let next_origin_seq = highwater.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?;
        if manifest.origin_manifest_sequence <= next_origin_seq {
            return Ok(());
        }

        let missing_origin_segments = manifest.origin_manifest_sequence - next_origin_seq;
        info!(
            "HLS forward jump accepted: proxy_session={} origin_sequence={} missing_segments={missing_origin_segments}",
            safe_proxy_session_id(&self.proxy_session_id),
            manifest.origin_manifest_sequence
        );
        Ok(())
    }

    fn should_start_new_origin_epoch_for_rollover(&self, manifest: &ParsedOriginManifest) -> bool {
        let Some(highwater) = self.origin_seq_highwater else {
            return false;
        };
        manifest.segments.last().is_some_and(|segment| segment.origin_seq < highwater)
    }

    fn start_new_origin_epoch(
        &mut self,
        next_origin_seq: u64,
        reason: OriginEpochTransitionReason,
    ) -> Result<(), TimelineMapError> {
        let previous_highwater = self.origin_seq_highwater;
        self.origin_epoch = self.origin_epoch.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?;
        self.origin_epoch_effective_host_id = None;
        self.origin_epoch_sequence_base = None;
        self.origin_seq_highwater = None;
        if let (OriginEpochTransitionReason::Rollover, Some(highwater)) = (reason, previous_highwater) {
            info!(
                "HLS media sequence rollover detected: proxy_session={} previous_highwater={highwater} next_origin_seq={} origin_epoch={}",
                safe_proxy_session_id(&self.proxy_session_id),
                next_origin_seq,
                self.origin_epoch
            );
        }
        Ok(())
    }

    fn map_origin_segment(
        &mut self,
        parsed: &ParsedOriginSegment,
        manifest: &ParsedOriginManifest,
        effective_host_id: u64,
        handoff_discontinuity_before: bool,
    ) -> Result<u64, TimelineMapError> {
        let current_epoch_key = self.origin_segment_key(effective_host_id, parsed.origin_seq);
        if let Some(proxy_seq) = self.origin_to_proxy.get(&current_epoch_key).copied() {
            if let Some(entry) = self.segments.get_mut(&proxy_seq) {
                // Refresh only the concrete fetch reference. Stable identity remains host- and epoch-local.
                entry.origin_fetch_ref = Some(OriginSegmentFetchRef {
                    resolved_origin_url: parsed.resolved_origin_url.clone(),
                    byte_range: parsed.origin_byte_range,
                    valid_until_ms: None,
                });
                entry.encryption = map_segment_encryption(parsed)?;
            }
            return Ok(proxy_seq);
        }

        self.origin_seq_highwater =
            Some(self.origin_seq_highwater.map_or(parsed.origin_seq, |highwater| highwater.max(parsed.origin_seq)));

        let origin_key = current_epoch_key;

        let proxy_seq = self.proxy_next_seq.unwrap_or_default();
        self.proxy_next_seq = Some(proxy_seq.checked_add(1).ok_or(TimelineMapError::ProxySequenceOverflow)?);

        let map_ref = parsed
            .map_ref
            .map(|map_id| {
                let parsed_map = manifest.maps.get(map_id).ok_or(TimelineMapError::UnsupportedMapExtension)?;
                self.map_origin_map(parsed_map)
            })
            .transpose()?;
        let proxy_file_ext = proxy_extension_from_url(&parsed.resolved_origin_url, SEGMENT_EXTENSIONS)
            .ok_or(TimelineMapError::UnsupportedSegmentExtension)?;
        let entry = SegmentEntry {
            origin_key,
            proxy_seq,
            duration_ms: parsed.duration_ms,
            content_type: default_content_type_for_segment_ext(&proxy_file_ext).to_string(),
            cache_key: SegmentCacheKey::new(self.proxy_session_id.clone(), proxy_seq, &proxy_file_ext),
            proxy_file_ext,
            discontinuity_before: parsed.discontinuity_before || handoff_discontinuity_before,
            program_date_time: parsed.program_date_time.clone(),
            daterange_tags_before: parsed.daterange_tags_before.clone(),
            origin_byte_range: parsed.origin_byte_range,
            map_ref,
            encryption: map_segment_encryption(parsed)?,
            // Preserve the concrete fetch target resolved by the parser; do not derive provider/session identity from it.
            origin_fetch_ref: Some(OriginSegmentFetchRef {
                resolved_origin_url: parsed.resolved_origin_url.clone(),
                byte_range: parsed.origin_byte_range,
                valid_until_ms: None,
            }),
            status: SegmentCacheStatus::Discovered,
            last_rendered_at_ms: None,
            access: Arc::new(CacheAccessState::new()),
        };

        self.origin_to_proxy.insert(origin_key, proxy_seq);
        self.segments.insert(proxy_seq, entry);
        Ok(proxy_seq)
    }

    fn origin_segment_key(&mut self, effective_host_id: u64, host_local_sequence: u64) -> OriginSegmentKey {
        let sequence_base = *self.origin_epoch_sequence_base.get_or_insert(host_local_sequence);
        let relative_index = host_local_sequence.saturating_sub(sequence_base);
        let host_local_index = u32::try_from(relative_index).unwrap_or(u32::MAX);
        OriginSegmentKey { origin_epoch: self.origin_epoch, effective_host_id, host_local_sequence, host_local_index }
    }

    fn map_origin_map(&mut self, parsed_map: &ParsedOriginMap) -> Result<ProxyMapId, TimelineMapError> {
        let origin_map_key = OriginMapKey {
            origin_epoch: self.origin_epoch,
            resolved_origin_uri: parsed_map.resolved_origin_uri.clone(),
            byte_range: parsed_map.byte_range,
        };
        if let Some(proxy_map_id) = self.origin_map_to_proxy.get(&origin_map_key).copied() {
            return Ok(proxy_map_id);
        }

        let proxy_map_id = ProxyMapId(self.next_proxy_map_id);
        self.next_proxy_map_id = self.next_proxy_map_id.checked_add(1).ok_or(TimelineMapError::ProxyMapIdOverflow)?;
        let proxy_file_ext = proxy_extension_from_url(&origin_map_key.resolved_origin_uri, MAP_EXTENSIONS)
            .ok_or(TimelineMapError::UnsupportedMapExtension)?;
        self.maps.insert(
            proxy_map_id,
            MapEntry::new(&self.proxy_session_id, proxy_map_id, origin_map_key.clone(), proxy_file_ext),
        );
        self.origin_map_to_proxy.insert(origin_map_key, proxy_map_id);
        Ok(proxy_map_id)
    }
}

fn map_segment_encryption(parsed: &ParsedOriginSegment) -> Result<Option<HlsSegmentEncryption>, TimelineMapError> {
    parsed
        .encryption
        .as_ref()
        .map(|encryption| {
            let resource_id = encryption
                .proxy_resource_id
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or(TimelineMapError::MissingKeyResource)?;
            let resource_extension = encryption
                .proxy_resource_extension
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or(TimelineMapError::MissingKeyResource)?;
            Ok(HlsSegmentEncryption {
                resource_id: TransientResourceId(resource_id),
                resource_extension,
                // The proxy rewrites MEDIA-SEQUENCE into its own monotone timeline. An
                // omitted origin IV would therefore be re-derived from the wrong sequence
                // by clients unless the origin's host-local media sequence is materialized.
                iv: Some(encryption.iv.clone().unwrap_or_else(|| format!("0x{:032x}", parsed.origin_seq))),
                key_format: encryption.key_format.clone(),
                key_format_versions: encryption.key_format_versions.clone(),
            })
        })
        .transpose()
}

fn proxy_extension_from_url(url: &str, allowed_extensions: &[&str]) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let file_name = parsed.path_segments()?.next_back()?;
    let extension = file_name.rsplit_once('.')?.1.to_ascii_lowercase();
    allowed_extensions.contains(&extension.as_str()).then_some(extension)
}

#[cfg(test)]
fn inferred_effective_origin_host_id(manifest: &ParsedOriginManifest) -> u64 {
    manifest
        .segments
        .first()
        .and_then(|segment| Url::parse(&segment.resolved_origin_url).ok())
        .and_then(|url| url.host_str().map(effective_origin_host_id))
        .or_else(|| {
            manifest
                .maps
                .first()
                .and_then(|map| Url::parse(&map.resolved_origin_uri).ok())
                .and_then(|url| url.host_str().map(effective_origin_host_id))
        })
        .unwrap_or(UNKNOWN_EFFECTIVE_ORIGIN_HOST_ID)
}

#[cfg(test)]
mod tests {
    use super::{
        effective_origin_host_id, HlsOriginHandoffPreviewError, OriginSegmentKey, SegmentCacheStatus, TimelineMapError,
    };
    use crate::{
        api::model::{HlsSession, HlsSessionKey, MapCacheStatus, ProxyMapId, SegmentFetchPriority},
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    };

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn session() -> HlsSession { HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0) }

    fn origin_key(epoch: u64, host: &str, sequence: u64, index: u32) -> OriginSegmentKey {
        OriginSegmentKey {
            origin_epoch: epoch,
            effective_host_id: effective_origin_host_id(host),
            host_local_sequence: sequence,
            host_local_index: index,
        }
    }

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        normal_manifest_at(body, BASE_URL)
    }

    fn normal_manifest_at(
        body: &str,
        final_manifest_url: &str,
    ) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, final_manifest_url) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    fn publish_ready_manifest(session: &mut HlsSession, rendered_at_ms: u64) -> String {
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms };
        }
        session.render_and_store_manifest(rendered_at_ms).expect("ready manifest should render").body
    }

    #[test]
    fn stale_resource_prefix_is_trimmed_before_new_same_host_media() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:471\n\
             #EXTINF:4,\n471.ts\n#EXTINF:4,\n472.ts\n#EXTINF:4,\n473.ts\n\
             #EXTINF:4,\n474.ts\n#EXTINF:4,\n475.ts\n#EXTINF:4,\n476.ts\n",
        );
        let stale = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:476\n\
             #EXTINF:4,\n474.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n",
        );

        session.apply_origin_manifest(&first).expect("baseline maps");
        publish_ready_manifest(&mut session, 1);
        session.apply_origin_manifest(&stale).expect("published stale prefix is safely trimmed");

        assert_eq!(session.proxy_next_seq, Some(8));
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(6));
        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(7));
        assert!(session.segments.get(&6).expect("first new resource").discontinuity_before);
        assert_eq!(
            session
                .segments
                .values()
                .filter_map(super::SegmentEntry::media_resource_identity)
                .filter(|identity| {
                    identity.matches(super::HlsMediaResourceIdentity::from_url(
                        "http://origin.example.com/live/final/474.ts",
                        None,
                    ))
                })
                .count(),
            1
        );
    }

    #[test]
    fn explicit_handoff_trims_all_overlap_and_discontinues_first_genuine_resource() {
        let mut session = session();
        let first = normal_manifest_at(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:476\n\
             #EXTINF:4,\n476.ts\n#EXTINF:4,\n477.ts\n#EXTINF:4,\n478.ts\n\
             #EXTINF:4,\n479.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let handoff = normal_manifest_at(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:476\n\
             #EXTINF:4,\n476.ts\n#EXTINF:4,\n477.ts\n#EXTINF:4,\n481.ts\n\
             #EXTINF:4,\n482.ts\n#EXTINF:4,\n483.ts\n#EXTINF:4,\n484.ts\n\
             #EXTINF:4,\n485.ts\n#EXTINF:4,\n486.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let host_a = effective_origin_host_id("cdn-a.example.net");
        let host_b = effective_origin_host_id("cdn-b.example.net");

        session.apply_origin_manifest_for_host(&first, host_a).expect("baseline maps");
        publish_ready_manifest(&mut session, 1);
        let preview = session.preview_origin_handoff_manifest(&handoff, host_b, 0).expect("handoff previews");

        assert_eq!(preview.segments.len(), 5);
        assert_eq!(preview.segments.first().map(|segment| segment.proxy_seq), Some(6));
        assert!(preview.segments.first().is_some_and(|segment| segment.discontinuity_before));
        session
            .apply_origin_handoff_manifest_if_preview_matches(&handoff, host_b, 0, &preview)
            .expect("matching handoff commits");
        assert_eq!(session.origin_epoch, 1);
        assert_eq!(session.proxy_next_seq, Some(11));
        let rendered = publish_ready_manifest(&mut session, 2);
        assert!(rendered.contains("#EXT-X-MEDIA-SEQUENCE:5\n"));
        assert!(rendered.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,"));
        let visible = session.last_rendered_manifest.as_ref().expect("handoff manifest stored");
        let identities = visible
            .segment_proxy_seqs
            .iter()
            .filter_map(|proxy_seq| session.segments.get(proxy_seq)?.media_resource_identity())
            .collect::<Vec<_>>();
        assert!(identities.iter().enumerate().all(|(index, identity)| {
            identities[..index].iter().all(|previous| !previous.matches(*identity))
        }));
    }

    #[test]
    fn published_resource_history_survives_segment_gc() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
        );
        let replay_then_new = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n490.ts\n#EXTINF:4,\n491.ts\n#EXTINF:4,\n492.ts\n",
        );

        session.apply_origin_manifest(&first).expect("baseline maps");
        publish_ready_manifest(&mut session, 1);
        session.segments.clear();
        session.origin_to_proxy.clear();
        session.apply_origin_manifest(&replay_then_new).expect("GC-independent history trims replay");

        assert_eq!(session.proxy_next_seq, Some(6));
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(3));
        assert!(session.segments.get(&3).expect("new head").discontinuity_before);
        assert_eq!(session.published_resource_history.len(), 3);
    }

    #[test]
    fn replay_after_new_resource_is_rejected_without_partial_timeline_mutation() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
        );
        let contradictory = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
             #EXTINF:4,\n490.ts\n#EXTINF:4,\n480.ts\n#EXTINF:4,\n491.ts\n",
        );

        session.apply_origin_manifest(&first).expect("baseline maps");
        publish_ready_manifest(&mut session, 1);
        let proxy_next_seq = session.proxy_next_seq;

        assert!(matches!(
            session.apply_origin_manifest(&contradictory),
            Err(TimelineMapError::PublishedResourceReplay {
                existing_proxy_seq: 0,
                candidate_origin_seq: 491,
                ..
            })
        ));
        assert_eq!(session.proxy_next_seq, proxy_next_seq);
        assert_eq!(session.segments.len(), 3);
    }

    #[test]
    fn parsed_target_duration_is_stored_for_account_overlap_timing() {
        let mut session = session();
        let manifest =
            normal_manifest("#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:12.0,\n10.ts\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.target_duration, Some(12));
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
        assert_eq!(session.account_overlap_timing().hard_active_window_ms, 12_000);
        assert_eq!(session.account_overlap_timing().soft_active_window_ms, 24_000);
    }

    #[test]
    fn capacity_recovery_blocks_only_at_first_dependency_ready_boundary() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.0,\n0.m4s\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n",
        );
        session.apply_origin_manifest(&manifest).expect("capacity timeline maps");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        session.segments.get_mut(&1).expect("deferred segment").status = SegmentCacheStatus::CapacityDeferred {
            priority: SegmentFetchPriority::Prefetch,
            deferred_at_ms: 2,
        };

        let missing_map = session.ready_timeline_snapshot(0, 3);
        assert!(!session.capacity_recovery_blocks_ready_timeline(&missing_map));

        session.maps.get_mut(&ProxyMapId(0)).expect("shared map").status =
            MapCacheStatus::Ready { content_length: 1, ready_at_ms: 3 };
        let deferred_boundary = session.ready_timeline_snapshot(0, 3);
        assert!(session.capacity_recovery_blocks_ready_timeline(&deferred_boundary));

        session.segments.get_mut(&0).expect("unrecoverable head").status =
            SegmentCacheStatus::FailedPermanent { failed_at_ms: 4, status: None };
        let unrecoverable_boundary = session.ready_timeline_snapshot(0, 4);
        assert!(!session.capacity_recovery_blocks_ready_timeline(&unrecoverable_boundary));
    }

    #[test]
    fn ready_timeline_snapshot_stops_before_the_first_sequence_gap() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n\
             #EXTINF:4.0,\n20.ts\n#EXTINF:4.0,\n21.ts\n#EXTINF:4.0,\n22.ts\n",
        );
        session.apply_origin_manifest(&manifest).expect("timeline maps");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        session.segments.remove(&1);

        let snapshot = session.ready_timeline_snapshot(0, 2);

        assert_eq!(snapshot.units.len(), 1);
        assert_eq!(snapshot.units[0].proxy_seq, 0);
        assert_eq!(snapshot.units[0].start_ms, 0);
        assert!(session.ready_timeline_snapshot(1, 2).units.is_empty());
    }

    #[test]
    fn implicit_aes_iv_is_materialized_from_host_local_sequence_before_proxy_rewrite() {
        let mut manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:77\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"origin-a.key\"\n#EXTINF:4,\n77.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"origin-b.key\",IV=0x00000000000000000000000000000001\n#EXTINF:4,\n78.ts\n\
             #EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\n79.ts\n",
        );
        for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
            encryption.proxy_resource_id = Some("opaque-key".to_string());
            encryption.proxy_resource_extension = Some("key".to_string());
        }
        let mut session = session();

        session.apply_origin_manifest(&manifest).expect("encrypted timeline maps");

        let implicit = session.segments.get(&0).expect("implicit-IV segment");
        assert_eq!(implicit.origin_key.host_local_sequence, 77);
        assert_eq!(
            implicit.encryption.as_ref().and_then(|state| state.iv.as_deref()),
            Some("0x0000000000000000000000000000004d")
        );
        let explicit = session.segments.get(&1).expect("explicit-IV segment");
        assert_eq!(explicit.origin_key.host_local_sequence, 78);
        assert_eq!(
            explicit.encryption.as_ref().and_then(|state| state.iv.as_deref()),
            Some("0x00000000000000000000000000000001")
        );
        assert!(session.segments.get(&2).expect("METHOD=NONE segment").encryption.is_none());
    }

    #[test]
    fn origin_rollover_maps_to_monotone_proxy_sequence() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:322\n#EXTINF:4.0,\n322.ts\n#EXTINF:4.0,\n323.ts\n#EXTINF:4.0,\n324.ts\n",
        );
        let second = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(session.origin_to_proxy.get(&origin_key(1, "origin.example.com", 0, 0)), Some(&3));
        assert!(session.segments.get(&3).expect("rollover segment").discontinuity_before);
    }

    #[test]
    fn rollover_with_only_previously_seen_resources_does_not_advance_timeline() {
        let mut session = session();
        let old_low = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        );
        let high = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:190\n#EXTINF:4.0,\n190.ts\n#EXTINF:4.0,\n191.ts\n#EXTINF:4.0,\n192.ts\n",
        );
        let rollover = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        );

        session.apply_origin_manifest(&old_low).expect("old low manifest should map");
        session.apply_origin_manifest(&high).expect("high manifest should map");
        session.apply_origin_manifest(&rollover).expect("rollover manifest should map");

        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.origin_seq_highwater, Some(192));
        assert_eq!(session.origin_to_proxy.get(&origin_key(0, "origin.example.com", 0, 0)), Some(&0));
        assert_eq!(session.origin_to_proxy.get(&origin_key(0, "origin.example.com", 190, 190)), Some(&3));
        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(session.proxy_next_seq, Some(6));
    }

    #[test]
    fn same_host_rollover_after_timeline_gc_starts_exactly_one_new_epoch() {
        let mut session = session();
        let first =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1000\n#EXTINF:4.0,\n1000.ts\n#EXTINF:4.0,\n1001.ts\n");
        let rollover = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n");
        let sliding = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        let previous_epoch = session.origin_epoch;
        session.segments.clear();
        session.origin_to_proxy.clear();

        assert_eq!(session.origin_seq_highwater, Some(1_001));
        assert_eq!(session.origin_epoch_sequence_base, Some(1_000));
        session.apply_origin_manifest(&rollover).expect("rollover after GC should map in a new epoch");

        let rollover_epoch = previous_epoch.saturating_add(1);
        assert_eq!(session.origin_epoch, rollover_epoch);
        assert_eq!(session.origin_seq_highwater, Some(1));
        assert_eq!(session.origin_epoch_sequence_base, Some(0));
        assert_eq!(session.origin_to_proxy.get(&origin_key(rollover_epoch, "origin.example.com", 0, 0)), Some(&2));
        assert!(session.segments.get(&2).expect("rollover head").discontinuity_before);

        session.apply_origin_manifest(&sliding).expect("same-host sliding refresh should remain in the new epoch");

        assert_eq!(session.origin_epoch, rollover_epoch);
        assert_eq!(session.origin_seq_highwater, Some(2));
        assert_eq!(session.origin_to_proxy.get(&origin_key(rollover_epoch, "origin.example.com", 2, 2)), Some(&4));
        assert!(!session.segments.get(&4).expect("sliding tail").discontinuity_before);
    }

    #[test]
    fn known_origin_key_is_not_remapped() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.apply_origin_manifest(&manifest).expect("same manifest should be ignored");

        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.proxy_next_seq, Some(1));
    }

    #[test]
    fn overlapping_old_origin_sequences_do_not_trigger_rollover() {
        let mut session = session();
        let first = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n",
        );
        let overlap = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:101\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n#EXTINF:4.0,\n103.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&overlap).expect("overlapping manifest should map");

        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.origin_seq_highwater, Some(103));
        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(1));
        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(3));
    }

    #[test]
    fn forward_jump_preserves_compact_proxy_sequence() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n");
        let jump = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:107\n#EXTINF:4.0,\n107.ts\n#EXTINF:4.0,\n108.ts\n#EXTINF:4.0,\n109.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&jump).expect("jump manifest should map");

        assert_eq!(session.origin_to_proxy.get(&origin_key(0, "origin.example.com", 107, 7)), Some(&1));
        assert_eq!(session.proxy_next_seq, Some(4));
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(1));
        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(3));
    }

    #[test]
    fn host_handoff_with_only_published_overlap_does_not_advance() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n");
        let second = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.mark_pending_origin_epoch_handoff_discontinuity(0);
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.origin_to_proxy.get(&origin_key(0, "origin.example.com", 100, 0)), Some(&0));
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.segments.len(), 1);
        assert_eq!(session.proxy_next_seq, Some(1));
    }

    #[test]
    fn handoff_rejects_published_overlap_after_a_new_resource() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n");
        let second = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:99\n#EXTINF:4.0,\n99.ts\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n",
        );

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.mark_pending_origin_epoch_handoff_discontinuity(0);
        assert!(matches!(
            session.apply_origin_manifest(&second),
            Err(TimelineMapError::PublishedResourceReplay {
                existing_proxy_seq: 0,
                candidate_origin_seq: 100,
                ..
            })
        ));
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.proxy_next_seq, Some(2));
    }

    #[test]
    fn cross_host_candidate_with_only_identical_resources_does_not_advance() {
        let mut session = session();
        let first = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let second = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let host_a = effective_origin_host_id("cdn-a.example.net");
        let host_b = effective_origin_host_id("cdn-b.example.net");

        session.apply_origin_manifest_for_host(&first, host_a).expect("first host should map");
        session.apply_origin_manifest_for_host(&second, host_b).expect("second host should map in a new epoch");

        assert_ne!(host_a, host_b);
        assert_eq!(session.origin_epoch, 0);
        assert_eq!(session.origin_to_proxy.get(&origin_key(0, "cdn-a.example.net", 100, 0)), Some(&0));
        assert_eq!(session.origin_to_proxy.get(&origin_key(1, "cdn-b.example.net", 100, 0)), None);
        assert_eq!(session.proxy_next_seq, Some(1));
    }

    #[test]
    fn explicit_handoff_always_starts_new_epoch_even_for_lower_disjoint_sequence_range() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n");
        let second = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n");
        let host_id = effective_origin_host_id("origin.example.com");

        session.apply_origin_manifest_for_host(&first, host_id).expect("first manifest should map");
        let preview =
            session.preview_origin_handoff_manifest(&second, host_id, 0).expect("explicit handoff should preview");
        session
            .apply_origin_handoff_manifest_if_preview_matches(&second, host_id, 0, &preview)
            .expect("explicit handoff should map in a new epoch");

        assert_eq!(session.origin_epoch, 1);
        assert_eq!(session.origin_seq_highwater, Some(1));
        assert_eq!(session.origin_to_proxy.get(&origin_key(1, "origin.example.com", 0, 0)), Some(&2));
        assert!(session.segments.get(&2).expect("handoff head").discontinuity_before);
    }

    #[test]
    fn cross_host_handoff_after_timeline_gc_starts_exactly_one_new_epoch() {
        let mut session = session();
        let first = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1000\n#EXTINF:4.0,\n1000.ts\n#EXTINF:4.0,\n1001.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let handoff = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let sliding = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:6\n#EXTINF:4.0,\n6.ts\n#EXTINF:4.0,\n7.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let host_a = effective_origin_host_id("cdn-a.example.net");
        let host_b = effective_origin_host_id("cdn-b.example.net");

        session.apply_origin_manifest_for_host(&first, host_a).expect("first host should map");
        let previous_epoch = session.origin_epoch;
        session.segments.clear();
        session.origin_to_proxy.clear();

        assert_eq!(session.origin_epoch_effective_host_id, Some(host_a));
        assert_eq!(session.origin_seq_highwater, Some(1_001));
        assert_eq!(session.origin_epoch_sequence_base, Some(1_000));
        let preview = session
            .preview_origin_handoff_manifest(&handoff, host_b, 0)
            .expect("cross-host handoff after GC should preview in a new epoch");
        session
            .apply_origin_handoff_manifest_if_preview_matches(&handoff, host_b, 0, &preview)
            .expect("matching cross-host handoff should commit");

        let handoff_epoch = previous_epoch.saturating_add(1);
        assert_eq!(session.origin_epoch, handoff_epoch);
        assert_eq!(session.origin_epoch_effective_host_id, Some(host_b));
        assert_eq!(session.origin_seq_highwater, Some(6));
        assert_eq!(session.origin_epoch_sequence_base, Some(5));
        assert_eq!(session.origin_to_proxy.get(&origin_key(handoff_epoch, "cdn-b.example.net", 5, 0)), Some(&2));
        assert!(session.segments.get(&2).expect("handoff head").discontinuity_before);

        session
            .apply_origin_manifest_for_host(&sliding, host_b)
            .expect("same-host sliding refresh should remain in the handoff epoch");

        assert_eq!(session.origin_epoch, handoff_epoch);
        assert_eq!(session.origin_seq_highwater, Some(7));
        assert_eq!(session.origin_to_proxy.get(&origin_key(handoff_epoch, "cdn-b.example.net", 7, 2)), Some(&4));
        assert!(!session.segments.get(&4).expect("sliding tail").discontinuity_before);
    }

    #[test]
    fn handoff_preview_is_mutation_free_and_matches_later_commit_entries() {
        let mut session = session();
        let first = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let handoff = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n500.m4s\n#EXTINF:4.0,\n501.m4s\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let host_a = effective_origin_host_id("cdn-a.example.net");
        let host_b = effective_origin_host_id("cdn-b.example.net");
        session.apply_origin_manifest_for_host(&first, host_a).expect("first manifest should map");
        let old_epoch = session.origin_epoch;
        let old_segment_count = session.segments.len();
        let old_map_count = session.maps.len();
        let old_next_proxy_seq = session.proxy_next_seq;

        let preview = session.preview_origin_handoff_manifest(&handoff, host_b, 7).expect("handoff preview should map");

        assert_eq!(session.origin_epoch, old_epoch);
        assert_eq!(session.segments.len(), old_segment_count);
        assert_eq!(session.maps.len(), old_map_count);
        assert_eq!(session.proxy_next_seq, old_next_proxy_seq);
        assert_eq!(preview.origin_epoch, old_epoch + 1);
        assert_eq!(preview.segments.len(), 2);
        assert_eq!(preview.maps.len(), 1);
        assert!(preview.segments[0].discontinuity_before);
        assert_eq!(preview.segments[0].map_ref, Some(preview.maps[0].proxy_map_id));

        session
            .apply_origin_handoff_manifest_if_preview_matches(&handoff, host_b, 7, &preview)
            .expect("matching handoff preview should commit");

        let committed_segments = preview
            .segments
            .iter()
            .map(|planned| session.segments.get(&planned.proxy_seq).cloned().expect("committed segment"))
            .collect::<Vec<_>>();
        let committed_maps = preview
            .maps
            .iter()
            .map(|planned| session.maps.get(&planned.proxy_map_id).cloned().expect("committed map"))
            .collect::<Vec<_>>();
        assert_eq!(committed_segments, preview.segments);
        assert_eq!(committed_maps, preview.maps);
    }

    #[test]
    fn handoff_commit_rejects_stale_preview_without_partial_mutation() {
        let mut session = session();
        let first = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let progressed = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n#EXTINF:4.0,\n11.ts\n",
            "https://cdn-a.example.net/live/index.m3u8",
        );
        let handoff = normal_manifest_at(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:500\n#EXTINF:4.0,\n500.ts\n",
            "https://cdn-b.example.net/live/index.m3u8",
        );
        let host_a = effective_origin_host_id("cdn-a.example.net");
        let host_b = effective_origin_host_id("cdn-b.example.net");
        session.apply_origin_manifest_for_host(&first, host_a).expect("first manifest should map");
        let stale_preview =
            session.preview_origin_handoff_manifest(&handoff, host_b, 0).expect("handoff preview should map");
        session.apply_origin_manifest_for_host(&progressed, host_a).expect("same-host progress should commit");
        let epoch_before_rejected_commit = session.origin_epoch;
        let segments_before_rejected_commit = session.segments.clone();
        let next_proxy_seq_before_rejected_commit = session.proxy_next_seq;

        let result = session.apply_origin_handoff_manifest_if_preview_matches(&handoff, host_b, 0, &stale_preview);

        assert_eq!(result, Err(HlsOriginHandoffPreviewError::PreviewInconsistent));
        assert_eq!(session.origin_epoch, epoch_before_rejected_commit);
        assert_eq!(session.segments, segments_before_rejected_commit);
        assert_eq!(session.proxy_next_seq, next_proxy_seq_before_rejected_commit);
    }

    #[test]
    fn effective_host_id_is_stable_and_normalized_without_retaining_host_text() {
        let normalized = effective_origin_host_id("CDN.Example.NET.");

        assert_eq!(normalized, effective_origin_host_id("cdn.example.net"));
        assert_ne!(normalized, effective_origin_host_id("other.example.net"));
        assert_ne!(normalized, 0);
    }

    #[test]
    fn mapping_error_does_not_commit_partial_session_state() {
        let mut session = session();
        let first = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\n10.ts\n");
        let invalid =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:11\n#EXTINF:4.0,\n11.ts\n#EXTINF:4.0,\n12.webm\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        let previous_proxy_next_seq = session.proxy_next_seq;
        let previous_highwater = session.origin_seq_highwater;
        let previous_head = session.publishable_origin_head_proxy_seq;
        let previous_tail = session.publishable_origin_tail_proxy_seq;
        let previous_segments = session.segments.clone();
        let previous_origin_to_proxy = session.origin_to_proxy.clone();

        assert_eq!(session.apply_origin_manifest(&invalid), Err(TimelineMapError::UnsupportedSegmentExtension));

        assert_eq!(session.proxy_next_seq, previous_proxy_next_seq);
        assert_eq!(session.origin_seq_highwater, previous_highwater);
        assert_eq!(session.publishable_origin_head_proxy_seq, previous_head);
        assert_eq!(session.publishable_origin_tail_proxy_seq, previous_tail);
        assert_eq!(session.segments, previous_segments);
        assert_eq!(session.origin_to_proxy, previous_origin_to_proxy);
    }

    #[test]
    fn unsupported_segment_extension_fails_mapping() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXTINF:4.0,\nseg.webm\n");

        assert_eq!(session.apply_origin_manifest(&manifest), Err(TimelineMapError::UnsupportedSegmentExtension));
    }

    #[test]
    fn mapped_maps_start_as_discovered_placeholders() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg.m4s\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let map = session.maps.values().next().expect("map placeholder");
        assert_eq!(map.status, MapCacheStatus::Discovered);
        assert_eq!(map.proxy_map_id, ProxyMapId(0));
        assert_eq!(map.proxy_file_ext, "mp4");
        assert_eq!(map.origin_key.origin_epoch, 0);
        assert_eq!(map.origin_key.byte_range, None);
        assert_eq!(map.cache_key.proxy_map_id(), ProxyMapId(0));
        let fetch_ref = map.origin_fetch_ref.as_ref().expect("map fetch ref");
        assert_eq!(fetch_ref.byte_range, None);
        assert!(format!("{fetch_ref:?}").contains("<redacted>"));
        assert!(!format!("{fetch_ref:?}").contains("init.mp4"));
    }

    #[test]
    fn map_fetch_ref_preserves_final_manifest_host_for_relative_map_uri() {
        let mut session = session();
        let manifest = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg.m4s\n",
            "https://cdn.example.net/live/redirected/playlist.m3u8",
        );
        let crate::processing::parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) = manifest
        else {
            panic!("manifest should parse as normal timeline");
        };

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let map = session.maps.get(&ProxyMapId(0)).expect("map placeholder");
        assert_eq!(map.origin_key.resolved_origin_uri, "https://cdn.example.net/live/redirected/init.mp4");
        assert_eq!(
            map.origin_fetch_ref.as_ref().expect("map fetch ref").resolved_origin_url,
            "https://cdn.example.net/live/redirected/init.mp4"
        );
    }

    #[test]
    fn manifest_mapping_sets_origin_fetch_ref() {
        let mut session = session();
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\nseg.ts\n");

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let segment = session.segments.get(&0).expect("segment should be mapped");
        let fetch_ref = segment.origin_fetch_ref.as_ref().expect("fetch ref should be set");
        assert!(format!("{fetch_ref:?}").contains("<redacted>"));
        assert!(!format!("{fetch_ref:?}").contains("seg.ts"));
    }

    #[test]
    fn segment_fetch_ref_preserves_final_manifest_host_for_relative_segment_uri() {
        let mut session = session();
        let manifest = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nmedia/seg001.ts\n",
            "https://cdn.example.net/live/redirected/playlist.m3u8",
        );
        let crate::processing::parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(manifest) = manifest
        else {
            panic!("manifest should parse as normal timeline");
        };

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        let segment = session.segments.get(&0).expect("segment should be mapped");
        assert_eq!(segment.origin_key, origin_key(0, "cdn.example.net", 10, 0));
        assert_eq!(
            segment.origin_fetch_ref.as_ref().expect("fetch ref").resolved_origin_url,
            "https://cdn.example.net/live/redirected/media/seg001.ts"
        );
    }

    #[test]
    fn cold_start_prefetch_prioritizes_visible_window_then_known_tail() {
        let mut session = session();
        session.initial_prefetch_gap_segments = 3;
        session.configure_segment_prefetch_queue(6);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n100.ts\n#EXTINF:4.0,\n101.ts\n#EXTINF:4.0,\n102.ts\n#EXTINF:4.0,\n103.ts\n#EXTINF:4.0,\n104.ts\n#EXTINF:4.0,\n105.ts\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.queue_manifest_prefetch_candidates(10);

        assert!(matches!(
            session.segments.get(&0).expect("0").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, .. }
        ));
        assert!(matches!(
            session.segments.get(&2).expect("2").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, .. }
        ));
        assert!(matches!(
            session.segments.get(&3).expect("3").status,
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, .. }
        ));
        assert_eq!(session.segment_prefetch_queue.prefetch_len(), 3);
    }

    #[test]
    fn same_map_uri_with_different_byterange_uses_distinct_proxy_map_ids() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@0\"\n#EXTINF:4.0,\n1.m4s\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@100\"\n#EXTINF:4.0,\n2.m4s\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.maps.len(), 2);
        assert_eq!(session.segments.get(&0).expect("first segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second segment").map_ref, Some(ProxyMapId(1)));
    }

    #[test]
    fn same_map_uri_in_same_epoch_reuses_proxy_map_id() {
        let mut session = session();
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n2.m4s\n",
        );

        session.apply_origin_manifest(&manifest).expect("manifest should map");

        assert_eq!(session.maps.len(), 1);
        assert_eq!(session.segments.get(&0).expect("first segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second segment").map_ref, Some(ProxyMapId(0)));
    }

    #[test]
    fn same_map_uri_after_rollover_uses_distinct_proxy_map_id() {
        let mut session = session();
        let first =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:322\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n322.m4s\n");
        let second =
            normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n0.m4s\n");

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.maps.len(), 2);
        assert_eq!(session.segments.get(&0).expect("first epoch segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second epoch segment").map_ref, Some(ProxyMapId(1)));
        assert_eq!(session.maps.get(&ProxyMapId(1)).expect("second map").origin_key.origin_epoch, 1);
    }

    #[test]
    fn same_relative_map_uri_on_different_final_hosts_uses_distinct_proxy_map_ids() {
        let mut session = session();
        let first = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n0.m4s\n",
            "https://cdn-a.example.net/live/playlist.m3u8",
        );
        let second = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n",
            "https://cdn-b.example.net/live/playlist.m3u8",
        );
        let crate::processing::parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(first) = first else {
            panic!("first manifest should parse as normal timeline");
        };
        let crate::processing::parser::hls::origin_manifest::OriginManifestParseOutcome::Normal(second) = second else {
            panic!("second manifest should parse as normal timeline");
        };

        session.apply_origin_manifest(&first).expect("first manifest should map");
        session.apply_origin_manifest(&second).expect("second manifest should map");

        assert_eq!(session.maps.len(), 2);
        assert_eq!(
            session.maps.get(&ProxyMapId(0)).expect("first map").origin_key.resolved_origin_uri,
            "https://cdn-a.example.net/live/init.mp4"
        );
        assert_eq!(
            session.maps.get(&ProxyMapId(1)).expect("second map").origin_key.resolved_origin_uri,
            "https://cdn-b.example.net/live/init.mp4"
        );
        assert_eq!(session.segments.get(&0).expect("first segment").map_ref, Some(ProxyMapId(0)));
        assert_eq!(session.segments.get(&1).expect("second segment").map_ref, Some(ProxyMapId(1)));
    }
}
