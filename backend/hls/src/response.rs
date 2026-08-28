#![allow(clippy::large_futures, clippy::large_enum_variant, clippy::too_many_lines)]

use super::{
    hls_client_body_send_deadline, refresh_hls_client_body_send_deadline, safe_hls_access_lease_id,
    safe_proxy_session_id, CacheAccessState, HlsAccessLeaseId, HlsCacheMetrics, HlsLogIdentity, HlsMapFile,
    HlsMediaActivityCommitOutcome, HlsMediaLeaseIdentity, HlsPlaybackRequestToken, HlsProxyManager,
    HlsRepairRenderedObjectId, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, HlsStartupBodyObservation, MapCacheKey, MapCacheStatus, ProtectedSet,
    ProxyMapId, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey, TransientResourceFile,
    TransientResourceKind,
};
use arc_swap::ArcSwapOption;
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::Stream;
use log::debug;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::Semaphore,
    time::{sleep, Sleep},
};
use tokio_util::io::ReaderStream;
use tuliprox_core::utils::{
    byte_range::{resolve_single_byte_range, SingleByteRange},
    response_compression::mark_response_as_uncompressed,
};
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

fn select_finite_bytes(bytes: Bytes, range_header: Option<&HeaderValue>) -> Result<FiniteBytesSelection, u64> {
    let full_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    match resolve_single_byte_range(range_header, full_size) {
        SingleByteRange::Full => Ok(FiniteBytesSelection {
            status: StatusCode::OK,
            content_length: full_size,
            body: bytes,
            content_range: None,
        }),
        SingleByteRange::Partial { start, end, length } => {
            let start_usize = usize::try_from(start).unwrap_or(bytes.len());
            let end_exclusive = usize::try_from(end.saturating_add(1)).unwrap_or(bytes.len()).min(bytes.len());
            Ok(FiniteBytesSelection {
                status: StatusCode::PARTIAL_CONTENT,
                body: bytes.slice(start_usize.min(end_exclusive)..end_exclusive),
                content_length: length,
                content_range: Some(format!("bytes {start}-{end}/{full_size}")),
            })
        }
        SingleByteRange::Unsatisfiable => Err(full_size),
    }
}

fn build_finite_bytes_response(
    selection: &FiniteBytesSelection,
    body: Body,
    content_type: &str,
    cache_control: &'static str,
) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = selection.status;
    let headers = response.headers_mut();
    insert_header_value(headers, header::CONTENT_TYPE, content_type);
    insert_header_value(headers, header::ACCEPT_RANGES, ACCEPT_RANGES_VALUE);
    insert_header_value(headers, header::CACHE_CONTROL, cache_control);
    insert_u64_header(headers, header::CONTENT_LENGTH, selection.content_length);
    if let Some(content_range) = selection.content_range.as_deref() {
        insert_header_value(headers, header::CONTENT_RANGE, content_range);
    }
    mark_response_as_uncompressed(&mut response);
    response
}

/// Serves immutable prepared bytes without touching the live-lease state.
///
/// The route is the authorization boundary; callers do not get any
/// upstream origin work on top of the response.
pub fn finite_hls_immutable_media_response(
    bytes: Bytes,
    range_header: Option<&HeaderValue>,
    content_type: &'static str,
    cache_control: &'static str,
    head_only: bool,
) -> Response<Body> {
    let selection = match select_finite_bytes(bytes, range_header) {
        Ok(selection) => selection,
        Err(full_size) => return range_not_satisfiable_response(full_size),
    };
    let body = if head_only { Body::empty() } else { Body::from(selection.body.clone()) };
    build_finite_bytes_response(&selection, body, content_type, cache_control)
}

/// Serves immutable in-memory media through the same `QoS`, completion, drop and
/// client-send-deadline stream used by disk-backed Shared-HLS media.
pub fn finite_hls_media_response(
    bytes: Bytes,
    range_header: Option<&HeaderValue>,
    content_type: &'static str,
    cache_control: &'static str,
    context: &HlsCacheResponseContext,
    _proxy_session_id: &ProxySessionId,
    resource_id: String,
) -> Response<Body> {
    let selection = match select_finite_bytes(bytes, range_header) {
        Ok(selection) => selection,
        Err(full_size) => return range_not_satisfiable_response(full_size),
    };
    context.metrics.record_cache_hit();
    if selection.status == StatusCode::PARTIAL_CONTENT {
        context.metrics.record_cache_range_hit();
    }
    let body_context = CacheBodyLogContext {
        body_id: next_hls_body_log_id(),
        identity: context.log_identity.clone(),
        resource_id,
        object_kind: "TerminalSegment",
        source: "prepared",
        content_length: selection.content_length,
    };
    let stream = PreparedBytesStream::new(selection.body.clone());
    let stream = ActiveReaderStream::new(
        Box::pin(stream),
        None,
        body_context,
        Arc::clone(&context.qos_meter),
        context.media_activity_marker.as_ref().and_then(HlsMediaActivityMarker::completed_segment_marker),
        None,
    );
    build_finite_bytes_response(&selection, Body::from_stream(stream), content_type, cache_control)
}

/// Serves one frozen lease-bound AES key revision without consulting mutable
/// transient-resource state or performing origin I/O.
pub fn finite_hls_terminal_key_response(
    bytes: Bytes,
    range_header: Option<&HeaderValue>,
    content_type: &str,
    cache_control: &'static str,
    context: &HlsCacheResponseContext,
    _proxy_session_id: &ProxySessionId,
    resource_id: String,
) -> Response<Body> {
    let selection = match select_finite_bytes(bytes, range_header) {
        Ok(selection) => selection,
        Err(full_size) => return range_not_satisfiable_response(full_size),
    };
    context.metrics.record_cache_hit();
    if selection.status == StatusCode::PARTIAL_CONTENT {
        context.metrics.record_cache_range_hit();
    }
    let body_context = CacheBodyLogContext {
        body_id: next_hls_body_log_id(),
        identity: context.log_identity.clone(),
        resource_id,
        object_kind: "TerminalKey",
        source: "prepared",
        content_length: selection.content_length,
    };
    let stream = PreparedBytesStream::new(selection.body.clone());
    let stream = ActiveReaderStream::new(
        Box::pin(stream),
        None,
        body_context,
        Arc::clone(&context.qos_meter),
        context.media_activity_marker.as_ref().and_then(HlsMediaActivityMarker::completed_segment_marker),
        None,
    );
    build_finite_bytes_response(&selection, Body::from_stream(stream), content_type, cache_control)
}

/// Builds terminal-media HEAD metadata without rendering bytes or creating a
/// reader, `QoS` stream, completion callback, or media-activity side effect.
pub fn finite_hls_media_head_response(
    full_size: u64,
    range_header: Option<&HeaderValue>,
    content_type: &'static str,
    cache_control: &'static str,
) -> Response<Body> {
    let selection = match resolve_single_byte_range(range_header, full_size) {
        SingleByteRange::Full => FiniteBytesSelection {
            status: StatusCode::OK,
            body: Bytes::new(),
            content_length: full_size,
            content_range: None,
        },
        SingleByteRange::Partial { start, end, length } => FiniteBytesSelection {
            status: StatusCode::PARTIAL_CONTENT,
            body: Bytes::new(),
            content_length: length,
            content_range: Some(format!("bytes {start}-{end}/{full_size}")),
        },
        SingleByteRange::Unsatisfiable => return range_not_satisfiable_response(full_size),
    };
    build_finite_bytes_response(&selection, Body::empty(), content_type, cache_control)
}

