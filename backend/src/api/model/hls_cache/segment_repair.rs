#![allow(clippy::large_futures)]

use super::{
    safe_hls_access_lease_id, safe_proxy_session_id, segment_watchdog::HlsCorruptSegmentWatchdogManager,
    CachedSegmentMetadata, HlsAccessLeaseId, HlsCacheObjectKey, HlsSegmentCache, ProxySessionId, StagedCacheObject,
};
use crate::model::{HlsCorruptSegmentWatchdogConfig, HlsSegmentRepairConfig, HlsSegmentRepairMode};
use arc_swap::ArcSwap;
use log::{debug, warn};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    fs::{self, File},
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, RwLock, Semaphore},
    time::timeout,
};

const COMMAND_VERSION: u32 = 1;
const REPAIR_METADATA_MAX_ENTRIES: usize = 4_096;
const REPAIR_OBJECT_METADATA_MAX_ENTRIES: usize = 8_192;
const REPAIR_CANDIDATE_MAX_ENTRIES: usize = 8_192;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum HlsSegmentRepairSource {
    Normal,
    Transient,
}

impl HlsSegmentRepairSource {
    pub(super) const fn as_log_value(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Transient => "transient",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum HlsRepairRenderedObjectId {
    Normal { proxy_seq: u64 },
    Transient { resource_id: String },
}

#[derive(Debug, Clone)]
pub struct HlsSegmentRepairObjectContext {
    pub source: HlsSegmentRepairSource,
    pub proxy_session_id: ProxySessionId,
    pub hls_access_lease_id: Option<HlsAccessLeaseId>,
    pub rendered_object_id: HlsRepairRenderedObjectId,
    pub resource_id: String,
    pub file_ext: String,
    /// Concrete origin fetch URI retained for diagnostics and postprocess metadata.
    ///
    /// This may include a provider mirror or redirect/CDN host. It must not be used as HLS session identity, account
    /// binding, provider-failover state, repair object identity, or repair-window candidate identity.
    pub origin_fetch_uri_for_diagnostics: String,
    pub media_sequence: Option<u64>,
    pub discontinuity_sequence: Option<u64>,
    pub complete_object: bool,
    pub encrypted: bool,
    pub custom_response: bool,
}

impl HlsSegmentRepairObjectContext {
    pub(super) fn is_repairable_ts(&self) -> bool {
        self.file_ext.eq_ignore_ascii_case("ts") && self.complete_object && !self.encrypted && !self.custom_response
    }

    fn repair_skip_reason(&self) -> Option<&'static str> {
        if self.is_repairable_ts() {
            return None;
        }
        if !self.file_ext.eq_ignore_ascii_case("ts") {
            return Some("not-ts");
        }
        if !self.complete_object {
            return Some("partial-object");
        }
        if self.encrypted {
            return Some("encrypted");
        }
        if self.custom_response {
            return Some("custom-response");
        }
        None
    }
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
enum RepairCandidateSelection {
    Selected(HlsSegmentRepairMode),
    Skipped(&'static str),
    AlreadyChecked,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RepairStatus {
    Clean,
    Fixed,
    PolicyLimited,
    Unsupported,
    Timeout,
    RemuxFailed,
    ValidationFailed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RepairVideoCodec {
    H264,
    Hevc,
    Unsupported,
}

impl RepairVideoCodec {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[allow(dead_code)]
enum HlsSegmentRepairWarningKind {
    MissingPat,
    MissingPmt,
    MissingPmtPid,
    MissingVideoPidInPmt,
    MissingAudioPidInPmt,
    MultiplePrograms,
    PesPacketSizeMismatch,
    PacketCorrupt,
    ContinuityCheckFailed,
    MissingVps,
    MissingSps,
    MissingPps,
    VpsOutOfRange,
    SpsOutOfRange,
    PpsOutOfRange,
    NoFrame,
    DecodeSliceHeaderError,
    MissingPicture,
    InvalidNal,
    InvalidData,
    CodecParametersMissing,
    MmcoUnrefShortFailure,
    ReorderBufferIncrease,
    PpsIdOutOfRange,
    NoStartCode,
    NalSplitError,
    NalParseError,
    InvalidVclNalu,
    InvalidMetadataNalu,
    MultipleDolbyVisionRpus,
    AvDesync,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct HlsSegmentRepairDecision {
    codec: RepairVideoCodec,
    required_level: HlsSegmentRepairMode,
    trigger_source: HlsSegmentRepairTriggerSource,
    common_low_trigger: bool,
    codec_medium_trigger: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsSegmentRepairTriggerSource {
    Off,
    CommonMpegTsLow,
    H264Medium,
    H264High,
    HevcMedium,
    HevcHigh,
    UnsupportedCodec,
}

impl HlsSegmentRepairTriggerSource {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::CommonMpegTsLow => "common-mpegts-low",
            Self::H264Medium => "h264-medium",
            Self::H264High => "h264-high",
            Self::HevcMedium => "hevc-medium",
            Self::HevcHigh => "hevc-high",
            Self::UnsupportedCodec => "unsupported-codec",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum HlsSegmentRepairExecutionPlan {
    Repair(HlsSegmentRepairMode),
    SkipNoTrigger,
    SkipConfiguredMaxBelowRequired,
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

impl HlsRepairWindowRegistry {
    fn start_window(&mut self, lease_id: HlsAccessLeaseId, config: &HlsSegmentRepairConfig) {
        if config.max_level == HlsSegmentRepairMode::Off || config.apply_to_first_segments == 0 {
            self.windows.remove(&lease_id);
            return;
        }
        let generation = self
            .generations
            .entry(lease_id.clone())
            .and_modify(|generation| *generation = generation.saturating_add(1))
            .or_insert(1);
        self.windows.insert(
            lease_id,
            HlsRepairWindow {
                mode: config.max_level,
                activation_generation: *generation,
                remaining_segments: config.apply_to_first_segments,
                seen_candidates: HashSet::new(),
            },
        );
    }

    fn try_select_candidate(&mut self, context: &HlsSegmentRepairObjectContext) -> RepairCandidateSelection {
        if let Some(reason) = context.repair_skip_reason() {
            return RepairCandidateSelection::Skipped(reason);
        }
        let Some(lease_id) = context.hls_access_lease_id.as_ref() else {
            return RepairCandidateSelection::Skipped("missing-lease");
        };
        let Some(activation_generation) = self
            .windows
            .get(lease_id)
            .map(|window| window.activation_generation)
            .or_else(|| self.generations.get(lease_id).copied())
        else {
            return RepairCandidateSelection::Skipped("no-window");
        };
        let candidate_key = HlsRepairWindowCandidateKey {
            proxy_session_id: context.proxy_session_id.clone(),
            hls_access_lease_id: lease_id.clone(),
            activation_generation,
            object_id: context.rendered_object_id.clone(),
            file_ext: context.file_ext.clone(),
        };
        if !self.remember_candidate(candidate_key.clone()) {
            return RepairCandidateSelection::AlreadyChecked;
        }
        let Some(window) = self.windows.get_mut(lease_id) else {
            return RepairCandidateSelection::Skipped("no-window");
        };
        if window.seen_candidates.contains(&candidate_key) {
            return RepairCandidateSelection::Skipped("duplicate-identity");
        }
        if window.remaining_segments == 0 {
            return RepairCandidateSelection::Skipped("window-exhausted");
        }
        window.seen_candidates.insert(candidate_key);
        window.remaining_segments = window.remaining_segments.saturating_sub(1);
        RepairCandidateSelection::Selected(window.mode)
    }

    fn remove_access_lease(&mut self, lease_id: &HlsAccessLeaseId) {
        self.windows.remove(lease_id);
        self.generations.remove(lease_id);
        self.checked_candidates.retain(|key| key.hls_access_lease_id != *lease_id);
        self.checked_candidate_order.retain(|key| key.hls_access_lease_id != *lease_id);
    }

    fn remove_proxy_session(&mut self, proxy_session_id: &ProxySessionId, lease_ids: &[HlsAccessLeaseId]) {
        for lease_id in lease_ids {
            self.windows.remove(lease_id);
            self.generations.remove(lease_id);
        }
        self.checked_candidates.retain(|key| key.proxy_session_id != *proxy_session_id);
        self.checked_candidate_order.retain(|key| key.proxy_session_id != *proxy_session_id);
    }

    fn clear(&mut self) {
        self.windows.clear();
        self.generations.clear();
        self.checked_candidates.clear();
        self.checked_candidate_order.clear();
    }

    fn stats(&self) -> HlsSegmentRepairStats {
        HlsSegmentRepairStats {
            windows: self.windows.len(),
            generations: self.generations.len(),
            checked_candidates: self.checked_candidates.len(),
            ..HlsSegmentRepairStats::default()
        }
    }

    fn remember_candidate(&mut self, key: HlsRepairWindowCandidateKey) -> bool {
        if !self.checked_candidates.insert(key.clone()) {
            return false;
        }
        self.checked_candidate_order.push_back(key);
        self.prune_checked_candidates();
        true
    }

    fn prune_checked_candidates(&mut self) {
        while self.checked_candidates.len() > REPAIR_CANDIDATE_MAX_ENTRIES {
            let Some(oldest) = self.checked_candidate_order.pop_front() else {
                return;
            };
            self.checked_candidates.remove(&oldest);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct HlsSegmentRepairStats {
    pub windows: usize,
    pub generations: usize,
    pub checked_candidates: usize,
    pub metadata: usize,
    pub object_metadata: usize,
    pub locks: usize,
    pub watchdog_metadata: usize,
    pub watchdog_locks: usize,
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

impl HlsSegmentRepairRuntime {
    fn new(config: HlsSegmentRepairConfig) -> Self {
        let semaphore = if config.max_parallel_repairs == 0 {
            None
        } else {
            Some(Arc::new(Semaphore::new(config.max_parallel_repairs)))
        };
        let watchdog_config = &config.corrupt_segment_watchdog;
        let watchdog_semaphore = Arc::new(Semaphore::new(watchdog_config.max_parallel_jobs.max(1)));
        Self { config, semaphore, watchdog_semaphore }
    }

    fn repair_enabled(&self) -> bool {
        self.config.max_level != HlsSegmentRepairMode::Off && self.config.apply_to_first_segments > 0
    }

    fn postprocessing_enabled(&self) -> bool {
        self.repair_enabled() || self.config.corrupt_segment_watchdog.mode.is_enabled()
    }

    fn postprocess_timeout_ms(&self) -> u64 { self.config.postprocess_timeout_ms.max(100) }
}

fn log_segment_repair_config(config: &HlsSegmentRepairConfig) {
    debug!(
        "HLS segment repair configured: max_level={} segments={} max_parallel={} postprocess_timeout_ms={}",
        config.max_level.as_log_value(),
        config.apply_to_first_segments,
        config.max_parallel_repairs,
        config.postprocess_timeout_ms
    );
}

fn log_corrupt_segment_watchdog_config(config: &HlsCorruptSegmentWatchdogConfig) {
    debug!(
        "HLS corrupt segment watchdog configured: mode={} max_parallel_jobs={}",
        config.mode.as_log_value(),
        config.max_parallel_jobs
    );
}

impl HlsSegmentRepairManager {
    pub fn new(config: HlsSegmentRepairConfig) -> Self {
        let watchdog_config = config.corrupt_segment_watchdog.clone();
        log_segment_repair_config(&config);
        log_corrupt_segment_watchdog_config(&watchdog_config);
        Self {
            runtime: ArcSwap::from_pointee(HlsSegmentRepairRuntime::new(config)),
            watchdog: HlsCorruptSegmentWatchdogManager::new(),
            windows: RwLock::new(HlsRepairWindowRegistry::default()),
            metadata: RwLock::new(HashMap::new()),
            metadata_order: Mutex::new(VecDeque::new()),
            object_metadata: RwLock::new(HashMap::new()),
            object_metadata_order: Mutex::new(VecDeque::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn update_config(&self, config: HlsSegmentRepairConfig) {
        let current = self.runtime.load();
        if current.config == config {
            return;
        }
        let watchdog_config = config.corrupt_segment_watchdog.clone();
        log_segment_repair_config(&config);
        log_corrupt_segment_watchdog_config(&watchdog_config);
        self.runtime.store(Arc::new(HlsSegmentRepairRuntime::new(config)));
    }

    pub async fn start_access_lease_window(&self, lease_id: HlsAccessLeaseId) {
        let runtime = self.runtime.load_full();
        if !runtime.repair_enabled() {
            return;
        }
        self.windows.write().await.start_window(lease_id.clone(), &runtime.config);
        debug!(
            "HLS segment repair window started: lease={} max_level={} segments={}",
            safe_hls_access_lease_id(&lease_id),
            runtime.config.max_level.as_log_value(),
            runtime.config.apply_to_first_segments
        );
    }

    pub async fn remove_access_lease_window(&self, lease_id: &HlsAccessLeaseId) {
        self.windows.write().await.remove_access_lease(lease_id);
    }

    pub async fn remove_proxy_session_state(&self, proxy_session_id: &ProxySessionId, lease_ids: &[HlsAccessLeaseId]) {
        self.windows.write().await.remove_proxy_session(proxy_session_id, lease_ids);
        self.object_metadata.write().await.retain(|key, _| key.proxy_session_id != *proxy_session_id);
        self.object_metadata_order.lock().await.retain(|key| key.proxy_session_id != *proxy_session_id);
    }

    pub async fn clear_runtime_state(&self) {
        self.windows.write().await.clear();
        self.metadata.write().await.clear();
        self.metadata_order.lock().await.clear();
        self.object_metadata.write().await.clear();
        self.object_metadata_order.lock().await.clear();
        self.locks.lock().await.clear();
        self.watchdog.clear_runtime_state().await;
    }

    pub async fn stats(&self) -> HlsSegmentRepairStats {
        let mut stats = self.windows.read().await.stats();
        stats.metadata = self.metadata.read().await.len();
        stats.object_metadata = self.object_metadata.read().await.len();
        stats.locks = self.locks.lock().await.len();
        let watchdog = self.watchdog.stats().await;
        stats.watchdog_metadata = watchdog.metadata;
        stats.watchdog_locks = watchdog.locks;
        stats
    }

    pub async fn commit_origin_response<K, R>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        reader: R,
        deadline: Duration,
        context: HlsSegmentRepairObjectContext,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let raw = segment_cache.stage_temp_with_timeout(key, reader, deadline).await?;
        let runtime = self.runtime.load_full();
        if !runtime.postprocessing_enabled() {
            return segment_cache.commit_staged(key, raw).await;
        }
        let postprocessing_deadline = HlsPostProcessingDeadline::new(runtime.postprocess_timeout_ms());
        self.process_staged_and_commit(segment_cache, key, raw, context, runtime, postprocessing_deadline).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn repair_ready_cache_hit<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        context: HlsSegmentRepairObjectContext,
    ) -> io::Result<Option<CachedSegmentMetadata>>
    where
        K: HlsCacheObjectKey,
    {
        if !self.runtime.load().repair_enabled() {
            return Ok(None);
        }
        let Some((mode, runtime)) = self.try_select_candidate(&context).await else {
            return Ok(None);
        };
        let Some(metadata) = segment_cache.metadata(key).await? else {
            return Ok(None);
        };
        let raw_hash = sha256_file(&metadata.path).await?;
        let object_key = repair_object_metadata_key(&context, mode);
        if self.object_metadata_matches(&object_key, &raw_hash).await {
            return Ok(None);
        }
        let identity = RepairIdentity {
            raw_sha256: raw_hash.clone(),
            repair_mode: mode,
            command_version: COMMAND_VERSION,
            ffmpeg_version: ffmpeg_identity_version(),
        };
        if self.repair_metadata(&identity).await.is_some() {
            self.record_object_metadata_from_repair_identity(object_key, raw_hash.clone(), Some(raw_hash), &identity)
                .await;
            return Ok(None);
        }
        let lock = self.lock_for_identity(identity.clone()).await;
        let result = {
            let _guard = lock.lock().await;
            if let Some(current_metadata) = segment_cache.metadata(key).await? {
                let current_hash = sha256_file(&current_metadata.path).await?;
                if current_hash != raw_hash {
                    Ok(None)
                } else if self.repair_metadata(&identity).await.is_some() {
                    self.record_object_metadata_from_repair_identity(
                        object_key.clone(),
                        current_hash.clone(),
                        Some(current_hash),
                        &identity,
                    )
                    .await;
                    Ok(None)
                } else {
                    let deadline = HlsPostProcessingDeadline::new(runtime.postprocess_timeout_ms());
                    if let Some(fixed_path) = self
                        .repair_file(
                            &current_metadata.path,
                            current_metadata.size,
                            &identity,
                            &context,
                            runtime.clone(),
                            &deadline,
                        )
                        .await?
                    {
                        let fixed_size = fs::metadata(&fixed_path).await?.len();
                        let staged = StagedCacheObject { path: fixed_path, size: fixed_size };
                        let committed = segment_cache.commit_staged(key, staged).await?;
                        self.record_metadata(
                            identity.clone(),
                            RepairStatus::Fixed,
                            current_metadata.size,
                            committed.size,
                            None,
                        )
                        .await;
                        let committed_hash = sha256_file(&committed.path).await?;
                        self.record_object_metadata(
                            object_key.clone(),
                            HlsRepairObjectMetadata {
                                committed_sha256: committed_hash,
                                raw_sha256: Some(current_hash),
                                status: RepairStatus::Fixed,
                                raw_size: current_metadata.size,
                                final_size: committed.size,
                                validation_reason: None,
                            },
                        )
                        .await;
                        Ok(Some(committed))
                    } else {
                        self.record_object_metadata_from_repair_identity(
                            object_key.clone(),
                            current_hash.clone(),
                            Some(current_hash),
                            &identity,
                        )
                        .await;
                        Ok(None)
                    }
                }
            } else {
                Ok(None)
            }
        };
        self.remove_lock_if_unused(&identity, &lock).await;
        result
    }

    async fn process_staged_and_commit<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        raw: StagedCacheObject,
        context: HlsSegmentRepairObjectContext,
        runtime: Arc<HlsSegmentRepairRuntime>,
        deadline: HlsPostProcessingDeadline,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        let selected_repair = self.try_select_candidate_with_runtime(&context, &runtime).await;
        if selected_repair.is_none()
            && runtime.config.corrupt_segment_watchdog.mode.is_enabled()
            && context.is_repairable_ts()
        {
            let raw_hash = sha256_file(&raw.path).await?;
            return self
                .watchdog
                .process_staged_and_commit(
                    segment_cache,
                    key,
                    raw,
                    &context,
                    &runtime.config.corrupt_segment_watchdog,
                    &runtime.watchdog_semaphore,
                    raw_hash,
                    &deadline,
                )
                .await;
        }
        let Some(mode) = selected_repair else {
            return segment_cache.commit_staged(key, raw).await;
        };
        let raw_hash = sha256_file(&raw.path).await?;
        let object_key = repair_object_metadata_key(&context, mode);
        if self.object_metadata_matches(&object_key, &raw_hash).await {
            return segment_cache.commit_staged(key, raw).await;
        }
        let identity = RepairIdentity {
            raw_sha256: raw_hash.clone(),
            repair_mode: mode,
            command_version: COMMAND_VERSION,
            ffmpeg_version: ffmpeg_identity_version(),
        };
        if self.repair_metadata(&identity).await.is_some() {
            let committed = segment_cache.commit_staged(key, raw).await?;
            self.record_object_metadata_from_repair_identity(object_key, raw_hash.clone(), Some(raw_hash), &identity)
                .await;
            return Ok(committed);
        }
        let lock = self.lock_for_identity(identity.clone()).await;
        let result = {
            let _guard = lock.lock().await;
            if self.repair_metadata(&identity).await.is_some() {
                let committed = segment_cache.commit_staged(key, raw).await?;
                self.record_object_metadata_from_repair_identity(
                    object_key.clone(),
                    raw_hash.clone(),
                    Some(raw_hash.clone()),
                    &identity,
                )
                .await;
                Ok(committed)
            } else {
                let raw_size = raw.size;
                if let Some(fixed_path) =
                    self.repair_file(&raw.path, raw.size, &identity, &context, runtime.clone(), &deadline).await?
                {
                    let fixed_size = fs::metadata(&fixed_path).await?.len();
                    let _ = segment_cache.remove_staged(raw.clone()).await;
                    let committed = segment_cache
                        .commit_staged(key, StagedCacheObject { path: fixed_path, size: fixed_size })
                        .await?;
                    self.record_metadata(identity.clone(), RepairStatus::Fixed, raw.size, committed.size, None).await;
                    let committed_hash = sha256_file(&committed.path).await?;
                    self.record_object_metadata(
                        object_key.clone(),
                        HlsRepairObjectMetadata {
                            committed_sha256: committed_hash,
                            raw_sha256: Some(raw_hash.clone()),
                            status: RepairStatus::Fixed,
                            raw_size,
                            final_size: committed.size,
                            validation_reason: None,
                        },
                    )
                    .await;
                    Ok(committed)
                } else {
                    let committed = segment_cache.commit_staged(key, raw).await?;
                    self.record_object_metadata_from_repair_identity(
                        object_key.clone(),
                        raw_hash.clone(),
                        Some(raw_hash.clone()),
                        &identity,
                    )
                    .await;
                    Ok(committed)
                }
            }
        };
        self.remove_lock_if_unused(&identity, &lock).await;
        result
    }

    async fn try_select_candidate(
        &self,
        context: &HlsSegmentRepairObjectContext,
    ) -> Option<(HlsSegmentRepairMode, Arc<HlsSegmentRepairRuntime>)> {
        let runtime = self.runtime.load_full();
        self.try_select_candidate_with_runtime(context, &runtime).await.map(|mode| (mode, runtime))
    }

    async fn try_select_candidate_with_runtime(
        &self,
        context: &HlsSegmentRepairObjectContext,
        runtime: &Arc<HlsSegmentRepairRuntime>,
    ) -> Option<HlsSegmentRepairMode> {
        if !runtime.repair_enabled() {
            return None;
        }
        context.hls_access_lease_id.as_ref()?;
        match self.windows.write().await.try_select_candidate(context) {
            RepairCandidateSelection::Selected(mode) => {
                debug!(
                    "HLS segment repair candidate selected: session={} lease={} source={} resource={} mode={}",
                    safe_proxy_session_id(&context.proxy_session_id),
                    context.hls_access_lease_id.as_ref().map_or_else(|| "<none>".to_string(), safe_hls_access_lease_id),
                    context.source.as_log_value(),
                    context.resource_id,
                    mode.as_log_value()
                );
                Some(mode)
            }
            RepairCandidateSelection::Skipped(reason) => {
                debug!(
                    "HLS segment repair candidate skipped: session={} lease={} source={} resource={} reason={}",
                    safe_proxy_session_id(&context.proxy_session_id),
                    context.hls_access_lease_id.as_ref().map_or_else(|| "<none>".to_string(), safe_hls_access_lease_id),
                    context.source.as_log_value(),
                    context.resource_id,
                    reason
                );
                None
            }
            RepairCandidateSelection::AlreadyChecked => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn repair_file(
        &self,
        raw_path: &Path,
        raw_size: u64,
        identity: &RepairIdentity,
        context: &HlsSegmentRepairObjectContext,
        runtime: Arc<HlsSegmentRepairRuntime>,
        deadline: &HlsPostProcessingDeadline,
    ) -> io::Result<Option<PathBuf>> {
        let _permit = match &runtime.semaphore {
            Some(semaphore) => {
                let Some(remaining) = deadline.remaining() else {
                    self.record_metadata(
                        identity.clone(),
                        RepairStatus::Timeout,
                        raw_size,
                        raw_size,
                        Some("timeout".to_string()),
                    )
                    .await;
                    return Ok(None);
                };
                Some(
                    timeout(remaining, semaphore.acquire())
                        .await
                        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "repair semaphore timed out"))?
                        .map_err(|_| io::Error::other("repair semaphore closed"))?,
                )
            }
            None => None,
        };
        let started = Instant::now();
        let raw_scan = match analyze_segment(raw_path, deadline).await {
            Ok(scan) => scan,
            Err(reason) => {
                debug_repair_event(context, identity.repair_mode, "analysis skipped", Some(&reason));
                self.record_metadata(identity.clone(), RepairStatus::Unsupported, raw_size, raw_size, Some(reason))
                    .await;
                return Ok(None);
            }
        };
        let codec = detect_video_codec(&raw_scan);
        let decision = decide_repair(codec, &raw_scan.warnings);
        let execution_plan = identity.repair_mode.execution_plan(decision.required_level);
        let executed_level = match execution_plan {
            HlsSegmentRepairExecutionPlan::Repair(level) => Some(level),
            HlsSegmentRepairExecutionPlan::SkipNoTrigger
            | HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired => None,
        };
        debug_repair_analysis(context, identity.repair_mode, decision, executed_level, &raw_scan.warnings);
        let executed_level = match execution_plan {
            HlsSegmentRepairExecutionPlan::Repair(level) => level,
            HlsSegmentRepairExecutionPlan::SkipNoTrigger => {
                self.record_metadata(identity.clone(), RepairStatus::Clean, raw_size, raw_size, None).await;
                return Ok(None);
            }
            HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired => {
                warn!(
                    "HLS segment repair required level exceeds configured max: session={} source={} resource={} configured_max_level={} required_level={} trigger={} action=raw_commit",
                    safe_proxy_session_id(&context.proxy_session_id),
                    context.source.as_log_value(),
                    context.resource_id,
                    identity.repair_mode.as_log_value(),
                    decision.required_level.as_log_value(),
                    decision.trigger_source.as_log_value()
                );
                self.record_metadata(
                    identity.clone(),
                    RepairStatus::PolicyLimited,
                    raw_size,
                    raw_size,
                    Some("configured_max_level_below_required_level".to_string()),
                )
                .await;
                return Ok(None);
            }
        };
        let stream_selection = match select_repair_remux_streams(&raw_scan) {
            Ok(selection) => selection,
            Err(reason) => {
                debug_repair_event(context, executed_level, "stream selection skipped", Some(&reason));
                self.record_metadata(identity.clone(), RepairStatus::Unsupported, raw_size, raw_size, Some(reason))
                    .await;
                return Ok(None);
            }
        };
        for dropped in &stream_selection.dropped_streams {
            debug_repair_stream_dropped(context, dropped);
        }
        let fixed_path = repair_output_path(raw_path);
        debug!(
            "HLS segment repair remux started: session={} source={} resource={} configured_max_level={} executed_level={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            identity.repair_mode.as_log_value(),
            executed_level.as_log_value()
        );
        let remux = run_remux(raw_path, &fixed_path, executed_level, &stream_selection, deadline).await;
        if let Err(reason) = remux {
            debug_repair_event(context, executed_level, "remux failed", Some(&reason));
            let status = if reason == "timeout" { RepairStatus::Timeout } else { RepairStatus::RemuxFailed };
            let _ = fs::remove_file(&fixed_path).await;
            self.record_metadata(identity.clone(), status, raw_size, raw_size, Some(reason)).await;
            return Ok(None);
        }
        let fixed_scan = match analyze_segment(&fixed_path, deadline).await {
            Ok(scan) => scan,
            Err(reason) => {
                debug_repair_event(context, executed_level, "validation probe failed", Some(&reason));
                let _ = fs::remove_file(&fixed_path).await;
                self.record_metadata(
                    identity.clone(),
                    RepairStatus::ValidationFailed,
                    raw_size,
                    raw_size,
                    Some(reason),
                )
                .await;
                return Ok(None);
            }
        };
        let validation = validate_repair(&runtime.config, codec, &raw_scan, &fixed_scan, executed_level, &stream_selection);
        if let Err(reason) = validation {
            debug_repair_event(context, executed_level, "validation failed", Some(&reason));
            let _ = fs::remove_file(&fixed_path).await;
            self.record_metadata(identity.clone(), RepairStatus::ValidationFailed, raw_size, raw_size, Some(reason))
                .await;
            return Ok(None);
        }
        debug!(
            "HLS segment repair remux completed: session={} source={} resource={} configured_max_level={} executed_level={} duration_ms={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            identity.repair_mode.as_log_value(),
            executed_level.as_log_value(),
            started.elapsed().as_millis()
        );
        Ok(Some(fixed_path))
    }

    async fn record_metadata(
        &self,
        identity: RepairIdentity,
        status: RepairStatus,
        raw_size: u64,
        final_size: u64,
        validation_reason: Option<String>,
    ) {
        let inserted_new = {
            let mut metadata = self.metadata.write().await;
            let inserted_new = !metadata.contains_key(&identity);
            metadata
                .insert(identity.clone(), SegmentRepairMetadata { status, raw_size, final_size, validation_reason });
            inserted_new
        };
        if inserted_new {
            self.metadata_order.lock().await.push_back(identity);
        }
        self.prune_metadata().await;
    }

    async fn repair_metadata(&self, identity: &RepairIdentity) -> Option<SegmentRepairMetadata> {
        self.metadata.read().await.get(identity).cloned()
    }

    async fn object_metadata_matches(&self, key: &HlsRepairObjectMetadataKey, committed_sha256: &str) -> bool {
        let matches = self
            .object_metadata
            .read()
            .await
            .get(key)
            .is_some_and(|metadata| metadata.committed_sha256 == committed_sha256);
        if matches {
            let (source, resource) = match &key.rendered_object_id {
                HlsRepairRenderedObjectId::Normal { proxy_seq } => ("normal", format!("{proxy_seq:06}")),
                HlsRepairRenderedObjectId::Transient { resource_id } => ("transient", resource_id.clone()),
            };
            debug!(
                "HLS segment repair object metadata hit: session={} source={} resource={} mode={}",
                safe_proxy_session_id(&key.proxy_session_id),
                source,
                resource,
                key.repair_mode.as_log_value()
            );
        }
        matches
    }

    async fn record_object_metadata_from_repair_identity(
        &self,
        key: HlsRepairObjectMetadataKey,
        committed_sha256: String,
        raw_sha256: Option<String>,
        identity: &RepairIdentity,
    ) {
        let Some(metadata) = self.repair_metadata(identity).await else {
            return;
        };
        self.record_object_metadata(
            key,
            HlsRepairObjectMetadata {
                committed_sha256,
                raw_sha256,
                status: metadata.status,
                raw_size: metadata.raw_size,
                final_size: metadata.final_size,
                validation_reason: metadata.validation_reason,
            },
        )
        .await;
    }

    async fn record_object_metadata(&self, key: HlsRepairObjectMetadataKey, metadata: HlsRepairObjectMetadata) {
        let inserted_new = {
            let mut object_metadata = self.object_metadata.write().await;
            let inserted_new = !object_metadata.contains_key(&key);
            object_metadata.insert(key.clone(), metadata);
            inserted_new
        };
        if inserted_new {
            self.object_metadata_order.lock().await.push_back(key);
        }
        self.prune_object_metadata().await;
    }

    async fn lock_for_identity(&self, identity: RepairIdentity) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(locks.entry(identity).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    async fn remove_lock_if_unused(&self, identity: &RepairIdentity, lock: &Arc<Mutex<()>>) {
        let mut locks = self.locks.lock().await;
        if Arc::strong_count(lock) <= 2 && locks.get(identity).is_some_and(|current| Arc::ptr_eq(current, lock)) {
            locks.remove(identity);
        }
    }

    async fn prune_metadata(&self) {
        loop {
            let should_prune = self.metadata.read().await.len() > REPAIR_METADATA_MAX_ENTRIES;
            if !should_prune {
                return;
            }
            let Some(oldest) = self.metadata_order.lock().await.pop_front() else {
                return;
            };
            self.metadata.write().await.remove(&oldest);
        }
    }

    async fn prune_object_metadata(&self) {
        loop {
            let should_prune = self.object_metadata.read().await.len() > REPAIR_OBJECT_METADATA_MAX_ENTRIES;
            if !should_prune {
                return;
            }
            let Some(oldest) = self.object_metadata_order.lock().await.pop_front() else {
                return;
            };
            self.object_metadata.write().await.remove(&oldest);
        }
    }
}

fn repair_object_metadata_key(
    context: &HlsSegmentRepairObjectContext,
    repair_mode: HlsSegmentRepairMode,
) -> HlsRepairObjectMetadataKey {
    HlsRepairObjectMetadataKey {
        proxy_session_id: context.proxy_session_id.clone(),
        rendered_object_id: context.rendered_object_id.clone(),
        file_ext: context.file_ext.to_ascii_lowercase(),
        repair_mode,
        command_version: COMMAND_VERSION,
        ffmpeg_version: ffmpeg_identity_version(),
    }
}

impl HlsSegmentRepairMode {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    const fn execution_plan(self, required_level: Self) -> HlsSegmentRepairExecutionPlan {
        if matches!(self, Self::Off) || matches!(required_level, Self::Off) {
            HlsSegmentRepairExecutionPlan::SkipNoTrigger
        } else if self.rank() < required_level.rank() {
            HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired
        } else {
            HlsSegmentRepairExecutionPlan::Repair(required_level)
        }
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WarningCounters {
    pub missing_pat: u32,
    pub missing_pmt: u32,
    pub missing_pmt_pid: u32,
    pub missing_video_pid_in_pmt: u32,
    pub missing_audio_pid_in_pmt: u32,
    pub multiple_programs: u32,
    pub pes_packet_size_mismatch: u32,
    pub packet_corrupt: u32,
    pub continuity_check_failed: u32,
    pub missing_vps: u32,
    pub missing_sps: u32,
    pub missing_pps: u32,
    pub vps_out_of_range: u32,
    pub sps_out_of_range: u32,
    pub pps_out_of_range: u32,
    pub no_frame: u32,
    pub decode_slice_header_error: u32,
    pub missing_picture: u32,
    pub invalid_nal: u32,
    pub invalid_data: u32,
    pub codec_parameters_missing: u32,
    pub mmco_unref_short_failure: u32,
    pub reorder_buffer: u32,
    pub pps_id_out_of_range: u32,
    pub no_start_code: u32,
    pub nal_split_error: u32,
    pub nal_parse_error: u32,
    pub invalid_undecodable_nalu_total: u32,
    pub invalid_undecodable_nalu_non_metadata: u32,
    pub invalid_undecodable_nalu_keyframe: u32,
    pub invalid_undecodable_nalu_metadata: u32,
    pub dolby_vision_rpu: u32,
    pub av_desync: u32,
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SegmentProbeStreamType {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RepairRemuxStreamSelection {
    mapped_streams: Vec<usize>,
    dropped_streams: Vec<RepairRemuxDroppedStream>,
}

#[cfg(test)]
impl RepairRemuxStreamSelection {
    fn preserve_all(probe: &SegmentProbe) -> Self {
        Self {
            mapped_streams: probe.streams.iter().map(|stream| stream.index).collect(),
            dropped_streams: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RepairRemuxDroppedStream {
    index: usize,
    reason: &'static str,
}

pub fn parse_ffmpeg_warnings(stderr: &str) -> WarningCounters {
    let mut counters = WarningCounters::default();
    let mut last_increment: Option<fn(&mut WarningCounters, u32)> = None;
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(repeated) = parse_repeated_count(trimmed) {
            if let Some(increment) = last_increment {
                increment(&mut counters, repeated);
            }
            continue;
        }
        if let Some(increment) = warning_increment(trimmed) {
            increment(&mut counters, 1);
            last_increment = Some(increment);
        }
    }
    counters
}

fn parse_repeated_count(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Last message repeated ")?;
    let count = rest.strip_suffix(" times")?.parse().ok()?;
    Some(count)
}

#[allow(clippy::too_many_lines)]
fn warning_increment(line: &str) -> Option<fn(&mut WarningCounters, u32)> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("missing pat") {
        return Some(|counters, count| counters.missing_pat = counters.missing_pat.saturating_add(count));
    }
    if lower.contains("missing pmt pid") {
        return Some(|counters, count| counters.missing_pmt_pid = counters.missing_pmt_pid.saturating_add(count));
    }
    if lower.contains("missing pmt") {
        return Some(|counters, count| counters.missing_pmt = counters.missing_pmt.saturating_add(count));
    }
    if lower.contains("missing video pid") && lower.contains("pmt") {
        return Some(|counters, count| {
            counters.missing_video_pid_in_pmt = counters.missing_video_pid_in_pmt.saturating_add(count);
        });
    }
    if lower.contains("missing audio pid") && lower.contains("pmt") {
        return Some(|counters, count| {
            counters.missing_audio_pid_in_pmt = counters.missing_audio_pid_in_pmt.saturating_add(count);
        });
    }
    if lower.contains("multiple mpeg-ts programs") || lower.contains("multiple programs") {
        return Some(|counters, count| counters.multiple_programs = counters.multiple_programs.saturating_add(count));
    }
    if lower.contains("pes packet size mismatch") {
        return Some(|counters, count| {
            counters.pes_packet_size_mismatch = counters.pes_packet_size_mismatch.saturating_add(count);
        });
    }
    if lower.contains("packet corrupt") {
        return Some(|counters, count| counters.packet_corrupt = counters.packet_corrupt.saturating_add(count));
    }
    if lower.contains("continuity check failed") {
        return Some(|counters, count| {
            counters.continuity_check_failed = counters.continuity_check_failed.saturating_add(count);
        });
    }
    if lower.contains("non-existing vps") || lower.contains("missing vps") {
        return Some(|counters, count| counters.missing_vps = counters.missing_vps.saturating_add(count));
    }
    if lower.contains("non-existing sps") || lower.contains("missing sps") {
        return Some(|counters, count| counters.missing_sps = counters.missing_sps.saturating_add(count));
    }
    if lower.contains("non-existing pps") || lower.contains("missing pps") {
        return Some(|counters, count| counters.missing_pps = counters.missing_pps.saturating_add(count));
    }
    if lower.contains("vps id out of range") {
        return Some(|counters, count| counters.vps_out_of_range = counters.vps_out_of_range.saturating_add(count));
    }
    if lower.contains("sps id out of range") {
        return Some(|counters, count| counters.sps_out_of_range = counters.sps_out_of_range.saturating_add(count));
    }
    if lower.contains("pps id out of range") {
        return Some(|counters, count| {
            counters.pps_id_out_of_range = counters.pps_id_out_of_range.saturating_add(count);
            counters.pps_out_of_range = counters.pps_out_of_range.saturating_add(count);
        });
    }
    if lower.contains("vps") && lower.contains("out of range") {
        return Some(|counters, count| counters.vps_out_of_range = counters.vps_out_of_range.saturating_add(count));
    }
    if lower.contains("sps") && lower.contains("out of range") {
        return Some(|counters, count| counters.sps_out_of_range = counters.sps_out_of_range.saturating_add(count));
    }
    if lower.contains("pps") && lower.contains("out of range") {
        return Some(|counters, count| counters.pps_out_of_range = counters.pps_out_of_range.saturating_add(count));
    }
    if lower.contains("no frame") {
        return Some(|counters, count| counters.no_frame = counters.no_frame.saturating_add(count));
    }
    if lower.contains("decode_slice_header error") {
        return Some(|counters, count| {
            counters.decode_slice_header_error = counters.decode_slice_header_error.saturating_add(count);
        });
    }
    if lower.contains("missing picture") {
        return Some(|counters, count| counters.missing_picture = counters.missing_picture.saturating_add(count));
    }
    if lower.contains("invalid nal") {
        return Some(|counters, count| counters.invalid_nal = counters.invalid_nal.saturating_add(count));
    }
    if lower.contains("invalid data found") {
        return Some(|counters, count| counters.invalid_data = counters.invalid_data.saturating_add(count));
    }
    if lower.contains("could not find codec parameters") {
        return Some(|counters, count| {
            counters.codec_parameters_missing = counters.codec_parameters_missing.saturating_add(count);
        });
    }
    if lower.contains("mmco: unref short failure") {
        return Some(|counters, count| {
            counters.mmco_unref_short_failure = counters.mmco_unref_short_failure.saturating_add(count);
        });
    }
    if lower.contains("increasing reorder buffer") {
        return Some(|counters, count| counters.reorder_buffer = counters.reorder_buffer.saturating_add(count));
    }
    if lower.contains("no start code is found") {
        return Some(|counters, count| counters.no_start_code = counters.no_start_code.saturating_add(count));
    }
    if lower.contains("error splitting") && lower.contains("nal") {
        return Some(|counters, count| counters.nal_split_error = counters.nal_split_error.saturating_add(count));
    }
    if lower.contains("error parsing nal unit") {
        return Some(|counters, count| counters.nal_parse_error = counters.nal_parse_error.saturating_add(count));
    }
    if let Some(nalu_type) = parse_invalid_undecodable_nalu_type(line) {
        return Some(match nalu_type {
            0..=20 | 22..=31 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_non_metadata =
                    counters.invalid_undecodable_nalu_non_metadata.saturating_add(count);
            },
            21 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_keyframe =
                    counters.invalid_undecodable_nalu_keyframe.saturating_add(count);
            },
            39 => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
                counters.invalid_undecodable_nalu_metadata =
                    counters.invalid_undecodable_nalu_metadata.saturating_add(count);
            },
            _ => |counters: &mut WarningCounters, count| {
                counters.invalid_undecodable_nalu_total = counters.invalid_undecodable_nalu_total.saturating_add(count);
            },
        });
    }
    if lower.contains("multiple dolby vision rpus found in one au") {
        return Some(|counters, count| counters.dolby_vision_rpu = counters.dolby_vision_rpu.saturating_add(count));
    }
    if lower.contains("audio/video desynchronisation detected") {
        return Some(|counters, count| counters.av_desync = counters.av_desync.saturating_add(count));
    }
    None
}

fn parse_invalid_undecodable_nalu_type(line: &str) -> Option<u8> {
    let (_, rest) = line.split_once("Skipping invalid undecodable NALU:")?;
    rest.trim().split(|ch: char| !ch.is_ascii_digit()).next().filter(|value| !value.is_empty())?.parse().ok()
}

fn warning_count(warnings: &WarningCounters, kind: HlsSegmentRepairWarningKind) -> u32 {
    match kind {
        HlsSegmentRepairWarningKind::MissingPat => warnings.missing_pat,
        HlsSegmentRepairWarningKind::MissingPmt => warnings.missing_pmt,
        HlsSegmentRepairWarningKind::MissingPmtPid => warnings.missing_pmt_pid,
        HlsSegmentRepairWarningKind::MissingVideoPidInPmt => warnings.missing_video_pid_in_pmt,
        HlsSegmentRepairWarningKind::MissingAudioPidInPmt => warnings.missing_audio_pid_in_pmt,
        HlsSegmentRepairWarningKind::MultiplePrograms => warnings.multiple_programs,
        HlsSegmentRepairWarningKind::PesPacketSizeMismatch => warnings.pes_packet_size_mismatch,
        HlsSegmentRepairWarningKind::PacketCorrupt => warnings.packet_corrupt,
        HlsSegmentRepairWarningKind::ContinuityCheckFailed => warnings.continuity_check_failed,
        HlsSegmentRepairWarningKind::MissingVps => warnings.missing_vps,
        HlsSegmentRepairWarningKind::MissingSps => warnings.missing_sps,
        HlsSegmentRepairWarningKind::MissingPps => warnings.missing_pps,
        HlsSegmentRepairWarningKind::VpsOutOfRange => warnings.vps_out_of_range,
        HlsSegmentRepairWarningKind::SpsOutOfRange => warnings.sps_out_of_range,
        HlsSegmentRepairWarningKind::PpsOutOfRange => warnings.pps_out_of_range,
        HlsSegmentRepairWarningKind::NoFrame => warnings.no_frame,
        HlsSegmentRepairWarningKind::DecodeSliceHeaderError => warnings.decode_slice_header_error,
        HlsSegmentRepairWarningKind::MissingPicture => warnings.missing_picture,
        HlsSegmentRepairWarningKind::InvalidNal => warnings.invalid_nal,
        HlsSegmentRepairWarningKind::InvalidData => warnings.invalid_data,
        HlsSegmentRepairWarningKind::CodecParametersMissing => warnings.codec_parameters_missing,
        HlsSegmentRepairWarningKind::MmcoUnrefShortFailure => warnings.mmco_unref_short_failure,
        HlsSegmentRepairWarningKind::ReorderBufferIncrease => warnings.reorder_buffer,
        HlsSegmentRepairWarningKind::PpsIdOutOfRange => warnings.pps_id_out_of_range,
        HlsSegmentRepairWarningKind::NoStartCode => warnings.no_start_code,
        HlsSegmentRepairWarningKind::NalSplitError => warnings.nal_split_error,
        HlsSegmentRepairWarningKind::NalParseError => warnings.nal_parse_error,
        HlsSegmentRepairWarningKind::InvalidVclNalu => {
            warnings.invalid_undecodable_nalu_non_metadata.saturating_add(warnings.invalid_undecodable_nalu_keyframe)
        }
        HlsSegmentRepairWarningKind::InvalidMetadataNalu => warnings.invalid_undecodable_nalu_metadata,
        HlsSegmentRepairWarningKind::MultipleDolbyVisionRpus => warnings.dolby_vision_rpu,
        HlsSegmentRepairWarningKind::AvDesync => warnings.av_desync,
    }
}

fn has_any_warning(warnings: &WarningCounters, kinds: &[HlsSegmentRepairWarningKind]) -> bool {
    kinds.iter().any(|kind| warning_count(warnings, *kind) > 0)
}

fn common_mpegts_low_trigger(warnings: &WarningCounters) -> bool {
    has_any_warning(
        warnings,
        &[
            HlsSegmentRepairWarningKind::MissingPat,
            HlsSegmentRepairWarningKind::MissingPmt,
            HlsSegmentRepairWarningKind::MissingPmtPid,
            HlsSegmentRepairWarningKind::MissingVideoPidInPmt,
            HlsSegmentRepairWarningKind::MissingAudioPidInPmt,
            HlsSegmentRepairWarningKind::MultiplePrograms,
            HlsSegmentRepairWarningKind::PesPacketSizeMismatch,
            HlsSegmentRepairWarningKind::PacketCorrupt,
            HlsSegmentRepairWarningKind::ContinuityCheckFailed,
            HlsSegmentRepairWarningKind::CodecParametersMissing,
        ],
    )
}

fn h264_parameter_trigger(warnings: &WarningCounters) -> bool {
    has_any_warning(
        warnings,
        &[
            HlsSegmentRepairWarningKind::MissingSps,
            HlsSegmentRepairWarningKind::MissingPps,
            HlsSegmentRepairWarningKind::SpsOutOfRange,
            HlsSegmentRepairWarningKind::PpsOutOfRange,
        ],
    )
}

fn h264_medium_trigger(warnings: &WarningCounters) -> bool {
    h264_parameter_trigger(warnings)
        || warnings.decode_slice_header_error > 0
        || warnings.no_frame > 0
        || (warnings.missing_picture > 0
            && (warnings.missing_sps > 0 || warnings.missing_pps > 0 || warnings.decode_slice_header_error > 0))
        || (warnings.invalid_nal > 0 && h264_parameter_trigger(warnings))
        || (warnings.no_start_code > 0 && (warnings.missing_sps > 0 || warnings.missing_pps > 0))
        || (warnings.nal_split_error > 0 && (warnings.missing_sps > 0 || warnings.missing_pps > 0))
}

fn hevc_missing_parameter_trigger(warnings: &WarningCounters) -> bool {
    warnings.missing_vps > 0 || warnings.missing_sps > 0 || warnings.missing_pps > 0
}

fn hevc_parameter_trigger(warnings: &WarningCounters) -> bool {
    hevc_missing_parameter_trigger(warnings)
        || warnings.vps_out_of_range > 0
        || warnings.sps_out_of_range > 0
        || warnings.pps_out_of_range > 0
        || warnings.pps_id_out_of_range > 0
}

fn hevc_invalid_vcl_nalu(warnings: &WarningCounters) -> bool {
    warning_count(warnings, HlsSegmentRepairWarningKind::InvalidVclNalu) > 0
}

fn hevc_medium_trigger(warnings: &WarningCounters) -> bool {
    hevc_parameter_trigger(warnings)
        || (hevc_invalid_vcl_nalu(warnings) && hevc_parameter_trigger(warnings))
        || (warnings.nal_parse_error > 0 && hevc_parameter_trigger(warnings))
        || (warnings.invalid_nal > 0 && hevc_parameter_trigger(warnings))
        || (warnings.no_start_code > 0 && hevc_missing_parameter_trigger(warnings))
        || (warnings.nal_split_error > 0 && hevc_missing_parameter_trigger(warnings))
}

fn decide_repair(codec: RepairVideoCodec, warnings: &WarningCounters) -> HlsSegmentRepairDecision {
    let common_low_trigger = common_mpegts_low_trigger(warnings);
    let codec_parameters_missing = warnings.codec_parameters_missing > 0;
    let codec_medium_trigger = match codec {
        RepairVideoCodec::H264 => h264_medium_trigger(warnings),
        RepairVideoCodec::Hevc => hevc_medium_trigger(warnings),
        RepairVideoCodec::Unsupported => false,
    };
    match codec {
        RepairVideoCodec::H264 if codec_medium_trigger && (common_low_trigger || codec_parameters_missing) => {
            HlsSegmentRepairDecision {
                codec,
                required_level: HlsSegmentRepairMode::High,
                trigger_source: HlsSegmentRepairTriggerSource::H264High,
                common_low_trigger,
                codec_medium_trigger,
            }
        }
        RepairVideoCodec::H264 if codec_medium_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Medium,
            trigger_source: HlsSegmentRepairTriggerSource::H264Medium,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::Hevc
            if (common_low_trigger && (codec_medium_trigger || hevc_invalid_vcl_nalu(warnings)))
                || (codec_parameters_missing && codec_medium_trigger) =>
        {
            HlsSegmentRepairDecision {
                codec,
                required_level: HlsSegmentRepairMode::High,
                trigger_source: HlsSegmentRepairTriggerSource::HevcHigh,
                common_low_trigger,
                codec_medium_trigger,
            }
        }
        RepairVideoCodec::Hevc if codec_medium_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Medium,
            trigger_source: HlsSegmentRepairTriggerSource::HevcMedium,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::H264 | RepairVideoCodec::Hevc if common_low_trigger => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Low,
            trigger_source: HlsSegmentRepairTriggerSource::CommonMpegTsLow,
            common_low_trigger,
            codec_medium_trigger,
        },
        RepairVideoCodec::Unsupported => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Off,
            trigger_source: HlsSegmentRepairTriggerSource::UnsupportedCodec,
            common_low_trigger,
            codec_medium_trigger,
        },
        _ => HlsSegmentRepairDecision {
            codec,
            required_level: HlsSegmentRepairMode::Off,
            trigger_source: HlsSegmentRepairTriggerSource::Off,
            common_low_trigger,
            codec_medium_trigger,
        },
    }
}

fn debug_repair_analysis(
    context: &HlsSegmentRepairObjectContext,
    configured_max_level: HlsSegmentRepairMode,
    decision: HlsSegmentRepairDecision,
    executed_level: Option<HlsSegmentRepairMode>,
    warnings: &WarningCounters,
) {
    let executed_level = executed_level.map_or("off", HlsSegmentRepairMode::as_log_value);
    match decision.codec {
        RepairVideoCodec::H264 => debug!(
            "HLS segment repair analysis completed: session={} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={} common_low={} codec_medium={} missing_sps={} missing_pps={} sps_out_of_range={} pps_out_of_range={} no_frame={} decode_slice_header_error={} missing_picture={} invalid_nal={} no_start_code={} nal_split_error={} packet_corrupt={} continuity_check_failed={} codec_parameters_missing={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value(),
            decision.common_low_trigger,
            decision.codec_medium_trigger,
            warnings.missing_sps,
            warnings.missing_pps,
            warnings.sps_out_of_range,
            warnings.pps_out_of_range,
            warnings.no_frame,
            warnings.decode_slice_header_error,
            warnings.missing_picture,
            warnings.invalid_nal,
            warnings.no_start_code,
            warnings.nal_split_error,
            warnings.packet_corrupt,
            warnings.continuity_check_failed,
            warnings.codec_parameters_missing
        ),
        RepairVideoCodec::Hevc => debug!(
            "HLS segment repair analysis completed: session={} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={} common_low={} codec_medium={} missing_vps={} missing_sps={} missing_pps={} vps_out_of_range={} sps_out_of_range={} pps_out_of_range={} pps_id_out_of_range={} invalid_vcl_nalu={} nal_parse_error={} invalid_nal={} no_start_code={} nal_split_error={} packet_corrupt={} continuity_check_failed={} codec_parameters_missing={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value(),
            decision.common_low_trigger,
            decision.codec_medium_trigger,
            warnings.missing_vps,
            warnings.missing_sps,
            warnings.missing_pps,
            warnings.vps_out_of_range,
            warnings.sps_out_of_range,
            warnings.pps_out_of_range,
            warnings.pps_id_out_of_range,
            warning_count(warnings, HlsSegmentRepairWarningKind::InvalidVclNalu),
            warnings.nal_parse_error,
            warnings.invalid_nal,
            warnings.no_start_code,
            warnings.nal_split_error,
            warnings.packet_corrupt,
            warnings.continuity_check_failed,
            warnings.codec_parameters_missing
        ),
        RepairVideoCodec::Unsupported => debug!(
            "HLS segment repair analysis completed: session={} source={} resource={} configured_max_level={} required_level={} executed_level={} codec={} trigger={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            configured_max_level.as_log_value(),
            decision.required_level.as_log_value(),
            executed_level,
            decision.codec.as_log_value(),
            decision.trigger_source.as_log_value()
        ),
    }
}

fn debug_repair_event(
    context: &HlsSegmentRepairObjectContext,
    mode: HlsSegmentRepairMode,
    event: &'static str,
    reason: Option<&str>,
) {
    if let Some(reason) = reason {
        debug!(
            "HLS segment repair {event}: session={} source={} resource={} mode={} reason={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value(),
            reason
        );
    } else {
        debug!(
            "HLS segment repair {event}: session={} source={} resource={} mode={}",
            safe_proxy_session_id(&context.proxy_session_id),
            context.source.as_log_value(),
            context.resource_id,
            mode.as_log_value()
        );
    }
}

fn debug_repair_stream_dropped(context: &HlsSegmentRepairObjectContext, dropped: &RepairRemuxDroppedStream) {
    debug!(
        "HLS segment repair stream dropped: session={} source={} resource={} stream={} reason={}",
        safe_proxy_session_id(&context.proxy_session_id),
        context.source.as_log_value(),
        context.resource_id,
        dropped.index,
        dropped.reason
    );
}

async fn analyze_segment(path: &Path, deadline: &HlsPostProcessingDeadline) -> Result<SegmentProbe, String> {
    let probe_output = run_command_with_deadline(
        "ffprobe",
        &[
            "-hide_banner",
            "-v",
            "warning",
            "-show_entries",
            "format=duration,size,bit_rate",
            "-show_streams",
            "-of",
            "json",
            path.to_str().ok_or_else(|| "invalid_path".to_string())?,
        ],
        deadline,
    )
    .await?;
    let warnings_output = run_command_with_deadline(
        "ffmpeg",
        &[
            "-hide_banner",
            "-nostdin",
            "-v",
            "warning",
            "-i",
            path.to_str().ok_or_else(|| "invalid_path".to_string())?,
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "null",
            "-",
        ],
        deadline,
    )
    .await
    .unwrap_or_else(|stderr| stderr);
    parse_probe(&probe_output, parse_ffmpeg_warnings(&warnings_output))
}

async fn run_remux(
    input_path: &Path,
    output_path: &Path,
    mode: HlsSegmentRepairMode,
    stream_selection: &RepairRemuxStreamSelection,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    let input = input_path.to_str().ok_or_else(|| "invalid_input_path".to_string())?;
    let output = output_path.to_str().ok_or_else(|| "invalid_output_path".to_string())?;
    let mut args = ["-hide_banner", "-nostdin", "-y", "-copyts", "-i", input]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    for stream_index in &stream_selection.mapped_streams {
        args.push("-map".to_string());
        args.push(format!("0:{stream_index}"));
    }
    args.extend(["-c", "copy"].into_iter().map(ToOwned::to_owned));
    if matches!(mode, HlsSegmentRepairMode::Medium | HlsSegmentRepairMode::High) {
        args.push("-bsf:v".to_string());
        args.push("dump_extra=freq=keyframe".to_string());
    }
    args.push("-mpegts_flags".to_string());
    args.push(
        if mode == HlsSegmentRepairMode::High { "+resend_headers+pat_pmt_at_frames" } else { "+resend_headers" }
            .to_string(),
    );
    args.extend(
        ["-mpegts_copyts", "1", "-muxpreload", "0", "-muxdelay", "0", "-f", "mpegts", output]
            .into_iter()
            .map(ToOwned::to_owned),
    );
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_with_deadline("ffmpeg", &args, deadline).await.map(|_| ())
}

pub(super) async fn run_command_with_deadline(
    binary: &str,
    args: &[&str],
    deadline: &HlsPostProcessingDeadline,
) -> Result<String, String> {
    let Some(remaining) = deadline.remaining() else {
        return Err("timeout".to_string());
    };
    let output = timeout(remaining, {
        let mut command = Command::new(binary);
        command.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        command.output()
    })
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(
        |err| {
            if err.kind() == io::ErrorKind::NotFound {
                "unsupported".to_string()
            } else {
                err.to_string()
            }
        },
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { "command_failed".to_string() } else { stderr })
    }
}

fn parse_probe(json: &str, warnings: WarningCounters) -> Result<SegmentProbe, String> {
    let value = serde_json::from_str::<Value>(json).map_err(|_| "invalid_probe_json".to_string())?;
    let streams = value.get("streams").and_then(Value::as_array).ok_or_else(|| "missing_streams".to_string())?;
    let mut probe = SegmentProbe { stream_count: streams.len(), warnings, ..SegmentProbe::default() };
    if let Some(format) = value.get("format") {
        probe.duration_ms = format.get("duration").and_then(Value::as_str).and_then(parse_seconds_ms_u64);
        probe.size =
            format.get("size").and_then(Value::as_str).and_then(|value| value.parse().ok()).unwrap_or_default();
    }
    for stream in streams {
        let codec_type = stream.get("codec_type").and_then(Value::as_str);
        let codec_name = stream.get("codec_name").and_then(Value::as_str).map(ToOwned::to_owned);
        let start_time_ms = stream.get("start_time").and_then(Value::as_str).and_then(parse_seconds_ms_i64);
        let extradata_size = stream
            .get("extradata_size")
            .and_then(Value::as_u64)
            .or_else(|| stream.get("extradata_size").and_then(Value::as_str).and_then(|value| value.parse().ok()));
        let stream_index = stream
            .get("index")
            .and_then(parse_u32_value)
            .map_or(probe.streams.len(), |value| value as usize);
        let probe_stream = SegmentProbeStream {
            index: stream_index,
            stream_type: match codec_type {
                Some("video") => SegmentProbeStreamType::Video,
                Some("audio") => SegmentProbeStreamType::Audio,
                _ => SegmentProbeStreamType::Other,
            },
            codec_name: codec_name.clone(),
            width: stream.get("width").and_then(parse_u32_value),
            height: stream.get("height").and_then(parse_u32_value),
            sample_rate: stream.get("sample_rate").and_then(parse_u32_value),
            channels: stream.get("channels").and_then(parse_u32_value),
        };
        match codec_type {
            Some("video") if probe.primary_video_codec.is_none() => {
                probe.primary_video_codec = codec_name;
                probe.primary_video_start_time_ms = start_time_ms;
                probe.primary_video_extradata_size = extradata_size;
            }
            Some("audio") if probe.primary_audio_codec.is_none() => {
                probe.primary_audio_codec = codec_name;
                probe.primary_audio_start_time_ms = start_time_ms;
            }
            _ => {}
        }
        probe.streams.push(probe_stream);
    }
    Ok(probe)
}

fn parse_u32_value(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|value| u32::try_from(value).ok()).or_else(|| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "N/A")
            .and_then(|value| value.parse::<u32>().ok())
    })
}

fn detect_video_codec(probe: &SegmentProbe) -> RepairVideoCodec {
    match probe.primary_video_codec.as_deref() {
        Some("h264") => RepairVideoCodec::H264,
        Some("hevc" | "h265") => RepairVideoCodec::Hevc,
        _ => RepairVideoCodec::Unsupported,
    }
}

fn select_repair_remux_streams(probe: &SegmentProbe) -> Result<RepairRemuxStreamSelection, String> {
    let mut mapped_streams = Vec::new();
    let mut dropped_streams = Vec::new();
    let mut has_video = false;
    for stream in &probe.streams {
        match stream.stream_type {
            SegmentProbeStreamType::Video if valid_video_stream(stream) => {
                has_video = true;
                mapped_streams.push(stream.index);
            }
            SegmentProbeStreamType::Audio if valid_audio_stream(stream) => {
                mapped_streams.push(stream.index);
            }
            SegmentProbeStreamType::Video => dropped_streams.push(RepairRemuxDroppedStream {
                index: stream.index,
                reason: "invalid-video-parameters",
            }),
            SegmentProbeStreamType::Audio => dropped_streams.push(RepairRemuxDroppedStream {
                index: stream.index,
                reason: "invalid-audio-parameters",
            }),
            SegmentProbeStreamType::Other => dropped_streams.push(RepairRemuxDroppedStream {
                index: stream.index,
                reason: "unsupported-stream-type",
            }),
        }
    }
    if !has_video {
        return Err("no_valid_video_stream".to_string());
    }
    Ok(RepairRemuxStreamSelection { mapped_streams, dropped_streams })
}

fn valid_video_stream(stream: &SegmentProbeStream) -> bool {
    stream.codec_name.as_deref().is_some_and(|codec| !codec.is_empty())
        && stream.width.unwrap_or_default() > 0
        && stream.height.unwrap_or_default() > 0
}

fn valid_audio_stream(stream: &SegmentProbeStream) -> bool {
    stream.codec_name.as_deref().is_some_and(|codec| !codec.is_empty())
        && stream.sample_rate.unwrap_or_default() > 0
        && stream.channels.unwrap_or_default() > 0
}

fn validate_repair(
    config: &HlsSegmentRepairConfig,
    codec: RepairVideoCodec,
    raw: &SegmentProbe,
    fixed: &SegmentProbe,
    executed_level: HlsSegmentRepairMode,
    stream_selection: &RepairRemuxStreamSelection,
) -> Result<(), String> {
    if codec == RepairVideoCodec::Unsupported {
        return Err("unsupported_codec".to_string());
    }
    let expected_stream_count = if stream_selection.dropped_streams.is_empty() {
        raw.stream_count
    } else {
        stream_selection.mapped_streams.len()
    };
    if expected_stream_count != fixed.stream_count {
        return Err("stream_count_changed".to_string());
    }
    if raw.primary_video_codec != fixed.primary_video_codec {
        return Err("video_codec_changed".to_string());
    }
    if raw.primary_audio_codec != fixed.primary_audio_codec {
        return Err("audio_codec_changed".to_string());
    }
    if delta_u64(raw.duration_ms, fixed.duration_ms) > 250 {
        return Err("duration_delta_too_large".to_string());
    }
    if delta_i64(raw.primary_video_start_time_ms, fixed.primary_video_start_time_ms) > 250 {
        return Err("video_start_time_delta_too_large".to_string());
    }
    if delta_i64(raw.primary_audio_start_time_ms, fixed.primary_audio_start_time_ms) > 250 {
        return Err("audio_start_time_delta_too_large".to_string());
    }
    let raw_decision = decide_repair(codec, &raw.warnings);
    let fixed_decision = decide_repair(codec, &fixed.warnings);
    if raw_decision.required_level != HlsSegmentRepairMode::Off
        && fixed_decision.required_level != HlsSegmentRepairMode::Off
    {
        return Err("repair_triggers_remaining".to_string());
    }
    if raw.size > 0 {
        let allowed =
            raw.size.saturating_add(raw.size.saturating_mul(size_increase_percent(config, executed_level)) / 100);
        if fixed.size > allowed {
            return Err("size_increase_too_large".to_string());
        }
    }
    Ok(())
}

fn size_increase_percent(config: &HlsSegmentRepairConfig, level: HlsSegmentRepairMode) -> u64 {
    match level {
        HlsSegmentRepairMode::Off => 0,
        HlsSegmentRepairMode::Low => u64::from(config.size_increase.low_percent),
        HlsSegmentRepairMode::Medium => u64::from(config.size_increase.medium_percent),
        HlsSegmentRepairMode::High => u64::from(config.size_increase.high_percent),
    }
}

fn parse_seconds_ms_u64(value: &str) -> Option<u64> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.parse::<u64>().ok()?.checked_mul(1_000)?;
    let frac_ms = frac.chars().take(3).collect::<String>();
    let frac_ms = format!("{frac_ms:0<3}").parse::<u64>().ok()?;
    Some(whole_ms.saturating_add(frac_ms))
}

fn parse_seconds_ms_i64(value: &str) -> Option<i64> {
    let negative = value.starts_with('-');
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let parsed = i64::try_from(parse_seconds_ms_u64(unsigned)?).ok()?;
    Some(if negative { -parsed } else { parsed })
}

fn delta_u64(lhs: Option<u64>, rhs: Option<u64>) -> u64 {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => lhs.abs_diff(rhs),
        (None, None) => 0,
        _ => u64::MAX,
    }
}

fn delta_i64(lhs: Option<i64>, rhs: Option<i64>) -> u64 {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => lhs.abs_diff(rhs),
        (None, None) => 0,
        _ => u64::MAX,
    }
}

pub(super) async fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn ffmpeg_identity_version() -> String { "system".to_string() }

fn repair_output_path(raw_path: &Path) -> PathBuf {
    let suffix = fastrand::u64(..);
    let file_name = raw_path.file_name().and_then(|file_name| file_name.to_str()).unwrap_or("segment");
    raw_path.with_file_name(format!("{file_name}.repair.tmp.{suffix:016x}"))
}

impl fmt::Display for RepairStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clean => f.write_str("clean"),
            Self::Fixed => f.write_str("fixed"),
            Self::PolicyLimited => f.write_str("policy_limited"),
            Self::Unsupported => f.write_str("unsupported"),
            Self::Timeout => f.write_str("timeout"),
            Self::RemuxFailed => f.write_str("remux_failed"),
            Self::ValidationFailed => f.write_str("validation_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_video_codec, parse_ffmpeg_warnings, parse_probe, repair_object_metadata_key, select_repair_remux_streams,
        sha256_file, validate_repair, HlsRepairObjectMetadata, HlsRepairRenderedObjectId, HlsSegmentRepairManager,
        HlsSegmentRepairObjectContext, HlsSegmentRepairSource, RepairIdentity, RepairRemuxStreamSelection,
        RepairStatus, RepairVideoCodec, WarningCounters, REPAIR_METADATA_MAX_ENTRIES,
    };
    use crate::{
        api::model::{
            HlsAccessLeaseId, HlsSegmentCache, ProxySessionId, SegmentCacheKey, TransientObjectCacheKey,
            TransientResourceId,
        },
        model::{HlsSegmentRepairConfig, HlsSegmentRepairMode},
    };
    use std::sync::Arc;

    fn repair_config(mode: HlsSegmentRepairMode, apply_to_first_segments: u8) -> HlsSegmentRepairConfig {
        HlsSegmentRepairConfig {
            max_level: mode,
            apply_to_first_segments,
            max_parallel_repairs: 1,
            ..Default::default()
        }
    }

    fn should_repair(codec: RepairVideoCodec, warnings: &WarningCounters) -> bool {
        super::decide_repair(codec, warnings).required_level != HlsSegmentRepairMode::Off
    }

    fn repair_context(lease_id: &str, resource_id: &str) -> HlsSegmentRepairObjectContext {
        HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Normal,
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            hls_access_lease_id: Some(HlsAccessLeaseId(lease_id.to_string())),
            rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: resource_id.parse().unwrap_or(1) },
            resource_id: resource_id.to_string(),
            file_ext: "ts".to_string(),
            origin_fetch_uri_for_diagnostics: format!("http://origin.example/{resource_id}.ts"),
            media_sequence: Some(1),
            discontinuity_sequence: Some(0),
            complete_object: true,
            encrypted: false,
            custom_response: false,
        }
    }

    async fn selected_repair_mode(
        manager: &HlsSegmentRepairManager,
        context: &HlsSegmentRepairObjectContext,
    ) -> Option<HlsSegmentRepairMode> {
        manager.try_select_candidate(context).await.map(|(mode, _)| mode)
    }

    #[tokio::test]
    async fn update_config_applies_to_new_access_lease_windows() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Medium, 2));
        let mut updated = repair_config(HlsSegmentRepairMode::Medium, 3);
        updated.max_parallel_repairs = 2;
        manager.update_config(updated);

        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;

        for resource_id in ["1", "2", "3"] {
            assert!(
                manager.try_select_candidate(&repair_context("lease-a", resource_id)).await.is_some(),
                "resource {resource_id} should be inside updated repair window"
            );
        }
        assert!(
            manager.try_select_candidate(&repair_context("lease-a", "4")).await.is_none(),
            "fourth resource should be outside updated repair window"
        );
    }

    #[test]
    fn warning_parser_expands_repeated_messages() {
        let warnings = parse_ffmpeg_warnings(
            "non-existing SPS 0 referenced in buffering period\nLast message repeated 2 times\nno frame!\n",
        );

        assert_eq!(warnings.missing_sps, 3);
        assert_eq!(warnings.no_frame, 1);
    }

    #[test]
    fn mmco_warning_alone_does_not_trigger_repair() {
        let warnings = WarningCounters { mmco_unref_short_failure: 20, ..WarningCounters::default() };

        assert!(!should_repair(RepairVideoCodec::H264, &warnings));
    }

    #[test]
    fn critical_warning_triggers_repair() {
        let warnings = WarningCounters { missing_sps: 1, ..WarningCounters::default() };

        assert!(should_repair(RepairVideoCodec::H264, &warnings));
    }

    #[test]
    fn hevc_pps_warning_triggers_repair() {
        let warnings = parse_ffmpeg_warnings("[hevc @ 0x1] PPS id out of range: 0\n");

        assert_eq!(warnings.pps_id_out_of_range, 1);
        assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn hevc_invalid_slice_nalus_with_parameter_issue_trigger_repair() {
        let warnings = parse_ffmpeg_warnings(
            "[hevc @ 0x1] missing SPS\n[hevc @ 0x1] Skipping invalid undecodable NALU: 0\n[hevc @ 0x1] Skipping invalid undecodable NALU: 1\n",
        );

        assert_eq!(warnings.invalid_undecodable_nalu_total, 2);
        assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 2);
        assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn hevc_keyframe_nalu_counts_as_vcl_but_does_not_trigger_alone() {
        let warnings = parse_ffmpeg_warnings("[hevc @ 0x1] Skipping invalid undecodable NALU: 21\n");

        assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
        assert_eq!(warnings.invalid_undecodable_nalu_keyframe, 1);
        assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn hevc_metadata_and_dolby_warnings_do_not_trigger_repair_alone() {
        let warnings = parse_ffmpeg_warnings(
            "[hevc @ 0x1] Skipping invalid undecodable NALU: 39\nMultiple Dolby Vision RPUs found in one AU. Skipping previous.\nAudio/Video desynchronisation detected!\n",
        );

        assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
        assert_eq!(warnings.invalid_undecodable_nalu_metadata, 1);
        assert_eq!(warnings.dolby_vision_rpu, 1);
        assert_eq!(warnings.av_desync, 1);
        assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn hevc_repeated_messages_expand_counters_without_triggering_alone() {
        let warnings =
            parse_ffmpeg_warnings("[hevc @ 0x1] Skipping invalid undecodable NALU: 0\nLast message repeated 2 times\n");

        assert_eq!(warnings.invalid_undecodable_nalu_total, 3);
        assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 3);
        assert!(!should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn hevc_invalid_undecodable_nalu_0_to_31_counts_as_vcl_trigger_input() {
        let warnings =
            parse_ffmpeg_warnings("[hevc @ 0x1] missing SPS\n[hevc @ 0x1] Skipping invalid undecodable NALU: 30\n");

        assert_eq!(warnings.invalid_undecodable_nalu_total, 1);
        assert_eq!(warnings.invalid_undecodable_nalu_non_metadata, 1);
        assert!(should_repair(RepairVideoCodec::Hevc, &warnings));
    }

    #[test]
    fn warning_parser_matches_trigger_patterns_case_insensitively() {
        let warnings = parse_ffmpeg_warnings(
            "NON-EXISTING SPS 0 referenced\ninvalid nal unit 1\ncould not find codec parameters for stream 0\n",
        );

        assert_eq!(warnings.missing_sps, 1);
        assert_eq!(warnings.invalid_nal, 1);
        assert_eq!(warnings.codec_parameters_missing, 1);
        assert!(should_repair(RepairVideoCodec::H264, &warnings));
    }

    #[test]
    fn unsupported_codec_never_triggers_repair() {
        let warnings = WarningCounters { missing_sps: 1, pps_id_out_of_range: 1, ..WarningCounters::default() };

        assert!(!should_repair(RepairVideoCodec::Unsupported, &warnings));
    }

    #[test]
    fn parse_probe_detects_hevc_codec_and_extradata() {
        let probe = parse_probe(
            r#"{
                "streams": [
                    {
                        "index": 0,
                        "codec_type": "video",
                        "codec_name": "hevc",
                        "start_time": "1.250000",
                        "extradata_size": 96
                    }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters::default(),
        )
        .expect("probe should parse");

        assert_eq!(detect_video_codec(&probe), RepairVideoCodec::Hevc);
        assert_eq!(probe.primary_video_extradata_size, Some(96));
    }

    #[test]
    fn repair_remux_selection_drops_invalid_audio_side_stream() {
        let probe = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080 },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6 },
                    { "index": 2, "codec_type": "audio", "codec_name": "ac3", "channels": 0 }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters::default(),
        )
        .expect("probe should parse");

        let selection = select_repair_remux_streams(&probe).expect("valid video should allow remux");

        assert_eq!(selection.mapped_streams, vec![0, 1]);
        assert_eq!(selection.dropped_streams.len(), 1);
        assert_eq!(selection.dropped_streams[0].index, 2);
        assert_eq!(selection.dropped_streams[0].reason, "invalid-audio-parameters");
    }

    #[test]
    fn repair_validation_allows_configured_stream_drop() {
        let raw = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080, "start_time": "0.000000" },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6, "start_time": "0.000000" },
                    { "index": 2, "codec_type": "audio", "codec_name": "ac3", "channels": 0, "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters { codec_parameters_missing: 1, ..WarningCounters::default() },
        )
        .expect("raw probe should parse");
        let fixed = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1916, "height": 1080, "start_time": "0.000000" },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6, "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters::default(),
        )
        .expect("fixed probe should parse");
        let selection = select_repair_remux_streams(&raw).expect("raw should select valid streams");

        assert!(validate_repair(
            &repair_config(HlsSegmentRepairMode::Low, 1),
            RepairVideoCodec::Hevc,
            &raw,
            &fixed,
            HlsSegmentRepairMode::Low,
            &selection
        )
        .is_ok());
    }

    #[test]
    fn repair_remux_selection_rejects_without_valid_video() {
        let probe = parse_probe(
            r#"{
                "streams": [
                    { "index": 0, "codec_type": "video", "codec_name": "hevc", "height": 1080 },
                    { "index": 1, "codec_type": "audio", "codec_name": "ac3", "sample_rate": "48000", "channels": 6 }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters::default(),
        )
        .expect("probe should parse");

        let err = select_repair_remux_streams(&probe).expect_err("missing width should reject video");

        assert_eq!(err, "no_valid_video_stream");
    }

    #[test]
    fn hevc_validation_accepts_when_repair_triggers_are_removed() {
        let raw = parse_probe(
            r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters {
                pps_id_out_of_range: 1,
                invalid_undecodable_nalu_non_metadata: 2,
                invalid_undecodable_nalu_metadata: 1,
                dolby_vision_rpu: 1,
                ..WarningCounters::default()
            },
        )
        .expect("raw probe should parse");
        let fixed = parse_probe(
            r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1010" }
            }"#,
            WarningCounters { invalid_undecodable_nalu_metadata: 1, dolby_vision_rpu: 1, ..WarningCounters::default() },
        )
        .expect("fixed probe should parse");

        assert!(validate_repair(
            &repair_config(HlsSegmentRepairMode::Medium, 1),
            RepairVideoCodec::Hevc,
            &raw,
            &fixed,
            HlsSegmentRepairMode::Medium,
            &RepairRemuxStreamSelection::preserve_all(&raw)
        )
        .is_ok());
    }

    #[test]
    fn validation_rejects_remaining_repair_triggers_even_when_level_improves() {
        let raw = parse_probe(
            r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters { codec_parameters_missing: 1, missing_sps: 1, ..WarningCounters::default() },
        )
        .expect("raw probe should parse");
        let fixed = parse_probe(
            r#"{
                "streams": [
                    { "codec_type": "video", "codec_name": "hevc", "start_time": "0.000000" },
                    { "codec_type": "audio", "codec_name": "aac", "start_time": "0.000000" }
                ],
                "format": { "duration": "2.000000", "size": "1000" }
            }"#,
            WarningCounters { missing_sps: 1, ..WarningCounters::default() },
        )
        .expect("fixed probe should parse");

        let err = validate_repair(
            &repair_config(HlsSegmentRepairMode::High, 1),
            RepairVideoCodec::Hevc,
            &raw,
            &fixed,
            HlsSegmentRepairMode::High,
            &RepairRemuxStreamSelection::preserve_all(&raw),
        )
        .expect_err("remaining medium trigger should fail validation");
        assert_eq!(err, "repair_triggers_remaining");
    }

