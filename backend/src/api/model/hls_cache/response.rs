#![allow(clippy::large_futures, clippy::large_enum_variant, clippy::too_many_lines)]

use super::{
    hls_client_body_send_deadline, safe_hls_access_lease_id, safe_proxy_session_id, CacheAccessState,
    HlsAccessLeaseId, HlsCacheMetrics, HlsMapFile, HlsProxyManager, HlsRepairRenderedObjectId, HlsSegmentCache,
    HlsSegmentFile, HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource,
    HlsSessionHandle, MapCacheKey, MapCacheStatus, ProxyMapId, SegmentCacheKey, SegmentCacheStatus,
    TransientObjectCacheKey, TransientResourceFile, TransientResourceKind,
};
use crate::api::{api_utils::mark_response_as_uncompressed, model::StreamMeterHandle};
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
    io,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
use tokio::time::{sleep, Sleep};
use tokio_util::io::ReaderStream;

const ACCEPT_RANGES_VALUE: &str = "bytes";
const NOT_READY_RETRY_AFTER_MS: u64 = 1_000;
const BODY_READER_WAIT_LOG_THRESHOLD_MS: u128 = 10;
static NEXT_HLS_BODY_LOG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RangeDecision {
    Full,
    Partial { start: u64, end: u64, length: u64 },
    Unsatisfiable,
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
    session: String,
    resource_id: String,
    object_kind: &'static str,
    body_source: &'static str,
}

#[derive(Clone)]
struct CacheBodyLogContext {
    session: String,
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
    pub cache_duration_seconds: u64,
    pub metrics: Arc<HlsCacheMetrics>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub qos_meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    pub media_activity_marker: Option<HlsMediaActivityMarker>,
    pub now_ms: u64,
}