#[derive(Clone)]
struct CacheObject<K> {
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

enum CacheObjectLookup<K> {
    Ready(CacheObject<K>),
    Failure(HlsResourceServeFailure),
}

/// Result of a Shared-HLS cache object serve decision.
pub enum HlsResourceServeOutcome {
    Ready(Response<Body>),
    Failure(HlsResourceServeFailure),
}

/// Typed Shared-HLS resource failure before endpoint-level custom-response mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsResourceServeFailure {
    TemporaryUnavailable { retry_after_ms: u64 },
    Missing,
    Expired,
    PermanentFailed { status: Option<StatusCode> },
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

impl HlsCacheResponseContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hls_access_lease_id: HlsAccessLeaseId,
        log_identity: HlsLogIdentity,
        cache_duration_seconds: u64,
        metrics: Arc<HlsCacheMetrics>,
        segment_repair: Arc<HlsSegmentRepairManager>,
        qos_meter: Option<Arc<StreamMeterHandle>>,
        media_activity_marker: Option<HlsMediaActivityMarker>,
        now_ms: u64,
    ) -> Self {
        Self {
            hls_access_lease_id,
            log_identity,
            cache_duration_seconds,
            metrics,
            segment_repair,
            qos_meter: Arc::new(ArcSwapOption::from(qos_meter)),
            media_activity_marker,
            now_ms,
        }
    }

    pub fn set_qos_meter(&self, qos_meter: Option<Arc<StreamMeterHandle>>) {
        self.qos_meter.store(qos_meter);
    }

    pub async fn mark_media_activity(&self) {
        if let Some(marker) = &self.media_activity_marker {
            marker.mark_at(self.now_ms).await;
        }
    }
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
}

impl HlsMediaActivityMarker {
    pub fn new(
        manager: Arc<HlsProxyManager>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        lease_identity: HlsMediaLeaseIdentity,
    ) -> Self {
        Self {
            manager,
            session,
            proxy_session_id,
            lease_id,
            lease_identity,
            completed_segment: None,
            completion_scheduled: None,
        }
    }

    async fn for_segment_request(mut self, proxy_seq: u64, requested_at_ms: u64) -> Option<Self> {
        // A terminal lease may still read the immutable READY live-tail segment
        // protected by its plan. That read marks terminal activity, but it must
        // never mutate the live playback cursor.
        if !self.lease_identity.is_live() {
            return Some(self);
        }
        self.completed_segment = self
            .manager
            .record_access_lease_segment_request_started_if_identity_matches(
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                proxy_seq,
                requested_at_ms,
            )
            .await;
        let _ = self.completed_segment.as_ref()?;
        self.completion_scheduled = Some(Arc::new(AtomicBool::new(false)));
        Some(self)
    }

    pub async fn mark_at(&self, now_ms: u64) {
        let outcome = self
            .manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &self.session,
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                now_ms,
            )
            .await;
        self.log_uncommitted_activity(outcome, "access");
    }

    async fn mark_completion_at(&self, now_ms: u64) {
        let Some(token) = self.completed_segment else {
            self.mark_at(now_ms).await;
            return;
        };
        let outcome = self
            .manager
            .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                &self.session,
                &self.lease_id,
                &self.proxy_session_id,
                self.lease_identity,
                token,
                now_ms,
            )
            .await;
        self.log_uncommitted_activity(outcome, "live-segment-completion");
    }

    fn spawn_mark_completion_now(&self) {
        let Some(scheduled) = self.completion_scheduled.as_ref() else {
            debug!(
                "HLS media completion ignored: lease={} proxy_session={} reason=missing-completion-token",
                safe_hls_access_lease_id(&self.lease_id),
                safe_proxy_session_id(&self.proxy_session_id)
            );
            return;
        };
        if scheduled.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            debug!(
                "HLS media completion ignored: lease={} proxy_session={} reason=completion-already-scheduled",
                safe_hls_access_lease_id(&self.lease_id),
                safe_proxy_session_id(&self.proxy_session_id)
            );
            return;
        }
        let marker = self.clone();
        spawn_bounded_media_completion(Arc::clone(&HLS_MEDIA_ACTIVITY_TASK_PERMITS), async move {
            marker.mark_completion_at(current_time_millis()).await;
        });
    }

    fn completed_segment_marker(&self) -> Option<Self> {
        self.completed_segment.map(|_| self.clone())
    }

    fn record_startup_segment_request(&self, proxy_seq: u64, now_ms: u64) {
        self.manager.startup_observability().record_first_visible_segment_request(&self.lease_id, proxy_seq, now_ms);
    }

    fn record_startup_repair_decision(&self, proxy_seq: u64, now_ms: u64) {
        self.manager.startup_observability().record_repair_decision(&self.lease_id, proxy_seq, now_ms);
    }

    fn begin_startup_cache_response(
        &self,
        proxy_seq: u64,
        body_id: &str,
        now_ms: u64,
    ) -> Option<HlsStartupBodyObservation> {
        self.manager.startup_observability().begin_cache_response(&self.lease_id, proxy_seq, body_id, now_ms)
    }

    fn log_uncommitted_activity(&self, outcome: HlsMediaActivityCommitOutcome, phase: &'static str) {
        let reason = match outcome {
            HlsMediaActivityCommitOutcome::Committed => return,
            HlsMediaActivityCommitOutcome::StaleLeaseIdentity => "expired-or-playback-generation-race",
            HlsMediaActivityCommitOutcome::DeferredLockContention => "lock-contention",
        };
        debug!(
            "HLS media activity ignored: phase={phase} lease={} proxy_session={} reason={reason}",
            safe_hls_access_lease_id(&self.lease_id),
            safe_proxy_session_id(&self.proxy_session_id)
        );
    }
}

fn spawn_bounded_media_completion(
    completion_permits: Arc<Semaphore>,
    completion: impl Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        let Ok(_permit) = completion_permits.acquire_owned().await else {
            return;
        };
        completion.await;
    });
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsPlaybackCursorTracking {
    Disabled,
    Segment { proxy_seq: u64 },
}

impl HlsPlaybackCursorTracking {
    fn full_object_proxy_seq(self, range: SingleByteRange, full_size: u64) -> Option<u64> {
        match self {
            Self::Segment { proxy_seq } if resolved_range_covers_full_object(range, full_size) => Some(proxy_seq),
            Self::Disabled | Self::Segment { .. } => None,
        }
    }
}

fn resolved_range_covers_full_object(range: SingleByteRange, full_size: u64) -> bool {
    match range {
        SingleByteRange::Full => true,
        SingleByteRange::Partial { start, end, length } => {
            start == 0 && full_size.checked_sub(1) == Some(end) && length == full_size
        }
        SingleByteRange::Unsatisfiable => false,
    }
}

impl CacheObjectServeContext {
    fn from_response_context(context: &HlsCacheResponseContext) -> Self {
        Self {
            cache_duration_seconds: context.cache_duration_seconds,
            metrics: Some(Arc::clone(&context.metrics)),
            segment_repair: Arc::clone(&context.segment_repair),
            qos_meter: Arc::clone(&context.qos_meter),
            media_activity_marker: context.media_activity_marker.clone(),
            playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
            now_ms: context.now_ms,
        }
    }
}