    #[test]
    fn configured_max_below_required_skips_repair() {
        assert_eq!(
            HlsSegmentRepairMode::Low.execution_plan(HlsSegmentRepairMode::High),
            super::HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired
        );
        assert_eq!(
            HlsSegmentRepairMode::Medium.execution_plan(HlsSegmentRepairMode::Low),
            super::HlsSegmentRepairExecutionPlan::Repair(HlsSegmentRepairMode::Low)
        );
        assert_eq!(
            HlsSegmentRepairMode::Off.execution_plan(HlsSegmentRepairMode::High),
            super::HlsSegmentRepairExecutionPlan::SkipNoTrigger
        );
    }

    #[tokio::test]
    async fn repair_window_selects_first_unique_segments_per_access_lease() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let first = repair_context("lease-a", "000001");
        let second = repair_context("lease-a", "000002");

        assert_eq!(selected_repair_mode(&manager, &first).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(selected_repair_mode(&manager, &first).await, None);
        assert_eq!(selected_repair_mode(&manager, &second).await, None);
    }

    #[tokio::test]
    async fn background_candidate_without_access_lease_is_ignored_before_window_check() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let mut context = repair_context("lease-a", "000001");
        context.hls_access_lease_id = None;

        assert_eq!(selected_repair_mode(&manager, &context).await, None);