impl HlsCacheResponseContext {
    pub fn new(
        hls_access_lease_id: HlsAccessLeaseId,
        cache_duration_seconds: u64,
        metrics: Arc<HlsCacheMetrics>,
        segment_repair: Arc<HlsSegmentRepairManager>,
        qos_meter: Option<Arc<StreamMeterHandle>>,
        media_activity_marker: Option<HlsMediaActivityMarker>,
        now_ms: u64,
    ) -> Self {
        Self {
            hls_access_lease_id,
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
}

#[derive(Clone)]
pub struct HlsMediaActivityMarker {
    manager: Arc<HlsProxyManager>,
    session: HlsSessionHandle,
}

impl HlsMediaActivityMarker {
    pub fn new(manager: Arc<HlsProxyManager>, session: HlsSessionHandle) -> Self { Self { manager, session } }

    pub async fn mark_at(&self, now_ms: u64) {
        self.manager.mark_authorized_media_access_for_session(&self.session, now_ms).await;
    }

    pub fn spawn_mark_at(&self, now_ms: u64) {
        let marker = self.clone();
        tokio::spawn(async move {
            marker.mark_at(now_ms).await;
        });
    }

    pub fn spawn_mark_now(&self) { self.spawn_mark_at(current_time_millis()); }
}

struct CacheObjectServeContext {
    cache_duration_seconds: u64,
    metrics: Option<Arc<HlsCacheMetrics>>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    qos_meter: Arc<ArcSwapOption<StreamMeterHandle>>,
    media_activity_marker: Option<HlsMediaActivityMarker>,
    now_ms: u64,
}

impl CacheObjectServeContext {
    fn from_response_context(context: &HlsCacheResponseContext) -> Self {
        Self {
            cache_duration_seconds: context.cache_duration_seconds,
            metrics: Some(Arc::clone(&context.metrics)),
            segment_repair: Arc::clone(&context.segment_repair),
            qos_meter: Arc::clone(&context.qos_meter),
            media_activity_marker: context.media_activity_marker.clone(),
            now_ms: context.now_ms,
        }
    }
}

/// Serves a committed Ready segment from the HLS cache.
pub async fn serve_hls_segment_cache_response(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    segment_file: HlsSegmentFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> Response<Body> {
    match serve_hls_segment_cache_outcome(segment_cache, session, segment_file, range_header, context).await {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => hls_resource_failure_default_response(failure),
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
        CacheObjectLookup::Ready(object) => {
            HlsResourceServeOutcome::Ready(serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext::from_response_context(context),
            )
            .await)
        }
        CacheObjectLookup::Failure(failure) => HlsResourceServeOutcome::Failure(failure),
    }
}

/// Serves a committed Ready EXT-X-MAP object from the HLS cache.
pub async fn serve_hls_map_cache_response(
    segment_cache: Arc<HlsSegmentCache>,
    session: HlsSessionHandle,
    map_file: HlsMapFile,
    range_header: Option<HeaderValue>,
    context: &HlsCacheResponseContext,
) -> Response<Body> {
    match serve_hls_map_cache_outcome(segment_cache, session, map_file, range_header, context).await {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => hls_resource_failure_default_response(failure),
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
        CacheObjectLookup::Ready(object) => {
            HlsResourceServeOutcome::Ready(serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext::from_response_context(context),
            )
            .await)
        }
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
        CacheObjectLookup::Ready(object) => {
            HlsResourceServeOutcome::Ready(serve_cache_object(
                segment_cache,
                object,
                range_header,
                CacheObjectServeContext::from_response_context(context),
            )
            .await)
        }
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

async fn serve_cache_object<K>(
    segment_cache: Arc<HlsSegmentCache>,
    object: CacheObject<K>,
    range_header: Option<HeaderValue>,
    context: CacheObjectServeContext,
) -> Response<Body>
where
    K: super::HlsCacheObjectKey + Send + Sync + 'static,
{
    let guard = CacheReadGuard::new(Arc::clone(&object.access), context.now_ms);
    if let Some(repair_context) = object.repair_context.clone() {
        if let Err(err) = context
            .segment_repair
            .repair_ready_cache_hit(&segment_cache, &object.key, repair_context)
            .await
        {
            debug!(
                "HLS segment repair skipped for ready cache hit: session={} resource={} error={err}",
                object.log_context.session, object.log_context.resource_id
            );
        }
    }
    let metadata_started_at = Instant::now();
    let metadata = match segment_cache.metadata(&object.key).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    log_body_reader_wait_if_slow(&object.log_context, "metadata", metadata_started_at.elapsed().as_millis());

    let body_id = next_hls_body_log_id();
    let range = resolve_range(range_header.as_ref(), metadata.size);
    let (status, start, end, content_length) = match range {
        RangeDecision::Full => {
            if metadata.size == 0 {
                if let Some(metrics) = &context.metrics {
                    metrics.record_cache_hit();
                }
                return empty_ok_response(&object.content_type, context.cache_duration_seconds);
            }
            if let Some(metrics) = &context.metrics {
                metrics.record_cache_hit();
            }
            debug!(
                "HLS cache response prepared: body_id={} lease={} session={} resource={} source=cache range=full content_length={} content_type={}",
                body_id,
                object.log_context.lease,
                object.log_context.session,
                object.log_context.resource_id,
                metadata.size,
                object.content_type
            );
            (StatusCode::OK, 0, metadata.size - 1, metadata.size)
        }
        RangeDecision::Partial { start, end, length } => {
            if let Some(metrics) = &context.metrics {
                metrics.record_cache_hit();
                metrics.record_cache_range_hit();
            }
            debug!(
                "HLS cache response prepared: body_id={} lease={} session={} resource={} source=cache range={start}-{end} content_length={length} content_type={}",
                body_id,
                object.log_context.lease,
                object.log_context.session,
                object.log_context.resource_id,
                object.content_type
            );
            (StatusCode::PARTIAL_CONTENT, start, end, length)
        }
        RangeDecision::Unsatisfiable => return range_not_satisfiable_response(metadata.size),
    };

    let file_started_at = Instant::now();
    let file = match segment_cache.open_range(&object.key, start).await {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    log_body_reader_wait_if_slow(&object.log_context, "file", file_started_at.elapsed().as_millis());
    if let Some(marker) = &context.media_activity_marker {
        marker.mark_at(context.now_ms).await;
    }

    let stream = ReaderStream::new(file.take(content_length));
    let body_context = CacheBodyLogContext {
        session: object.log_context.session.clone(),
        resource_id: object.log_context.resource_id.clone(),
        object_kind: object.log_context.object_kind,
        source: object.log_context.body_source,
        content_length,
    };
    let stream = ActiveReaderStream::new(
        Box::pin(stream),
        guard,
        body_context,
        context.qos_meter,
        context.media_activity_marker,
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
    response
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
        | SegmentCacheStatus::Discovered => {
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
                session: safe_proxy_session_id(&session.proxy_session_id),
                resource_id: format!("{:06}", segment_file.proxy_seq),
                object_kind: "Segment",
                body_source: "normal",
            },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Normal,
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
            media_sequence: Some(entry.origin_key.origin_seq),
            discontinuity_sequence: Some(session.discontinuity_sequence),
            complete_object: entry.origin_byte_range.is_none(),
            encrypted: false,
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
        MapCacheStatus::Fetching { .. }
        | MapCacheStatus::Queued { .. }
        | MapCacheStatus::Discovered => {
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
            session: safe_proxy_session_id(&session.proxy_session_id),
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
    let resource_kind = session.transient.resources.get(&resource_file.resource_id).map(|resource| resource.kind);
    let Some(entry) = session.transient.ready_object(&key, now_ms) else {
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
            session: safe_proxy_session_id(&proxy_session_id),
            resource_id: resource_file.resource_id.0.clone(),
            object_kind: transient_body_object_kind(resource_kind, &resource_file.extension),
            body_source: "transient",
        },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Transient,
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
            encrypted: false,
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
    insert_header_value(
        headers,
        header::RETRY_AFTER,
        &super::retry_after_secs_from_ms(retry_after_ms).to_string(),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    mark_response_as_uncompressed(&mut response);
    response
}

fn resolve_range(range_header: Option<&HeaderValue>, full_size: u64) -> RangeDecision {
    let Some(range_header) = range_header else {
        return RangeDecision::Full;
    };
    let Ok(range_header) = range_header.to_str() else {
        return RangeDecision::Full;
    };
    let Some(range_spec) = range_header.strip_prefix("bytes=") else {
        return RangeDecision::Full;
    };
    if range_spec.contains(',') || full_size == 0 {
        return RangeDecision::Unsatisfiable;
    }

    let Some((start, end)) = range_spec.split_once('-') else {
        return RangeDecision::Unsatisfiable;
    };
    if start.is_empty() {
        return resolve_suffix_range(end, full_size);
    }
    resolve_start_range(start, end, full_size)
}

fn resolve_suffix_range(suffix_length: &str, full_size: u64) -> RangeDecision {
    let Ok(suffix_length) = suffix_length.parse::<u64>() else {
        return RangeDecision::Unsatisfiable;
    };
    if suffix_length == 0 {
        return RangeDecision::Unsatisfiable;
    }
    let length = suffix_length.min(full_size);
    let start = full_size - length;
    let end = full_size - 1;
    RangeDecision::Partial { start, end, length }
}

fn resolve_start_range(start: &str, end: &str, full_size: u64) -> RangeDecision {
    let Ok(start) = start.parse::<u64>() else {
        return RangeDecision::Unsatisfiable;
    };
    if start >= full_size {
        return RangeDecision::Unsatisfiable;
    }
    let end = if end.is_empty() {
        full_size - 1
    } else {
        let Ok(parsed_end) = end.parse::<u64>() else {
            return RangeDecision::Unsatisfiable;
        };
        parsed_end.min(full_size - 1)
    };
    if end < start {
        return RangeDecision::Unsatisfiable;
    }
    RangeDecision::Partial { start, end, length: end - start + 1 }
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
    _guard: CacheReadGuard,
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
}

impl ActiveReaderStream {
    fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        guard: CacheReadGuard,
        context: CacheBodyLogContext,
        meter: Arc<ArcSwapOption<StreamMeterHandle>>,
        media_activity_marker: Option<HlsMediaActivityMarker>,
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
        }
    }

    fn log_completed(&mut self, outcome: &'static str) {
        if self.completed_logged {
            return;
        }
        self.completed_logged = true;
        debug!(
            "{} '{}' body completed: session={} source={} elapsed_s={:.3} idle_max_s={:.3} bytes={}/{} outcome={}",
            self.context.object_kind,
            self.context.resource_id,
            self.context.session,
            self.context.source,
            duration_secs(self.started_at.elapsed().as_millis()),
            duration_secs(self.max_idle_ms),
            self.bytes_yielded,
            self.context.content_length,
            outcome
        );
        if let Some(marker) = &self.media_activity_marker {
            marker.spawn_mark_now();
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
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hls client body send timed out",
            ))));
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let idle_ms = self.last_yield_at.elapsed().as_millis();
                self.max_idle_ms = self.max_idle_ms.max(idle_ms);
                self.last_yield_at = Instant::now();
                self.bytes_yielded = self.bytes_yielded.saturating_add(chunk.len() as u64);
                if let Some(meter) = self.meter.load_full() {
                    meter.record_bytes(chunk.len() as u64);
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
    fn drop(&mut self) { self.access.reader_finished(); }
}

fn log_body_reader_wait_if_slow(context: &CacheObjectLogContext, wait_for: &'static str, elapsed_ms: u128) {
    if elapsed_ms < BODY_READER_WAIT_LOG_THRESHOLD_MS {
        return;
    }
    debug!(
        "HLS cache reader wait: lease={} session={} resource={} wait_for={} elapsed_ms={}",
        context.lease, context.session, context.resource_id, wait_for, elapsed_ms
    );
}

fn duration_secs(elapsed_ms: u128) -> f64 {
    Duration::from_millis(u64::try_from(elapsed_ms).unwrap_or(u64::MAX)).as_secs_f64()
}

fn transient_body_object_kind(resource_kind: Option<TransientResourceKind>, extension: &str) -> &'static str {
    match resource_kind {
        Some(TransientResourceKind::Key) => "Key",
        Some(TransientResourceKind::Map) => "Map",
        Some(TransientResourceKind::Segment | TransientResourceKind::Other) => "Segment",
        None => {
            if extension.eq_ignore_ascii_case("key") { "Key" } else { "Segment" }
        }
    }
}

fn next_hls_body_log_id() -> String {
    let value = NEXT_HLS_BODY_LOG_ID.fetch_add(1, Ordering::Relaxed);
    format!("{value:08x}")
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        resolve_range, serve_cache_object, transient_body_object_kind, CacheObject, CacheObjectLogContext,
        CacheObjectServeContext, HlsMediaActivityMarker, RangeDecision,
    };
    use crate::{
        api::model::{
            CacheAccessState, HlsProxyManager, HlsSegmentCache, HlsSegmentRepairManager, HlsSession, HlsSessionKey,
            ProxySessionId, SegmentCacheKey, TransientResourceKind,
        },
        model::{HlsSegmentRepairConfig, HlsSegmentRepairMode},
    };
    use arc_swap::ArcSwapOption;
    use axum::http::{header, HeaderValue, StatusCode};
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::RwLock;

    fn header(value: &str) -> HeaderValue { HeaderValue::from_str(value).expect("valid header") }

    fn test_segment_repair_manager() -> Arc<HlsSegmentRepairManager> {
        Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
            max_level: HlsSegmentRepairMode::Off,
            apply_to_first_segments: 1,
            max_parallel_repairs: 1,
            ..Default::default()
        }))
    }

    #[test]
    fn range_parser_resolves_open_ended_range() {
        assert_eq!(
            resolve_range(Some(&header("bytes=4-")), 10),
            RangeDecision::Partial { start: 4, end: 9, length: 6 }
        );
    }

    #[test]
    fn range_parser_resolves_closed_range() {
        assert_eq!(
            resolve_range(Some(&header("bytes=2-5")), 10),
            RangeDecision::Partial { start: 2, end: 5, length: 4 }
        );
    }

    #[test]
    fn range_parser_resolves_suffix_range() {
        assert_eq!(
            resolve_range(Some(&header("bytes=-3")), 10),
            RangeDecision::Partial { start: 7, end: 9, length: 3 }
        );
    }

    #[test]
    fn range_parser_rejects_unsatisfiable_and_multi_ranges() {
        assert_eq!(resolve_range(Some(&header("bytes=20-")), 10), RangeDecision::Unsatisfiable);
        assert_eq!(resolve_range(Some(&header("bytes=0-1,4-5")), 10), RangeDecision::Unsatisfiable);
    }

    #[test]
    fn range_parser_rejects_malformed_bytes_ranges() {
        assert_eq!(resolve_range(Some(&header("bytes=abc")), 10), RangeDecision::Unsatisfiable);
        assert_eq!(resolve_range(Some(&header("bytes=a-b")), 10), RangeDecision::Unsatisfiable);
    }

    #[test]
    fn range_parser_ignores_unknown_units() {
        assert_eq!(resolve_range(Some(&header("items=0-1")), 10), RangeDecision::Full);
    }

    #[test]
    fn transient_body_log_kind_uses_resource_kind() {
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Key), "bin"), "Key");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Map), "bin"), "Map");
        assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Segment), "key"), "Segment");
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
                session: "proxy-se...".to_string(),
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
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<crate::api::model::StreamMeterHandle>>)),
                media_activity_marker: None,
                now_ms: 1,
            },
        )
        .await;
        let second = serve_cache_object(
            segment_cache,
            CacheObject {
                log_context: CacheObjectLogContext {
                    lease: "lease-b".to_string(),
                    session: "proxy-se...".to_string(),
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
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<crate::api::model::StreamMeterHandle>>)),
                media_activity_marker: None,
                now_ms: 2,
            },
        )
        .await;

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
    async fn cache_body_marks_media_activity_at_start_and_body_end() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 12, "ts");
        segment_cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");
        let access = Arc::new(CacheAccessState::new());
        let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
        let marker = HlsMediaActivityMarker::new(Arc::clone(&hls_proxy), Arc::clone(&session));

        let response = serve_cache_object(
            segment_cache,
            CacheObject {
                key,
                access,
                content_type: "video/mp2t".to_string(),
                log_context: CacheObjectLogContext {
                    lease: "lease-a".to_string(),
                    session: "proxy-se...".to_string(),
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
                qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<crate::api::model::StreamMeterHandle>>)),
                media_activity_marker: Some(marker),
                now_ms: 1_000,
            },
        )
        .await;

        assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(1_000));

        assert_eq!(response.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"0123456789"));
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(
            session.read().await.activity.last_authorized_media_at_ms.expect("media activity") >= 1_000,
            "body completion should not move media activity backwards"
        );
    }
}
