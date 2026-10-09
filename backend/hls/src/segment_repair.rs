#![allow(clippy::large_futures)]

use super::{
    is_hls_provisioning_segment, media_reserve::HlsLeaseManifestSnapshot, safe_hls_access_lease_id,
    safe_proxy_session_id, segment_watchdog::HlsCorruptSegmentWatchdogManager, CachedSegmentMetadata, HlsAccessLeaseId,
    HlsAccessLeaseStore, HlsCacheObjectKey, HlsLogIdentity, HlsSegmentCache, HlsSession, ProxySessionId,
    SegmentCacheKey, SegmentCacheStatus, StagedCacheObject,
};
use arc_swap::ArcSwap;
use shared::model::HlsSegmentRepairMode;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tuliprox_core::model::HlsSegmentRepairConfig;

const COMMAND_VERSION: u32 = 1;

const REPAIR_METADATA_MAX_ENTRIES: usize = 4_096;

const REPAIR_OBJECT_METADATA_MAX_ENTRIES: usize = 8_192;

const REPAIR_CANDIDATE_MAX_ENTRIES: usize = 8_192;

#[derive(Clone)]
pub struct HlsRepairPrewarmGuard {
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    lease_id: HlsAccessLeaseId,
    proxy_session_id: ProxySessionId,
    issued_at_ms: u64,
    snapshot_generation: u64,
}

struct HlsSelectedRepairCandidate<'a> {
    mode: HlsSegmentRepairMode,
    runtime: Arc<HlsSegmentRepairRuntime>,
    candidate: HlsRepairWindowCandidateKey,
    prewarm_guard: Option<&'a HlsRepairPrewarmGuard>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct HlsRepairWindowCandidateKey {
    proxy_session_id: ProxySessionId,
    hls_access_lease_id: HlsAccessLeaseId,
    activation_generation: u64,
    /// Proxy-rendered object identity. Deliberately excludes the concrete origin fetch URI.
    object_id: HlsRepairRenderedObjectId,
    file_ext: String,
}

#[derive(Debug, Clone)]
struct HlsRepairWindow {
    mode: HlsSegmentRepairMode,
    activation_generation: u64,
    remaining_segments: u8,
    seen_candidates: HashSet<HlsRepairWindowCandidateKey>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct RepairIdentity {
    raw_sha256: String,
    repair_mode: HlsSegmentRepairMode,
    command_version: u32,
    ffmpeg_version: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct HlsRepairObjectMetadataKey {
    proxy_session_id: ProxySessionId,
    /// Proxy-rendered object identity. Deliberately excludes the concrete origin fetch URI.
    rendered_object_id: HlsRepairRenderedObjectId,
    file_ext: String,
    repair_mode: HlsSegmentRepairMode,
    command_version: u32,
    ffmpeg_version: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct HlsSegmentRepairDecision {
    codec: RepairVideoCodec,
    required_level: HlsSegmentRepairMode,
    trigger_source: HlsSegmentRepairTriggerSource,
    common_low_trigger: bool,
    codec_medium_trigger: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct SegmentRepairMetadata {
    status: RepairStatus,
    raw_size: u64,
    final_size: u64,
    validation_reason: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct HlsRepairObjectMetadata {
    committed_sha256: String,
    raw_sha256: Option<String>,
    status: RepairStatus,
    raw_size: u64,
    final_size: u64,
    validation_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct HlsPostProcessingDeadline {
    started: Instant,
    timeout: Duration,
}

impl HlsPostProcessingDeadline {
    fn new(timeout_ms: u64) -> Self {
        Self { started: Instant::now(), timeout: Duration::from_millis(timeout_ms.max(100)) }
    }

    pub(super) fn remaining(&self) -> Option<Duration> { self.timeout.checked_sub(self.started.elapsed()) }
}

#[derive(Debug, Default)]
struct HlsRepairWindowRegistry {
    windows: HashMap<HlsAccessLeaseId, HlsRepairWindow>,
    generations: HashMap<HlsAccessLeaseId, u64>,
    checked_candidates: HashSet<HlsRepairWindowCandidateKey>,
    checked_candidate_order: VecDeque<HlsRepairWindowCandidateKey>,
}

#[derive(Debug)]
pub struct HlsSegmentRepairManager {
    runtime: ArcSwap<HlsSegmentRepairRuntime>,
    watchdog: HlsCorruptSegmentWatchdogManager,
    windows: RwLock<HlsRepairWindowRegistry>,
    metadata: RwLock<HashMap<RepairIdentity, SegmentRepairMetadata>>,
    metadata_order: Mutex<VecDeque<RepairIdentity>>,
    object_metadata: RwLock<HashMap<HlsRepairObjectMetadataKey, HlsRepairObjectMetadata>>,
    object_metadata_order: Mutex<VecDeque<HlsRepairObjectMetadataKey>>,
    locks: Mutex<HashMap<RepairIdentity, Arc<Mutex<()>>>>,
}

#[derive(Debug, Clone)]
struct HlsSegmentRepairRuntime {
    config: HlsSegmentRepairConfig,
    semaphore: Option<Arc<Semaphore>>,
    watchdog_semaphore: Arc<Semaphore>,
}

#[derive(Debug, Clone, Default)]
struct SegmentProbe {
    duration_ms: Option<u64>,
    size: u64,
    stream_count: usize,
    streams: Vec<SegmentProbeStream>,
    primary_video_codec: Option<String>,
    primary_audio_codec: Option<String>,
    primary_video_start_time_ms: Option<i64>,
    primary_audio_start_time_ms: Option<i64>,
    primary_video_extradata_size: Option<u64>,
    warnings: WarningCounters,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SegmentProbeStream {
    index: usize,
    stream_type: SegmentProbeStreamType,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u32>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RepairRemuxStreamSelection {
    mapped_streams: Vec<usize>,
    dropped_streams: Vec<RepairRemuxDroppedStream>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RepairRemuxDroppedStream {
    index: usize,
    reason: &'static str,
}

#[cfg(test)]
mod tests;

mod diagnostics;
mod manager;
mod probe;
mod processing;
mod registry;
mod window;
pub use diagnostics::{parse_ffmpeg_warnings, WarningCounters};
use diagnostics::{HlsSegmentRepairTriggerSource, SegmentProbeStreamType};
pub(super) use probe::{ffmpeg_identity_version, sha256_file};
pub(super) use processing::run_command_with_deadline;
pub use registry::HlsSegmentRepairStats;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use window::{
    ready_segment_repair_prewarm_candidates, HlsRepairRenderedObjectId, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource,
};
use window::{RepairStatus, RepairVideoCodec};