/// Serves a committed Ready segment or returns a typed failure for endpoint-level mapping.
pub async fn serve_hls_segment_cache_outcome(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    segment_file: HlsSegmentFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> HlsResourceServeOutcome {
    match lookup_segment_cache_object(&session, &segment_file, &context.hls_access_lease_id).await {
        CacheObjectLookup::Ready(object) => cache_object_serve_outcome(
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext {
                    playback_cursor_tracking: HlsPlaybackCursorTracking::Segment { proxy_seq: segment_file.proxy_seq },
                    ..CacheObjectServeContext::from_response_context(context)
                },
            )
            .await,
        ),
        CacheObjectLookup::Failure(failure) => HlsResourceServeOutcome::Failure(failure),
    }
}

/// Serves a committed Ready EXT-X-MAP object or returns a typed failure for endpoint-level mapping.
pub async fn serve_hls_map_cache_outcome(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    map_file: HlsMapFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> HlsResourceServeOutcome {
    match lookup_map_cache_object(&session, &map_file, &context.hls_access_lease_id).await {
        CacheObjectLookup::Ready(object) => cache_object_serve_outcome(
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext::from_response_context(context),
            )
            .await,
        ),
        CacheObjectLookup::Failure(failure) => HlsResourceServeOutcome::Failure(failure),
    }
}

/// Serves a committed Ready transient passthrough full object from the HLS cache.
pub async fn serve_hls_transient_object_cache_response(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> Response<Body> {
    match serve_hls_transient_object_cache_outcome(segment_cache, session, resource_file, range_header, context).await {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => hls_resource_failure_default_response(failure),
    }
}

/// Serves a committed Ready transient passthrough object or returns a typed failure for endpoint-level mapping.
pub async fn serve_hls_transient_object_cache_outcome(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> HlsResourceServeOutcome {
    match lookup_transient_object_cache_object(&session, &resource_file, &context.hls_access_lease_id, context.now_ms)
        .await
    {
        CacheObjectLookup::Ready(object) => cache_object_serve_outcome(
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext::from_response_context(context),
            )
            .await,
        ),
        CacheObjectLookup::Failure(failure) => HlsResourceServeOutcome::Failure(failure),
    }
}

fn hls_resource_failure_default_response(failure: HlsResourceServeFailure) -> Response<Body> {
    match failure {
        HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms } => {
            service_unavailable_not_ready_response(retry_after_ms)
        }
        HlsResourceServeFailure::Missing
        | HlsResourceServeFailure::Expired
        | HlsResourceServeFailure::PermanentFailed { .. } => StatusCode::NOT_FOUND.into_response(),
    }
}

fn cache_object_serve_outcome(result: Result<Response<Body>, HlsResourceServeFailure>) -> HlsResourceServeOutcome {
    match result {
        Ok(response) => HlsResourceServeOutcome::Ready(response),
        Err(failure) => HlsResourceServeOutcome::Failure(failure),
    }
}

async fn serve_cache_object<K>(
    segment_cache: Arc<HlsSegmentCache>,
    object: CacheObject<K>,
    range_header: Option<HeaderValue>,
    context: CacheObjectServeContext,
) -> Result<Response<Body>, HlsResourceServeFailure>
where
    K: super::HlsCacheObjectKey + Send + Sync + 'static,
{
    let guard = CacheReadGuard::new(Arc::clone(&object.access), context.now_ms);
    let requested_proxy_seq = match context.playback_cursor_tracking {
        HlsPlaybackCursorTracking::Segment { proxy_seq } => Some(proxy_seq),
        HlsPlaybackCursorTracking::Disabled => None,
    };
    if let (Some(proxy_seq), Some(marker)) = (requested_proxy_seq, context.media_activity_marker.as_ref()) {
        marker.record_startup_segment_request(proxy_seq, context.now_ms);
    }
    if let Some(repair_context) = object.repair_context.clone() {
        if let Err(err) =
            context.segment_repair.repair_ready_cache_hit(&segment_cache, &object.key, repair_context).await
        {
            debug!(
                "HLS segment repair skipped for ready cache hit: session={} proxy_session={} resource={} error={err}",
                object.log_context.identity.session(),
                object.log_context.identity.proxy_session(),
                object.log_context.resource_id
            );
        }
    }
    if let (Some(proxy_seq), Some(marker)) = (requested_proxy_seq, context.media_activity_marker.as_ref()) {
        marker.record_startup_repair_decision(proxy_seq, current_time_millis());
    }
    let metadata_started_at = Instant::now();
    let metadata = match segment_cache.metadata(&object.key).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    log_body_reader_wait_if_slow(&object.log_context, "metadata", metadata_started_at.elapsed().as_millis());

    let body_id = next_hls_body_log_id();
    let range = resolve_single_byte_range(range_header.as_ref(), metadata.size);
    let cursor_proxy_seq = context.playback_cursor_tracking.full_object_proxy_seq(range, metadata.size);
    let (status, start, end, content_length) = match range {
        SingleByteRange::Full => {
            if metadata.size == 0 {
                if let Some(metrics) = &context.metrics {
                    metrics.record_cache_hit();
                }
                if let Some(marker) = &context.media_activity_marker {
                    marker.mark_at(context.now_ms).await;
                }
                return Ok(empty_ok_response(&object.content_type, context.cache_duration_seconds));
            }
            if let Some(metrics) = &context.metrics {
                metrics.record_cache_hit();
            }
            debug!(
                "HLS cache response prepared: body_id={} lease={} session={} proxy_session={} resource={} source=cache range=full content_length={} content_type={}",
                body_id,
                object.log_context.lease,
                object.log_context.identity.session(),
                object.log_context.identity.proxy_session(),
                object.log_context.resource_id,
                metadata.size,
                object.content_type
            );
            (StatusCode::OK, 0, metadata.size - 1, metadata.size)
        }
        SingleByteRange::Partial { start, end, length } => {
            if let Some(metrics) = &context.metrics {
                metrics.record_cache_hit();
                metrics.record_cache_range_hit();
            }
            debug!(
                "HLS cache response prepared: body_id={} lease={} session={} proxy_session={} resource={} source=cache range={start}-{end} content_length={length} content_type={}",
                body_id,
                object.log_context.lease,
                object.log_context.identity.session(),
                object.log_context.identity.proxy_session(),
                object.log_context.resource_id,
                object.content_type
            );
            (StatusCode::PARTIAL_CONTENT, start, end, length)
        }
        SingleByteRange::Unsatisfiable => return Ok(range_not_satisfiable_response(metadata.size)),
    };
    let file_started_at = Instant::now();
    let file = match segment_cache.open_range(&object.key, start).await {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    log_body_reader_wait_if_slow(&object.log_context, "file", file_started_at.elapsed().as_millis());
    let cursor_marker =
        if let (Some(proxy_seq), Some(marker)) = (cursor_proxy_seq, context.media_activity_marker.clone()) {
            let Some(marker) = marker.for_segment_request(proxy_seq, context.now_ms).await else {
                return Err(HlsResourceServeFailure::Expired);
            };
            Some(marker)
        } else {
            None
        };
    let startup_body_observation = requested_proxy_seq.and_then(|proxy_seq| {
        context
            .media_activity_marker
            .as_ref()
            .and_then(|marker| marker.begin_startup_cache_response(proxy_seq, &body_id, current_time_millis()))
    });
    if let Some(marker) = &context.media_activity_marker {
        marker.mark_at(context.now_ms).await;
    }
    let completion_marker = cursor_marker.as_ref().and_then(HlsMediaActivityMarker::completed_segment_marker);

    let stream = ReaderStream::new(file.take(content_length));
    let body_context = CacheBodyLogContext {
        body_id,
        identity: object.log_context.identity.clone(),
        resource_id: object.log_context.resource_id.clone(),
        object_kind: object.log_context.object_kind,
        source: object.log_context.body_source,
        content_length,
    };
    let stream = ActiveReaderStream::new(
        Box::pin(stream),
        Some(guard),
        body_context,
        context.qos_meter,
        completion_marker,
        startup_body_observation,
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    insert_header_value(headers, header::CONTENT_TYPE, &object.content_type);
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static(ACCEPT_RANGES_VALUE));
    insert_u64_header(headers, header::CONTENT_LENGTH, content_length);
    insert_cache_control(headers, context.cache_duration_seconds);
    if status == StatusCode::PARTIAL_CONTENT {
        insert_header_value(headers, header::CONTENT_RANGE, &format!("bytes {start}-{end}/{}", metadata.size));
    }
    mark_response_as_uncompressed(&mut response);
    Ok(response)
}

async fn lookup_segment_cache_object(
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    hls_access_lease_id: &HlsAccessLeaseId,
) -> CacheObjectLookup<SegmentCacheKey> {
    let session = session.read().await;
    if session.is_gc_marked_for_removal() {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Expired);
    }
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    };
    if entry.proxy_file_ext != segment_file.extension {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    }
    match entry.status {
        SegmentCacheStatus::Ready { .. } => {}
        SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Discovered
        | SegmentCacheStatus::CapacityDeferred { .. } => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                retry_after_ms: NOT_READY_RETRY_AFTER_MS,
            });
        }
        SegmentCacheStatus::FailedRetryable { retry_after_ms, .. } => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms });
        }
        SegmentCacheStatus::FailedPermanent { status, .. } => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::PermanentFailed { status });
        }
        SegmentCacheStatus::Expired => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::Expired);
        }
    }
    CacheObjectLookup::Ready(CacheObject {
        key: entry.cache_key.clone(),
        access: Arc::clone(&entry.access),
        content_type: entry.content_type.clone(),
        log_context: CacheObjectLogContext {
            lease: safe_hls_access_lease_id(hls_access_lease_id),
            identity: HlsLogIdentity::from_session(&session),
            resource_id: format!("{:06}", segment_file.proxy_seq),
            object_kind: "Segment",
            body_source: "normal",
        },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Normal,
            log_identity: HlsLogIdentity::from_session(&session),
            proxy_session_id: session.proxy_session_id.clone(),
            hls_access_lease_id: Some(hls_access_lease_id.clone()),
            rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: segment_file.proxy_seq },
            resource_id: format!("{:06}", segment_file.proxy_seq),
            file_ext: entry.proxy_file_ext.clone(),
            // Cache-hit repair validation may carry the concrete fetch URL as diagnostic metadata only.
            origin_fetch_uri_for_diagnostics: entry
                .origin_fetch_ref
                .as_ref()
                .map(|fetch_ref| fetch_ref.resolved_origin_url.clone())
                .unwrap_or_default(),
            media_sequence: Some(entry.origin_key.host_local_sequence),
            discontinuity_sequence: Some(session.discontinuity_sequence),
            complete_object: entry.origin_byte_range.is_none(),
            encrypted: entry.encryption.is_some(),
            custom_response: false,
        }),
    })
}

