#![allow(clippy::large_futures, clippy::large_enum_variant)]

use super::{
    hls_client_body_send_deadline, refresh_hls_client_body_send_deadline, safe_hls_access_lease_id,
    safe_proxy_session_id, CacheAccessState, HlsAccessLeaseId, HlsCacheMetrics, HlsLogIdentity, HlsMapFile,
    HlsMediaActivityCommitOutcome, HlsMediaLeaseIdentity, HlsPlaybackRequestToken, HlsProxyManager,
    HlsRepairRenderedObjectId, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, HlsStartupBodyObservation, MapCacheKey, MapCacheStatus, ProxyMapId,
    ProxySessionId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey, TransientResourceFile,
    TransientResourceKind,
};
use arc_swap::ArcSwapOption;
use axum::http::StatusCode;
use bytes::Bytes;
use futures::Stream;
use std::{
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, LazyLock,
    },
    time::Instant,
};
use tokio::{sync::Semaphore, time::Sleep};
use tuliprox_session::StreamMeterHandle;

const ACCEPT_RANGES_VALUE: &str = "bytes";

const NOT_READY_RETRY_AFTER_MS: u64 = 1_000;

const BODY_READER_WAIT_LOG_THRESHOLD_MS: u128 = 10;

const PREPARED_MEDIA_CHUNK_SIZE: usize = 4 * 1_024;

static NEXT_HLS_BODY_LOG_ID: AtomicU64 = AtomicU64::new(1);

const HLS_MEDIA_ACTIVITY_TASK_CAPACITY: usize = 256;

static HLS_MEDIA_ACTIVITY_TASK_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(HLS_MEDIA_ACTIVITY_TASK_CAPACITY)));

struct FiniteBytesSelection {
    status: StatusCode,
    body: Bytes,
    content_length: u64,
    content_range: Option<String>,
}

#[derive(Clone)]
struct CacheObject<K> {
    is_media: bool,
    key: K,
    access: Arc<CacheAccessState>,
    content_type: String,
    log_context: CacheObjectLogContext,
    repair_context: Option<HlsSegmentRepairObjectContext>,
}

#[derive(Clone)]
struct CacheObjectLogContext {
    lease: String,
    identity: HlsLogIdentity,
    resource_id: String,
    object_kind: &'static str,
    body_source: &'static str,
}

#[derive(Clone)]
struct CacheBodyLogContext {
    body_id: String,
    identity: HlsLogIdentity,
    resource_id: String,
    object_kind: &'static str,
    source: &'static str,
    content_length: u64,
}

pub struct HlsCacheResponseContext {
    pub hls_access_lease_id: HlsAccessLeaseId,
    log_identity: HlsLogIdentity,
    pub cache_duration_seconds: u64,
    pub metrics: Arc<HlsCacheMetrics>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub qos_meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    pub media_activity_marker: Option<HlsMediaActivityMarker>,
    pub now_ms: u64,
}

#[derive(Clone)]
pub struct HlsMediaActivityMarker {
    manager: Arc<HlsProxyManager>,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    lease_identity: HlsMediaLeaseIdentity,
    completed_segment: Option<HlsPlaybackRequestToken>,
    completion_scheduled: Option<Arc<AtomicBool>>,
    active_provider: Option<Arc<tuliprox_session::ActiveProviderManager>>,
    session_owner: Option<String>,
    playback_request_id: Option<tuliprox_core::model::PlaybackRequestId>,
}

struct CacheObjectServeContext {
    cache_duration_seconds: u64,
    metrics: Option<Arc<HlsCacheMetrics>>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    qos_meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    media_activity_marker: Option<HlsMediaActivityMarker>,
    playback_cursor_tracking: HlsPlaybackCursorTracking,
    now_ms: u64,
}

struct ActiveReaderStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    _guard: Option<CacheReadGuard>,
    context: CacheBodyLogContext,
    started_at: Instant,
    last_yield_at: Instant,
    send_deadline: Pin<Box<Sleep>>,
    max_idle_ms: u128,
    completed_logged: bool,
    finished: bool,
    bytes_yielded: u64,
    meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    media_activity_marker: Option<HlsMediaActivityMarker>,
    startup_body_observation: Option<HlsStartupBodyObservation>,
}

struct PreparedBytesStream {
    remaining: Bytes,
}

struct CacheReadGuard {
    access: Arc<CacheAccessState>,
}

struct RevisionResponseStream {
    inner: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>,
    qos_meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    /// Marks the segment completed once the body ends without an error.
    completion: Option<HlsMediaActivityMarker>,
    observation: Option<HlsStartupBodyObservation>,
    first_chunk: bool,
    finished: bool,
    idle_deadline: Pin<Box<Sleep>>,
}

#[cfg(test)]
mod tests;

mod activity;
mod cache;
mod finite;
mod reader;
mod revision;

#[cfg(test)]
use activity::spawn_bounded_media_completion;
use cache::HlsPlaybackCursorTracking;
#[cfg(test)]
use cache::{hls_resource_failure_default_response, serve_cache_object};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use cache::{
    serve_hls_map_cache_outcome, serve_hls_segment_cache_outcome, serve_hls_transient_object_cache_outcome,
    serve_hls_transient_object_cache_response, HlsResourceServeFailure, HlsResourceServeOutcome,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use finite::{
    finite_hls_immutable_media_response, finite_hls_media_head_response, finite_hls_media_response,
    finite_hls_terminal_key_response,
};