        let stats = manager.stats().await;
        assert_eq!(stats.windows, 1);
        assert_eq!(stats.checked_candidates, 0);
        assert_eq!(stats.object_metadata, 0);
    }

    #[tokio::test]
    async fn repair_window_is_separate_per_access_lease() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Medium, 1));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;

        assert_eq!(
            selected_repair_mode(&manager, &repair_context("lease-a", "000001")).await,
            Some(HlsSegmentRepairMode::Medium)
        );
        assert_eq!(
            selected_repair_mode(&manager, &repair_context("lease-b", "000001")).await,
            Some(HlsSegmentRepairMode::Medium)
        );
    }

    #[tokio::test]
    async fn known_normal_object_consumes_new_lease_window_without_rescan() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let cache_key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 1, "ts");
        let metadata = cache.write_bytes_and_commit(&cache_key, b"cached-normal-bytes").await.expect("commit");
        let committed_sha256 = sha256_file(&metadata.path).await.expect("hash");
        let previous_context = repair_context("lease-a", "1");
        manager
            .record_object_metadata(
                repair_object_metadata_key(&previous_context, HlsSegmentRepairMode::Low),
                HlsRepairObjectMetadata {
                    committed_sha256,
                    raw_sha256: Some("previous-raw".to_string()),
                    status: RepairStatus::Clean,
                    raw_size: metadata.size,
                    final_size: metadata.size,
                    validation_reason: None,
                },
            )
            .await;

        manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;

        assert!(manager
            .repair_ready_cache_hit(&cache, &cache_key, repair_context("lease-b", "1"))
            .await
            .expect("repair cache hit")
            .is_none());
        assert_eq!(manager.stats().await.metadata, 0);
        assert_eq!(selected_repair_mode(&manager, &repair_context("lease-b", "2")).await, None);
    }

    #[tokio::test]
    async fn known_transient_object_consumes_new_lease_window_without_rescan() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let cache_key = TransientObjectCacheKey::new(
            ProxySessionId("proxy-session".to_string()),
            TransientResourceId("resource-a".to_string()),
            "ts",
        );
        let metadata = cache.write_bytes_and_commit(&cache_key, b"cached-transient-bytes").await.expect("commit");
        let committed_sha256 = sha256_file(&metadata.path).await.expect("hash");
        let mut previous_context = repair_context("lease-a", "1");
        previous_context.source = HlsSegmentRepairSource::Transient;
        previous_context.rendered_object_id =
            HlsRepairRenderedObjectId::Transient { resource_id: "resource-a".to_string() };
        previous_context.resource_id = "resource-a".to_string();
        manager
            .record_object_metadata(
                repair_object_metadata_key(&previous_context, HlsSegmentRepairMode::Low),
                HlsRepairObjectMetadata {
                    committed_sha256,
                    raw_sha256: Some("previous-raw".to_string()),
                    status: RepairStatus::Clean,
                    raw_size: metadata.size,
                    final_size: metadata.size,
                    validation_reason: None,
                },
            )
            .await;

        manager.start_access_lease_window(HlsAccessLeaseId("lease-b".to_string())).await;
        let mut current_context = previous_context.clone();
        current_context.hls_access_lease_id = Some(HlsAccessLeaseId("lease-b".to_string()));

        assert!(manager
            .repair_ready_cache_hit(&cache, &cache_key, current_context)
            .await
            .expect("repair cache hit")
            .is_none());
        assert_eq!(manager.stats().await.metadata, 0);
        let mut second_context = repair_context("lease-b", "2");
        second_context.source = HlsSegmentRepairSource::Transient;
        second_context.rendered_object_id =
            HlsRepairRenderedObjectId::Transient { resource_id: "resource-b".to_string() };
        second_context.resource_id = "resource-b".to_string();
        assert_eq!(selected_repair_mode(&manager, &second_context).await, None);
    }

    #[tokio::test]
    async fn object_metadata_hash_mismatch_does_not_skip_repair_evaluation() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let context = repair_context("lease-a", "1");
        let object_key = repair_object_metadata_key(&context, HlsSegmentRepairMode::Low);
        manager
            .record_object_metadata(
                object_key.clone(),
                HlsRepairObjectMetadata {
                    committed_sha256: "old-hash".to_string(),
                    raw_sha256: Some("old-raw".to_string()),
                    status: RepairStatus::Clean,
                    raw_size: 1,
                    final_size: 1,
                    validation_reason: None,
                },
            )
            .await;

        assert!(!manager.object_metadata_matches(&object_key, "new-hash").await);
    }

    #[test]
    fn repair_object_metadata_key_ignores_origin_fetch_uri_for_diagnostics() {
        let mut first = repair_context("lease-a", "1");
        first.origin_fetch_uri_for_diagnostics = "http://mirror-a.example/live/1.ts".to_string();
        let mut second = first.clone();
        second.origin_fetch_uri_for_diagnostics = "http://redirect-b.example/cdn/path/1.ts".to_string();

        assert_eq!(
            repair_object_metadata_key(&first, HlsSegmentRepairMode::Low),
            repair_object_metadata_key(&second, HlsSegmentRepairMode::Low)
        );
    }

    #[tokio::test]
    async fn repair_window_candidate_key_ignores_origin_fetch_uri_for_diagnostics() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let mut first = repair_context("lease-a", "1");
        first.origin_fetch_uri_for_diagnostics = "http://mirror-a.example/live/1.ts".to_string();
        let mut same_rendered_object_other_fetch_uri = first.clone();
        same_rendered_object_other_fetch_uri.origin_fetch_uri_for_diagnostics =
            "http://redirect-b.example/cdn/path/1.ts".to_string();
        let second_rendered_object = repair_context("lease-a", "2");

        assert_eq!(selected_repair_mode(&manager, &first).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(selected_repair_mode(&manager, &same_rendered_object_other_fetch_uri).await, None);
        assert_eq!(
            selected_repair_mode(&manager, &second_rendered_object).await,
            Some(HlsSegmentRepairMode::Low)
        );
    }

    #[tokio::test]
    async fn repair_disabled_does_not_track_candidates() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Off, 1));
        let context = repair_context("lease-a", "000001");

        assert_eq!(selected_repair_mode(&manager, &context).await, None);

        let registry = manager.windows.read().await;
        assert_eq!(registry.checked_candidates.len(), 0);
    }

    #[tokio::test]
    async fn transient_commit_and_cache_hit_consume_repair_window_once() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let mut commit_context = repair_context("lease-a", "1");
        commit_context.source = HlsSegmentRepairSource::Transient;
        commit_context.rendered_object_id =
            HlsRepairRenderedObjectId::Transient { resource_id: "resource-a".to_string() };
        commit_context.resource_id = "resource-a".to_string();
        commit_context.origin_fetch_uri_for_diagnostics = "http://origin.example/live/resource-a.ts".to_string();
        commit_context.media_sequence = None;
        commit_context.discontinuity_sequence = None;

        let mut cache_hit_context = commit_context.clone();
        cache_hit_context.origin_fetch_uri_for_diagnostics = "resource-a".to_string();

        let mut second_context = commit_context.clone();
        second_context.rendered_object_id =
            HlsRepairRenderedObjectId::Transient { resource_id: "resource-b".to_string() };
        second_context.resource_id = "resource-b".to_string();
        second_context.origin_fetch_uri_for_diagnostics = "resource-b".to_string();

        assert_eq!(selected_repair_mode(&manager, &commit_context).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(selected_repair_mode(&manager, &cache_hit_context).await, None);
        assert_eq!(selected_repair_mode(&manager, &second_context).await, Some(HlsSegmentRepairMode::Low));
    }

    #[tokio::test]
    async fn normal_commit_and_cache_hit_consume_repair_window_once() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 2));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let commit_context = repair_context("lease-a", "1");
        let mut cache_hit_context = commit_context.clone();
        cache_hit_context.origin_fetch_uri_for_diagnostics = "http://redirect.example/other-path.ts".to_string();
        cache_hit_context.media_sequence = Some(99);
        cache_hit_context.discontinuity_sequence = Some(7);
        let second_context = repair_context("lease-a", "2");

        assert_eq!(selected_repair_mode(&manager, &commit_context).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(selected_repair_mode(&manager, &cache_hit_context).await, None);
        assert_eq!(selected_repair_mode(&manager, &second_context).await, Some(HlsSegmentRepairMode::Low));
    }

    #[tokio::test]
    async fn non_repairable_objects_do_not_consume_repair_window() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        manager.start_access_lease_window(HlsAccessLeaseId("lease-a".to_string())).await;
        let mut partial = repair_context("lease-a", "1");
        partial.complete_object = false;
        let repairable = repair_context("lease-a", "2");

        assert_eq!(selected_repair_mode(&manager, &partial).await, None);
        assert_eq!(selected_repair_mode(&manager, &repairable).await, Some(HlsSegmentRepairMode::Low));
    }

    #[tokio::test]
    async fn new_repair_window_generation_allows_rechecking_candidate() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let context = repair_context("lease-a", "000001");

        manager.start_access_lease_window(lease_id.clone()).await;
        assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

        manager.start_access_lease_window(lease_id).await;

        assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(manager.windows.read().await.checked_candidates.len(), 2);
    }

    #[tokio::test]
    async fn remove_access_lease_window_clears_window_and_generation() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.start_access_lease_window(lease_id.clone()).await;

        let before = manager.stats().await;
        assert_eq!(before.windows, 1);
        assert_eq!(before.generations, 1);

        manager.remove_access_lease_window(&lease_id).await;

        let after = manager.stats().await;
        assert_eq!(after.windows, 0);
        assert_eq!(after.generations, 0);
    }

    #[tokio::test]
    async fn remove_access_lease_window_clears_checked_candidates() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let lease_id = HlsAccessLeaseId("lease-a".to_string());

        manager.start_access_lease_window(lease_id.clone()).await;
        assert_eq!(
            selected_repair_mode(&manager, &repair_context("lease-a", "000001")).await,
            Some(HlsSegmentRepairMode::Low)
        );
        assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

        manager.remove_access_lease_window(&lease_id).await;

        assert_eq!(manager.windows.read().await.checked_candidates.len(), 0);
    }

    #[tokio::test]
    async fn remove_proxy_session_state_clears_checked_candidates() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        manager.start_access_lease_window(lease_id.clone()).await;
        let context = repair_context("lease-a", "000001");

        assert_eq!(selected_repair_mode(&manager, &context).await, Some(HlsSegmentRepairMode::Low));
        assert_eq!(manager.windows.read().await.checked_candidates.len(), 1);

        manager.remove_proxy_session_state(&proxy_session_id, &[lease_id]).await;

        assert_eq!(manager.windows.read().await.checked_candidates.len(), 0);
    }

    #[tokio::test]
    async fn remove_proxy_session_state_clears_object_metadata() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        let context = repair_context("lease-a", "000001");
        manager
            .record_object_metadata(
                repair_object_metadata_key(&context, HlsSegmentRepairMode::Low),
                HlsRepairObjectMetadata {
                    committed_sha256: "hash".to_string(),
                    raw_sha256: Some("raw".to_string()),
                    status: RepairStatus::Clean,
                    raw_size: 1,
                    final_size: 1,
                    validation_reason: None,
                },
            )
            .await;

        assert_eq!(manager.stats().await.object_metadata, 1);

        manager.remove_proxy_session_state(&proxy_session_id, &[lease_id]).await;

        assert_eq!(manager.stats().await.object_metadata, 0);
    }

    #[tokio::test]
    async fn repair_metadata_is_bounded() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));

        for index in 0..REPAIR_METADATA_MAX_ENTRIES + 5 {
            manager
                .record_metadata(
                    RepairIdentity {
                        raw_sha256: format!("{index:064x}"),
                        repair_mode: HlsSegmentRepairMode::Low,
                        command_version: 1,
                        ffmpeg_version: "test".to_string(),
                    },
                    RepairStatus::Clean,
                    1,
                    1,
                    None,
                )
                .await;
        }

        assert_eq!(manager.stats().await.metadata, REPAIR_METADATA_MAX_ENTRIES);
    }

    #[tokio::test]
    async fn repair_lock_cleanup_keeps_waited_lock_and_removes_unused_lock() {
        let manager = HlsSegmentRepairManager::new(repair_config(HlsSegmentRepairMode::Low, 1));
        let identity = RepairIdentity {
            raw_sha256: "a".repeat(64),
            repair_mode: HlsSegmentRepairMode::Low,
            command_version: 1,
            ffmpeg_version: "test".to_string(),
        };
        let lock = manager.lock_for_identity(identity.clone()).await;
        let waiter = Arc::clone(&lock);

        manager.remove_lock_if_unused(&identity, &lock).await;
        assert_eq!(manager.stats().await.locks, 1);

        drop(waiter);
        manager.remove_lock_if_unused(&identity, &lock).await;
        assert_eq!(manager.stats().await.locks, 0);
    }

    #[test]
    fn repair_context_excludes_non_finite_origin_ts_objects() {
        let mut context = repair_context("lease-a", "000001");

        context.complete_object = false;
        assert!(!context.is_repairable_ts());

        context.complete_object = true;
        context.encrypted = true;
        assert!(!context.is_repairable_ts());

        context.encrypted = false;
        context.custom_response = true;
        assert!(!context.is_repairable_ts());

        context.custom_response = false;
        context.file_ext = "m4s".to_string();
        assert!(!context.is_repairable_ts());
    }
}
