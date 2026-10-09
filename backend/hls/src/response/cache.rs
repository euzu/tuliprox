use super::{
    reader::{
        empty_ok_response, insert_cache_control, insert_header_value, insert_u64_header, log_body_reader_wait_if_slow,
        next_hls_body_log_id, range_not_satisfiable_response, service_unavailable_not_ready_response,
        transient_body_object_kind,
    },
    revision::serve_hls_revision_outcome,
    safe_hls_access_lease_id, ActiveReaderStream, CacheBodyLogContext, CacheObject, CacheObjectLogContext,
    CacheObjectServeContext, CacheReadGuard, HlsAccessLeaseId, HlsCacheMetrics, HlsCacheResponseContext,
    HlsLogIdentity, HlsMapFile, HlsMediaActivityMarker, HlsRepairRenderedObjectId, HlsSegmentCache, HlsSegmentFile,
    HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource, HlsSessionHandle, MapCacheKey,
    MapCacheStatus, ProxyMapId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey, TransientResourceFile,
    TransientResourceKind, ACCEPT_RANGES_VALUE, NOT_READY_RETRY_AFTER_MS,
};
use arc_swap::ArcSwapOption;
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use log::debug;
use std::{io, sync::Arc, time::Instant};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tuliprox_core::utils::{
    byte_range::{resolve_single_byte_range, SingleByteRange},
    current_time_millis,
    response_compression::mark_response_as_uncompressed,
};
use tuliprox_session::StreamMeterHandle;

pub(super) enum CacheObjectLookup<K> {
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

    pub fn set_qos_meter(&self, qos_meter: Option<Arc<StreamMeterHandle>>) { self.qos_meter.store(qos_meter); }

    pub async fn mark_media_activity(&self) {
        if let Some(marker) = &self.media_activity_marker {
            marker.mark_at(self.now_ms).await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsPlaybackCursorTracking {
    Disabled,
    Segment { proxy_seq: u64 },
}

impl HlsPlaybackCursorTracking {
    pub(super) fn full_object_proxy_seq(self, range: SingleByteRange, full_size: u64) -> Option<u64> {
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
    pub(super) fn from_response_context(context: &HlsCacheResponseContext) -> Self {
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
    if session.read().await.startup.is_some() {
        return serve_hls_revision_outcome(&session, &segment_file, range_header.as_ref(), context).await;
    }
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

pub(super) fn hls_resource_failure_default_response(failure: HlsResourceServeFailure) -> Response<Body> {
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

#[allow(clippy::too_many_lines)]
pub(super) async fn serve_cache_object<K>(
    segment_cache: Arc<HlsSegmentCache>,
    object: CacheObject<K>,
    range_header: Option<HeaderValue>,
    context: CacheObjectServeContext,
) -> Result<Response<Body>, HlsResourceServeFailure>
where
    K: super::super::HlsCacheObjectKey + Send + Sync + 'static,
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
    if object.is_media {
        if let Some(marker) = &context.media_activity_marker {
            response = marker.confirm_media_response(response);
        }
    }
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
        is_media: true,
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
        is_media: false,
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
    let key = super::super::TransientPassthroughState::transient_object_key(
        &proxy_session_id,
        &resource_file.resource_id,
        resource_file.extension.clone(),
    );
    let Some(resource) = session.transient.resolve_current_resource(&resource_file.resource_id, now_ms) else {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    };
    if resource.file_ext_hint.as_deref() != Some(resource_file.extension.as_str()) {
        return CacheObjectLookup::Failure(HlsResourceServeFailure::Missing);
    }
    let resource_state = (resource.kind, resource.encrypted_media);
    let resource_kind = Some(resource_state.0);
    let Some(entry) = session.transient.ready_object(&key, resource_state.0, now_ms) else {
        return match session.transient.object_cache.get(&key).map(|entry| &entry.status) {
            Some(super::super::TransientObjectCacheStatus::Fetching { .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                    retry_after_ms: NOT_READY_RETRY_AFTER_MS,
                })
            }
            Some(super::super::TransientObjectCacheStatus::FailedRetryable { retry_after_ms, .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::TemporaryUnavailable {
                    retry_after_ms: *retry_after_ms,
                })
            }
            Some(super::super::TransientObjectCacheStatus::FailedPermanent { status, .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::PermanentFailed { status: *status })
            }
            Some(super::super::TransientObjectCacheStatus::Ready { .. }) => {
                CacheObjectLookup::Failure(HlsResourceServeFailure::Expired)
            }
            None => CacheObjectLookup::Failure(HlsResourceServeFailure::Missing),
        };
    };
    CacheObjectLookup::Ready(CacheObject {
        is_media: matches!(resource_state.0, TransientResourceKind::Segment | TransientResourceKind::Part),
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