async fn lookup_map_cache_object(
    session: &HlsSessionHandle,
    map_file: &HlsMapFile,
    hls_access_lease_id: &HlsAccessLeaseId,
) -> CacheObjectLookup<MapCacheKey> {
    let session = session.read().await;
    if session.is_gc_marked_for_removal() {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Expired);
    }
    let Some(entry) = session.maps.get(&ProxyMapId(map_file.proxy_map_id)) else {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    };
    if entry.proxy_file_ext != map_file.extension {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    }
    match entry.status {
        MapCacheStatus::Ready { .. } => {}
        MapCacheStatus::Fetching { .. } | MapCacheStatus::Queued { .. } | MapCacheStatus::Discovered => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                retry_after_ms: NOT_READY_RETRY_AFTER_MS,
            });
        }
        MapCacheStatus::FailedRetryable { retry_after_ms, .. } => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms });
        }
        MapCacheStatus::FailedPermanent { status, .. } => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::PermanentFailed { status });
        }
        MapCacheStatus::Expired => {
            return CacheObjectLookup::Failure(HlsResourceServeFailure::Expired);
        }
    }
    CacheObjectLookup::Ready(CacheObject {
        key: entry.cache_key.clone(),
        access: Arc::clone(&entry.access),
        content_type: entry.content_type.clone(),
        log_context: CacheObjectLogContext {
            lease: safe_hls_access_lease_id(hls_access_lease_id),
            identity: HlsLogIdentity::from_session(&session),
            resource_id: format!("map:{:06}", map_file.proxy_map_id),
            object_kind: "Map",
            body_source: "normal",
        },
        repair_context: None,
    })
}

async fn lookup_transient_object_cache_object(
    session: &HlsSessionHandle,
    resource_file: &TransientResourceFile,
    hls_access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> CacheObjectLookup<TransientObjectCacheKey> {
    let mut session = session.write().await;
    if session.is_gc_marked_for_removal() {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Expired);
    }
    let proxy_session_id = session.proxy_session_id.clone();
    let key = super::TransientPassthroughState::transient_object_key(
        &proxy_session_id,
        &resource_file.resource_id,
        resource_file.extension.clone(),
    );
    let Some(resource) = session.transient.resources.get(&resource_file.resource_id) else {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    };
    if resource.file_ext_hint.as_deref() != Some(resource_file.extension.as_str()) {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    }
    let resource_state = (resource.kind, resource.encrypted_media);
    let resource_kind = Some(resource_state.0);
    let protected = ProtectedSet::from_session(&session).key_resource_ids.contains(&resource_file.resource_id);
    let Some(entry) = session.transient.ready_object(&key, resource_state.0, now_ms, protected) else {
        return match session.transient.object_cache.get(&key).map(|entry| &entry.status) {
            Some(super::TransientObjectCacheStatus::Fetching { .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                    retry_after_ms: NOT_READY_RETRY_AFTER_MS,
                })
            }
            Some(super::TransientObjectCacheStatus::FailedRetryable { retry_after_ms, .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                    retry_after_ms: *retry_after_ms,
                })
            }
            Some(super::TransientObjectCacheStatus::FailedPermanent { status, .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::PermanentFailed { status: *status })
            }
            Some(super::TransientObjectCacheStatus::Ready { .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::Expired)
            }
            None => CacheObjectLookup::Failure(HlsResourceServeFailure::Missing),
        };
    };
    CacheObjectLookup::Ready(CacheObject {
        key: entry.key,
        access: Arc::clone(&entry.access),
        content_type: entry.content_type,
        log_context: CacheObjectLogContext {
            lease: safe_hls_access_lease_id(hls_access_lease_id),
            identity: HlsLogIdentity::from_session(&session),
            resource_id: resource_file.resource_id.0.clone(),
            object_kind: transient_body_object_kind(resource_kind, &resource_file.extension),
            body_source: "transient",
        },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Transient,
            log_identity: HlsLogIdentity::from_session(&session),
            proxy_session_id,
            hls_access_lease_id: Some(hls_access_lease_id.clone()),
            rendered_object_id: HlsRepairRenderedObjectId::Transient {
                resource_id: resource_file.resource_id.0.clone(),
            },
            resource_id: resource_file.resource_id.0.clone(),
            file_ext: resource_file.extension.clone(),
            origin_fetch_uri_for_diagnostics: resource_file.resource_id.0.clone(),
            media_sequence: None,
            discontinuity_sequence: None,
            complete_object: true,
            encrypted: resource_state.1 || resource_state.0 == TransientResourceKind::Key,
            custom_response: false,
        }),
    })
}

fn empty_ok_response(content_type: &str, cache_duration_seconds: u64) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    insert_header_value(headers, header::CONTENT_TYPE, content_type);
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static(ACCEPT_RANGES_VALUE));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    insert_cache_control(headers, cache_duration_seconds);
    mark_response_as_uncompressed(&mut response);
    response
}

fn range_not_satisfiable_response(full_size: u64) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static(ACCEPT_RANGES_VALUE));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    insert_header_value(headers, header::CONTENT_RANGE, &format!("bytes */{full_size}"));
    mark_response_as_uncompressed(&mut response);
    response
}

fn service_unavailable_not_ready_response(retry_after_ms: u64) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    insert_header_value(headers, header::RETRY_AFTER, &super::retry_after_secs_from_ms(retry_after_ms).to_string());
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    mark_response_as_uncompressed(&mut response);
    response
}

fn insert_cache_control(headers: &mut axum::http::HeaderMap, cache_duration_seconds: u64) {
    insert_header_value(
        headers,
        header::CACHE_CONTROL,
        &format!("public, max-age={cache_duration_seconds}, immutable"),
    );
}

fn insert_u64_header(headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: u64) {
    insert_header_value(headers, name, &value.to_string());
}

fn insert_header_value(headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
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

impl PreparedBytesStream {
    fn new(bytes: Bytes) -> Self {
        Self { remaining: bytes }
    }
}

impl Stream for PreparedBytesStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining.is_empty() {
            return Poll::Ready(None);
        }
        let chunk_len = self.remaining.len().min(PREPARED_MEDIA_CHUNK_SIZE);
        Poll::Ready(Some(Ok(self.remaining.split_to(chunk_len))))
    }
}

impl ActiveReaderStream {
    fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        guard: Option<CacheReadGuard>,
        context: CacheBodyLogContext,
        meter: Arc<ArcSwapOption<StreamMeterHandle>>,
        media_activity_marker: Option<HlsMediaActivityMarker>,
        startup_body_observation: Option<HlsStartupBodyObservation>,
    ) -> Self {
        Self {
            inner,
            _guard: guard,
            context,
            started_at: Instant::now(),
            last_yield_at: Instant::now(),
            send_deadline: Box::pin(sleep(hls_client_body_send_deadline())),
            max_idle_ms: 0,
            completed_logged: false,
            finished: false,
            bytes_yielded: 0,
            meter,
            media_activity_marker,
            startup_body_observation,
        }
    }

    fn log_completed(&mut self, outcome: &'static str) {
        if self.completed_logged {
            return;
        }
        self.completed_logged = true;
        debug!(
            "{} '{}' body completed: body_id={} session={} proxy_session={} source={} elapsed_s={:.3} idle_max_s={:.3} bytes={}/{} outcome={}",
            self.context.object_kind,
            self.context.resource_id,
            self.context.body_id,
            self.context.identity.session(),
            self.context.identity.proxy_session(),
            self.context.source,
            duration_secs(self.started_at.elapsed().as_millis()),
            duration_secs(self.max_idle_ms),
            self.bytes_yielded,
            self.context.content_length,
            outcome
        );
        if let Some(observation) = &self.startup_body_observation {
            observation.finish(current_time_millis(), outcome);
        }
        if self.bytes_yielded >= self.context.content_length {
            if let Some(marker) = &self.media_activity_marker {
                marker.spawn_mark_completion_now();
            }
        }
    }
}

impl Stream for ActiveReaderStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if self.send_deadline.as_mut().poll(cx).is_ready() {
            self.finished = true;
            self.log_completed("timeout");
            return Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::TimedOut, "hls client body send timed out"))));
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let first_chunk = self.bytes_yielded == 0;
                refresh_hls_client_body_send_deadline(self.send_deadline.as_mut());
                let idle_ms = self.last_yield_at.elapsed().as_millis();
                self.max_idle_ms = self.max_idle_ms.max(idle_ms);
                self.last_yield_at = Instant::now();
                self.bytes_yielded = self.bytes_yielded.saturating_add(chunk.len() as u64);
                if let Some(meter) = self.meter.load_full() {
                    meter.record_bytes(chunk.len() as u64);
                }
                if first_chunk
                    && self
                        .startup_body_observation
                        .as_ref()
                        .is_some_and(|observation| observation.record_first_chunk(current_time_millis()))
                {
                    debug!(
                        "HLS cache body first chunk: body_id={} session={} proxy_session={} resource={} bytes={}",
                        self.context.body_id,
                        self.context.identity.session(),
                        self.context.identity.proxy_session(),
                        self.context.resource_id,
                        chunk.len()
                    );
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finished = true;
                self.log_completed("error");
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                self.finished = true;
                self.log_completed("ok");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ActiveReaderStream {
    fn drop(&mut self) {
        let outcome = if self.bytes_yielded >= self.context.content_length { "ok" } else { "drop" };
        self.log_completed(outcome);
    }
}

struct CacheReadGuard {
    access: Arc<CacheAccessState>,
}

impl CacheReadGuard {
    fn new(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for CacheReadGuard {
    fn drop(&mut self) {
        self.access.reader_finished();
    }
}

fn log_body_reader_wait_if_slow(context: &CacheObjectLogContext, wait_for: &'static str, elapsed_ms: u128) {
    if elapsed_ms < BODY_READER_WAIT_LOG_THRESHOLD_MS {
        return;
    }
    debug!(
        "HLS cache reader wait: lease={} session={} proxy_session={} resource={} wait_for={} elapsed_ms={}",
        context.lease,
        context.identity.session(),
        context.identity.proxy_session(),
        context.resource_id,
        wait_for,
        elapsed_ms
    );
}

fn duration_secs(elapsed_ms: u128) -> f64 {
    Duration::from_millis(u64::try_from(elapsed_ms).unwrap_or(u64::MAX)).as_secs_f64()
}

fn transient_body_object_kind(resource_kind: Option<TransientResourceKind>, extension: &str) -> &'static str {
    match resource_kind {
        Some(TransientResourceKind::Key) => "Key",
        Some(TransientResourceKind::Map) => "Map",
        Some(TransientResourceKind::Segment | TransientResourceKind::Part | TransientResourceKind::Other) => "Segment",
        None => {
            if extension.eq_ignore_ascii_case("key") {
                "Key"
            } else {
                "Segment"
            }
        }
    }
}

fn next_hls_body_log_id() -> String {
    let value = NEXT_HLS_BODY_LOG_ID.fetch_add(1, Ordering::Relaxed);
    format!("{value:08x}")
}

use tuliprox_core::utils::current_time_millis;

#[cfg(test)]
mod tests {
    use super::{
        current_time_millis, finite_hls_media_response, serve_cache_object, serve_hls_segment_cache_outcome,
        spawn_bounded_media_completion, transient_body_object_kind, ActiveReaderStream, CacheBodyLogContext,
        CacheObject, CacheObjectLogContext, CacheObjectServeContext, HlsCacheResponseContext, HlsMediaActivityMarker,
        HlsPlaybackCursorTracking, HlsResourceServeFailure, HlsResourceServeOutcome, PREPARED_MEDIA_CHUNK_SIZE,
    };
    use crate::{
        media_reserve::HlsLeasePlaybackCursor, CacheAccessState, HlsAccessLease, HlsAccessLeaseId, HlsLogIdentity,
        HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager,
        HlsSessionHandle, HlsSessionKey, OriginSegmentKey, ProxySessionId, SegmentCacheKey, SegmentCacheStatus,
        SegmentEntry, TransientResourceKind,
    };
    use tuliprox_core::model::HlsSegmentRepairConfig;
    use tuliprox_session::StreamMeterHandle;

    fn test_log_identity() -> HlsLogIdentity {
        HlsLogIdentity::for_test("content-session", "proxy-session")
    }
    use arc_swap::ArcSwapOption;
    use axum::http::{header, HeaderValue, StatusCode};
    use bytes::Bytes;
    use futures::StreamExt;
    use http_body_util::BodyExt;
    use shared::model::HlsSegmentRepairMode;
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Weak,
        },
        time::Duration,
    };
    use tokio::{sync::Semaphore, time::advance};

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).expect("valid header")
    }

    fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
        Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }))
    }

    #[tokio::test]
    async fn media_completion_scheduling_does_not_block_when_completion_capacity_is_busy() {
        let permits = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&permits).acquire_owned().await.expect("initial permit");
        let completed = Arc::new(AtomicBool::new(false));
        let completed_for_task = Arc::clone(&completed);
        spawn_bounded_media_completion(Arc::clone(&permits), async move {
            completed_for_task.store(true, Ordering::Release);
        });

        tokio::task::yield_now().await;
        assert!(!completed.load(Ordering::Acquire));
        drop(held);

        for _ in 0..16 {
            if completed.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(completed.load(Ordering::Acquire));
        assert_eq!(permits.available_permits(), 1);
    }

    async fn live_media_marker(
        manager: &Arc<HlsProxyManager>,
        session: &HlsSessionHandle,
        lease_id: HlsAccessLeaseId,
    ) -> HlsMediaActivityMarker {
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease = HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("test-user", "test-fingerprint"),
            proxy_session_id.clone(),
            "test-user".to_string(),
            "test-session".to_string(),
            1,
            "stream".to_string(),
            1,
            current_time_millis(),
            60_000,
        );
        let lease_identity = lease.media_identity().expect("live lease identity");
        manager.prepare_access_lease(lease).await;
        HlsMediaActivityMarker::new(
            Arc::clone(manager),
            Arc::clone(session),
            proxy_session_id,
            lease_id,
            lease_identity,
        )
    }

    struct CachedSegmentFixture {
        _temp_dir: tempfile::TempDir,
        manager: Arc<HlsProxyManager>,
        segment_cache: Arc<HlsSegmentCache>,
        session: HlsSessionHandle,
        proxy_session_id: ProxySessionId,
        lease_id: HlsAccessLeaseId,
        segment_file: HlsSegmentFile,
        access: Arc<CacheAccessState>,
        context: HlsCacheResponseContext,
        meter: Arc<StreamMeterHandle>,
        full_size: u64,
    }

    impl CachedSegmentFixture {
        async fn new(bytes: Vec<u8>) -> Self {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
            let segment_cache = Arc::clone(manager.segment_cache());
            let session = manager.get_or_create_session(HlsSessionKey::new(1, "range-stream"), b"secret", 1_000).await;
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            let proxy_seq = 12;
            let cache_key = SegmentCacheKey::new(proxy_session_id.clone(), proxy_seq, "ts");
            segment_cache.write_bytes_and_commit(&cache_key, &bytes).await.expect("segment cache commit");
            let full_size = u64::try_from(bytes.len()).expect("test segment size");
            let access = Arc::new(CacheAccessState::new());
            session.write().await.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch: 1,
                        effective_host_id: 1,
                        host_local_sequence: proxy_seq,
                        host_local_index: u32::try_from(proxy_seq).expect("test proxy sequence"),
                    },
                    proxy_seq,
                    duration_ms: 4_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key,
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready { content_length: full_size, ready_at_ms: 1_000 },
                    last_rendered_at_ms: Some(1_000),
                    access: Arc::clone(&access),
                },
            );
            let lease_id = HlsAccessLeaseId("range-lease".to_string());
            let marker = live_media_marker(&manager, &session, lease_id.clone()).await;
            let meter = Arc::new(StreamMeterHandle::new(7, Weak::new()));
            let log_identity = {
                let session = session.read().await;
                HlsLogIdentity::from_session(&session)
            };
            let context = HlsCacheResponseContext::new(
                lease_id.clone(),
                log_identity,
                300,
                Arc::clone(manager.metrics()),
                Arc::clone(manager.segment_repair()),
                Some(Arc::clone(&meter)),
                Some(marker),
                current_time_millis(),
            );
            Self {
                _temp_dir: temp_dir,
                manager,
                segment_cache,
                session,
                proxy_session_id,
                lease_id,
                segment_file: HlsSegmentFile { proxy_seq, extension: "ts".to_string() },
                access,
                context,
                meter,
                full_size,
            }
        }

        async fn serve(&self, range: Option<&str>) -> HlsResourceServeOutcome {
            serve_hls_segment_cache_outcome(
                Arc::clone(&self.segment_cache),
                Arc::clone(&self.session),
                self.segment_file.clone(),
                range.map(header),
                &self.context,
            )
            .await
        }

        async fn cursor(&self) -> HlsLeasePlaybackCursor {
            self.manager
                .access_lease_response_snapshot(&self.lease_id, &self.proxy_session_id, current_time_millis())
                .await
                .expect("range test lease")
                .playback_cursor
        }

        async fn wait_for_completion(&self) -> HlsLeasePlaybackCursor {
            for _ in 0..64 {
                let cursor = self.cursor().await;
                if cursor.highest_contiguous_completed_proxy_seq == Some(self.segment_file.proxy_seq) {
                    return cursor;
                }
                tokio::task::yield_now().await;
            }
            panic!("segment completion task did not commit")
        }
    }

    fn ready_response(outcome: HlsResourceServeOutcome) -> axum::response::Response {
        match outcome {
            HlsResourceServeOutcome::Ready(response) => response,
            HlsResourceServeOutcome::Failure(failure) => panic!("expected ready response, got {failure:?}"),
        }
    }

    #[tokio::test]
    async fn finite_terminal_media_supports_full_range_and_unsatisfiable_responses() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let context = HlsCacheResponseContext::new(
            HlsAccessLeaseId("terminal-lease".to_string()),
            test_log_identity(),
            300,
            Arc::clone(manager.metrics()),
            Arc::clone(manager.segment_repair()),
            None,
            None,
            1_000,
        );
        let proxy_session_id = ProxySessionId("terminal-session".to_string());
        let full = finite_hls_media_response(
            Bytes::from_static(b"0123456789"),
            None,
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        assert_eq!(full.status(), StatusCode::OK);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&full));
        assert_eq!(full.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(full.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"0123456789"));

        let range_header = header("bytes=2-5");
        let partial = finite_hls_media_response(
            Bytes::from_static(b"0123456789"),
            Some(&range_header),
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&partial));
        assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(partial.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"2345"));

        let invalid_header = header("bytes=20-");
        let invalid = finite_hls_media_response(
            Bytes::from_static(b"0123456789"),
            Some(&invalid_header),
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        assert_eq!(invalid.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&invalid));
        assert_eq!(invalid.headers()[header::CONTENT_RANGE], "bytes */10");
    }

    #[tokio::test]
    async fn finite_terminal_media_uses_qos_completion_and_drop_semantics() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let session = manager.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 1_000).await;
        let lease_id = HlsAccessLeaseId("terminal-lease".to_string());
        let marker = live_media_marker(&manager, &session, lease_id.clone()).await;
        let meter = Arc::new(StreamMeterHandle::new(7, Weak::new()));
        let context = HlsCacheResponseContext::new(
            lease_id,
            test_log_identity(),
            300,
            Arc::clone(manager.metrics()),
            Arc::clone(manager.segment_repair()),
            Some(Arc::clone(&meter)),
            Some(marker),
            1_000,
        );
        let proxy_session_id = ProxySessionId("terminal-session".to_string());

        let dropped = finite_hls_media_response(
            Bytes::from_static(b"0123456789"),
            None,
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        drop(dropped);
        tokio::task::yield_now().await;
        assert_eq!(meter.bytes_total(), 0);
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);

        let partially_consumed = finite_hls_media_response(
            Bytes::from(vec![7_u8; PREPARED_MEDIA_CHUNK_SIZE.saturating_mul(2)]),
            None,
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        let mut partial_body = partially_consumed.into_body();
        let partial_chunk = partial_body
            .frame()
            .await
            .expect("first frame")
            .expect("first frame body")
            .into_data()
            .expect("first data frame");
        drop(partial_body);
        tokio::task::yield_now().await;
        assert_eq!(partial_chunk.len(), PREPARED_MEDIA_CHUNK_SIZE);
        assert_eq!(meter.bytes_total(), PREPARED_MEDIA_CHUNK_SIZE as u64);
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);

        context.mark_media_activity().await;
        let range = header("bytes=2-5");
        let completed = finite_hls_media_response(
            Bytes::from_static(b"0123456789"),
            Some(&range),
            "video/mp2t",
            "private, immutable",
            &context,
            &proxy_session_id,
            "terminal/1/0".to_string(),
        );
        assert_eq!(completed.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&completed));
        assert_eq!(completed.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"2345"));
        assert_eq!(meter.bytes_total(), PREPARED_MEDIA_CHUNK_SIZE as u64 + 4);
        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(1_000));
    }

    #[tokio::test(start_paused = true)]
    async fn finite_terminal_media_stream_honors_shared_send_deadline() {
        let stream = ActiveReaderStream::new(
            Box::pin(futures::stream::pending()),
            None,
            CacheBodyLogContext {
                body_id: "00000001".to_string(),
                identity: test_log_identity(),
                resource_id: "terminal/1/0".to_string(),
                object_kind: "TerminalSegment",
                source: "prepared",
                content_length: 1,
            },
            Arc::new(ArcSwapOption::from(None::<Arc<StreamMeterHandle>>)),
            None,
            None,
        );
        let task = tokio::spawn(async move {
            let mut stream = stream;
            stream.next().await
        });
        tokio::task::yield_now().await;
        advance(super::hls_client_body_send_deadline().saturating_add(Duration::from_millis(1))).await;

        let result = task.await.expect("deadline task").expect("deadline result");
        assert_eq!(result.expect_err("deadline error").kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn transient_body_log_kind_uses_resource_kind() {
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Key), "bin"), "Key");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Map), "bin"), "Map");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Segment), "key"), "Segment");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Part), "m4s"), "Segment");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Other), "bin"), "Segment");
    }

    #[test]
    fn transient_body_log_kind_falls_back_to_key_extension() {
        assert_eq!(transient_body_object_kind(None, "key"), "Key");
        assert_eq!(transient_body_object_kind(None, "KEY"), "Key");
        assert_eq!(transient_body_object_kind(None, "ts"), "Segment");
    }

    #[test]
    fn temporary_unavailable_response_uses_concrete_retry_after() {
        let response =
            super::hls_resource_failure_default_response(super::HlsResourceServeFailure::TemporaryUnavailable {
                retry_after_ms: 2_500,
            });

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry-after"), "3");
    }

    #[tokio::test]
    async fn cache_hit_bodies_use_independent_readers_for_same_object() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 12, "ts");
        segment_cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");
        let access = Arc::new(CacheAccessState::new());
        let object = CacheObject {
            key,
            access: Arc::clone(&access),
            content_type: "video/mp2t".to_string(),
            log_context: CacheObjectLogContext {
                lease: "lease-a".to_string(),
                identity: test_log_identity(),
                resource_id: "000012".to_string(),
                object_kind: "Segment",
                body_source: "normal",
            },
            repair_context: None,
        };

        let first = serve_cache_object(
            Arc::clone(&segment_cache),
            object.clone(),
            Some(header("bytes=0-")),
            CacheObjectServeContext {
                cache_duration_seconds: 300,
                metrics: None,
                segment_repair: test_segment_repair_manager(),
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
                media_activity_marker: None,
                playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
                now_ms: 1,
            },
        )
        .await
        .expect("first cache object response");
        let second = serve_cache_object(
            segment_cache,
            CacheObject {
                log_context: CacheObjectLogContext {
                    lease: "lease-b".to_string(),
                    identity: test_log_identity(),
                    resource_id: "000012".to_string(),
                    object_kind: "Segment",
                    body_source: "normal",
                },
                ..object
            },
            Some(header("bytes=0-")),
            CacheObjectServeContext {
                cache_duration_seconds: 300,
                metrics: None,
                segment_repair: test_segment_repair_manager(),
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
                media_activity_marker: None,
                playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
                now_ms: 2,
            },
        )
        .await
        .expect("second cache object response");

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(access.active_readers(), 2);

        let (first_body, second_body) = tokio::join!(first.into_body().collect(), second.into_body().collect(),);

        assert_eq!(first_body.expect("first body").to_bytes(), Bytes::from_static(b"0123456789"));
        assert_eq!(second_body.expect("second body").to_bytes(), Bytes::from_static(b"0123456789"));
        assert_eq!(access.active_readers(), 0);
        assert_eq!(access.last_accessed_at_ms(), 2);
    }

    #[tokio::test]
    async fn full_object_segment_ranges_record_request_and_completion_without_changing_http_range_status() {
        const FULL_SIZE: usize = 512;
        for (range, expected_status) in [
            (None, StatusCode::OK),
            (Some("bytes=0-"), StatusCode::PARTIAL_CONTENT),
            (Some("bytes=0-511"), StatusCode::PARTIAL_CONTENT),
            (Some("bytes=-512"), StatusCode::PARTIAL_CONTENT),
        ] {
            let expected_body = vec![7_u8; FULL_SIZE];
            let fixture = CachedSegmentFixture::new(expected_body.clone()).await;
            let response = ready_response(fixture.serve(range).await);

            assert_eq!(response.status(), expected_status, "unexpected status for {range:?}");
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert!(!tuliprox_core::utils::response_compression::should_compress_response(&response));
            assert_eq!(response.headers()[header::CONTENT_LENGTH], FULL_SIZE.to_string());
            if range.is_some() {
                assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-511/512");
            } else {
                assert!(!response.headers().contains_key(header::CONTENT_RANGE));
            }
            assert_eq!(fixture.access.active_readers(), 1);
            let requested = fixture.cursor().await;
            assert_eq!(requested.first_requested_proxy_seq, Some(12), "missing request for {range:?}");
            assert_eq!(requested.last_requested_proxy_seq, Some(12), "missing request for {range:?}");
            assert_eq!(requested.highest_contiguous_completed_proxy_seq, None);
            assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());

            assert_eq!(
                response.into_body().collect().await.expect("full range body").to_bytes(),
                Bytes::from(expected_body)
            );
            let completed = fixture.wait_for_completion().await;
            assert_eq!(completed.highest_contiguous_completed_proxy_seq, Some(12));
            assert!(completed.first_segment_completed_at_ms.is_some());
            assert_eq!(fixture.meter.bytes_total(), fixture.full_size);
            assert_eq!(fixture.access.active_readers(), 0);
        }
    }

    #[tokio::test]
    async fn partial_segment_ranges_mark_activity_without_mutating_playback_cursor() {
        const FULL_SIZE: usize = 512;
        for (range, expected_start, expected_end) in [("bytes=1-", 1, 511), ("bytes=0-187", 0, 187)] {
            let bytes = (0..FULL_SIZE).map(|value| u8::try_from(value % 251).expect("test byte")).collect::<Vec<_>>();
            let fixture = CachedSegmentFixture::new(bytes.clone()).await;
            let response = ready_response(fixture.serve(Some(range)).await);
            let expected_body = &bytes[expected_start..=expected_end];

            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
            assert!(!tuliprox_core::utils::response_compression::should_compress_response(&response));
            assert_eq!(
                response.headers()[header::CONTENT_RANGE],
                format!("bytes {expected_start}-{expected_end}/{FULL_SIZE}")
            );
            assert_eq!(response.headers()[header::CONTENT_LENGTH], expected_body.len().to_string());
            assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());
            assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());

            assert_eq!(
                response.into_body().collect().await.expect("partial range body").to_bytes(),
                Bytes::copy_from_slice(expected_body)
            );
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
            assert_eq!(fixture.meter.bytes_total(), u64::try_from(expected_body.len()).expect("partial body size"));
            assert_eq!(fixture.access.active_readers(), 0);
        }
    }

    #[tokio::test]
    async fn dropped_full_range_segment_body_records_request_without_completion() {
        const FULL_SIZE: usize = 512 * 1_024;
        let fixture = CachedSegmentFixture::new(vec![9_u8; FULL_SIZE]).await;
        let response = ready_response(fixture.serve(Some("bytes=0-")).await);

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], format!("bytes 0-{}/{FULL_SIZE}", FULL_SIZE - 1));
        let requested = fixture.cursor().await;
        assert_eq!(requested.first_requested_proxy_seq, Some(12));
        assert_eq!(requested.last_requested_proxy_seq, Some(12));
        assert_eq!(requested.highest_contiguous_completed_proxy_seq, None);
        assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());

        let mut body = response.into_body();
        let first_chunk = body
            .frame()
            .await
            .expect("first segment frame")
            .expect("first segment frame body")
            .into_data()
            .expect("first segment data frame");
        assert!(first_chunk.len() < FULL_SIZE);
        drop(body);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let dropped = fixture.cursor().await;
        assert_eq!(dropped.first_requested_proxy_seq, Some(12));
        assert_eq!(dropped.last_requested_proxy_seq, Some(12));
        assert_eq!(dropped.highest_contiguous_completed_proxy_seq, None);
        assert_eq!(dropped.first_segment_completed_at_ms, None);
        assert_eq!(fixture.meter.bytes_total(), u64::try_from(first_chunk.len()).expect("first chunk size"));
        assert_eq!(fixture.access.active_readers(), 0);
    }

    #[tokio::test]
    async fn unsatisfiable_or_missing_segment_records_neither_activity_nor_cursor() {
        const FULL_SIZE: usize = 512;
        let fixture = CachedSegmentFixture::new(vec![5_u8; FULL_SIZE]).await;
        let unsatisfiable = ready_response(fixture.serve(Some("bytes=512-")).await);

        assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */512");
        assert_eq!(fixture.session.read().await.activity.last_authorized_media_at_ms, None);
        assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
        assert_eq!(fixture.meter.bytes_total(), 0);
        assert_eq!(fixture.access.active_readers(), 0);

        let missing = serve_hls_segment_cache_outcome(
            Arc::clone(&fixture.segment_cache),
            Arc::clone(&fixture.session),
            HlsSegmentFile { proxy_seq: 99, extension: "ts".to_string() },
            Some(header("bytes=0-")),
            &fixture.context,
        )
        .await;
        assert!(matches!(missing, HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Missing)));
        assert_eq!(fixture.session.read().await.activity.last_authorized_media_at_ms, None);
        assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
        assert_eq!(fixture.meter.bytes_total(), 0);
    }

    #[tokio::test]
    async fn cache_body_marks_media_activity_at_start_and_body_end() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 12, "ts");
        segment_cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");
        let access = Arc::new(CacheAccessState::new());
        let session = hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 1_000).await;
        let marker = live_media_marker(&hls_proxy, &session, HlsAccessLeaseId("lease-a".to_string())).await;

        let response = serve_cache_object(
            segment_cache,
            CacheObject {
                key,
                access,
                content_type: "video/mp2t".to_string(),
                log_context: CacheObjectLogContext {
                    lease: "lease-a".to_string(),
                    identity: test_log_identity(),
                    resource_id: "000012".to_string(),
                    object_kind: "Segment",
                    body_source: "normal",
                },
                repair_context: None,
            },
            Some(header("bytes=0-")),
            CacheObjectServeContext {
                cache_duration_seconds: 300,
                metrics: None,
                segment_repair: test_segment_repair_manager(),
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
                media_activity_marker: Some(marker),
                playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
                now_ms: 1_000,
            },
        )
        .await
        .expect("cache object response");

        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(1_000));

        assert_eq!(response.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"0123456789"));
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(
            session.read().await.activity.last_authorized_media_at_ms.expect("media activity") >= 1_000,
            "body completion should not move media activity backwards"
        );
    }
}
