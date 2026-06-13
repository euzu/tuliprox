#![allow(clippy::large_futures, clippy::large_enum_variant, clippy::too_many_lines)]

use super::{
    safe_hls_access_lease_id, safe_proxy_session_id, CacheAccessState, HlsAccessLeaseId, HlsCacheMetrics,
    HlsMapFile, HlsRepairRenderedObjectId, HlsSegmentCache, HlsSegmentFile, HlsSegmentRepairManager,
    HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, MapCacheKey, MapCacheStatus, ProxyMapId, SegmentCacheKey,
    SegmentCacheStatus, TransientObjectCacheKey, TransientResourceFile,
};
use crate::api::api_utils::mark_response_as_uncompressed;
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
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

const ACCEPT_RANGES_VALUE: &str = "bytes";
const NOT_READY_RETRY_AFTER_SECS: &str = "1";
const BODY_READER_WAIT_LOG_THRESHOLD_MS: u128 = 10;
const BODY_CHUNK_YIELD_DELAY_LOG_THRESHOLD_MS: u128 = 1_000;
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
}

#[derive(Clone)]
struct CacheBodyLogContext {
    body_id: String,
    lease: String,
    session: String,
    resource_id: String,
    source: &'static str,
    range: String,
    content_length: u64,
}

enum CacheObjectLookup<K> {
    Ready(CacheObject<K>),
    NotReady,
    Missing,
}

pub struct HlsCacheResponseContext {
    pub hls_access_lease_id: HlsAccessLeaseId,
    pub cache_duration_seconds: u64,
    pub metrics: Arc<HlsCacheMetrics>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub now_ms: u64,
}

impl HlsCacheResponseContext {
    pub fn new(
        hls_access_lease_id: HlsAccessLeaseId,
        cache_duration_seconds: u64,
        metrics: Arc<HlsCacheMetrics>,
        segment_repair: Arc<HlsSegmentRepairManager>,
        now_ms: u64,
    ) -> Self {
        Self {
            hls_access_lease_id,
            cache_duration_seconds,
            metrics,
            segment_repair,
            now_ms,
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
    match lookup_segment_cache_object(&session, &segment_file, &context.hls_access_lease_id).await {
        CacheObjectLookup::Ready(object) => {
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                context.cache_duration_seconds,
                Some(Arc::clone(&context.metrics)),
                Arc::clone(&context.segment_repair),
                context.now_ms,
            )
            .await
        }
        CacheObjectLookup::NotReady => service_unavailable_not_ready_response(),
        CacheObjectLookup::Missing => StatusCode::NOT_FOUND.into_response(),
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
    match lookup_map_cache_object(&session, &map_file, &context.hls_access_lease_id).await {
        CacheObjectLookup::Ready(object) => {
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                context.cache_duration_seconds,
                Some(Arc::clone(&context.metrics)),
                Arc::clone(&context.segment_repair),
                context.now_ms,
            )
            .await
        }
        CacheObjectLookup::NotReady => service_unavailable_not_ready_response(),
        CacheObjectLookup::Missing => StatusCode::NOT_FOUND.into_response(),
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
    match lookup_transient_object_cache_object(
        &session,
        &resource_file,
        &context.hls_access_lease_id,
        context.now_ms,
    )
    .await
    {
        CacheObjectLookup::Ready(object) => {
            serve_cache_object(
                segment_cache,
                object,
                range_header,
                context.cache_duration_seconds,
                Some(Arc::clone(&context.metrics)),
                Arc::clone(&context.segment_repair),
                context.now_ms,
            )
            .await
        }
        CacheObjectLookup::NotReady => service_unavailable_not_ready_response(),
        CacheObjectLookup::Missing => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_cache_object<K>(
    segment_cache: Arc<HlsSegmentCache>,
    object: CacheObject<K>,
    range_header: Option<HeaderValue>,
    cache_duration_seconds: u64,
    metrics: Option<Arc<HlsCacheMetrics>>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    now_ms: u64,
) -> Response<Body>
where
    K: super::HlsCacheObjectKey + Send + Sync + 'static,
{
    let guard = CacheReadGuard::new(Arc::clone(&object.access), now_ms);
    if let Some(repair_context) = object.repair_context.clone() {
        if let Err(err) = segment_repair
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
                if let Some(metrics) = &metrics {
                    metrics.record_cache_hit();
                }
                return empty_ok_response(&object.content_type, cache_duration_seconds);
            }
            if let Some(metrics) = &metrics {
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
            if let Some(metrics) = &metrics {
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

    let stream = ReaderStream::new(file.take(content_length));
    let body_context = CacheBodyLogContext {
        body_id,
        lease: object.log_context.lease.clone(),
        session: object.log_context.session.clone(),
        resource_id: object.log_context.resource_id.clone(),
        source: "cache",
        range: if status == StatusCode::PARTIAL_CONTENT { format!("{start}-{end}") } else { "full".to_string() },
        content_length,
    };
    debug!(
        "HLS body stream created: body_id={} lease={} session={} resource={} source={} range={} content_length={}",
        body_context.body_id,
        body_context.lease,
        body_context.session,
        body_context.resource_id,
        body_context.source,
        body_context.range,
        body_context.content_length
    );
    let stream = ActiveReaderStream::new(Box::pin(stream), guard, body_context);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    insert_header_value(headers, header::CONTENT_TYPE, &object.content_type);
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static(ACCEPT_RANGES_VALUE));
    insert_u64_header(headers, header::CONTENT_LENGTH, content_length);
    insert_cache_control(headers, cache_duration_seconds);
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
        return CacheObjectLookup::Missing;
    }
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return CacheObjectLookup::Missing;
    };
    if entry.proxy_file_ext != segment_file.extension {
        return CacheObjectLookup::Missing;
    }
    if !matches!(entry.status, SegmentCacheStatus::Ready { .. }) {
        return CacheObjectLookup::NotReady;
    }
    CacheObjectLookup::Ready(CacheObject {
        key: entry.cache_key.clone(),
        access: Arc::clone(&entry.access),
        content_type: entry.content_type.clone(),
        log_context: CacheObjectLogContext {
            lease: safe_hls_access_lease_id(hls_access_lease_id),
            session: safe_proxy_session_id(&session.proxy_session_id),
            resource_id: format!("{:06}", segment_file.proxy_seq),
        },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Normal,
            proxy_session_id: session.proxy_session_id.clone(),
            hls_access_lease_id: Some(hls_access_lease_id.clone()),
            rendered_object_id: HlsRepairRenderedObjectId::Normal { proxy_seq: segment_file.proxy_seq },
            resource_id: format!("{:06}", segment_file.proxy_seq),
            file_ext: entry.proxy_file_ext.clone(),
            normalized_origin_uri: entry
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
        return CacheObjectLookup::Missing;
    }
    let Some(entry) = session.maps.get(&ProxyMapId(map_file.proxy_map_id)) else {
        return CacheObjectLookup::Missing;
    };
    if entry.proxy_file_ext != map_file.extension {
        return CacheObjectLookup::Missing;
    }
    if !matches!(entry.status, MapCacheStatus::Ready { .. }) {
        return CacheObjectLookup::NotReady;
    }
    CacheObjectLookup::Ready(CacheObject {
        key: entry.cache_key.clone(),
        access: Arc::clone(&entry.access),
        content_type: entry.content_type.clone(),
        log_context: CacheObjectLogContext {
            lease: safe_hls_access_lease_id(hls_access_lease_id),
            session: safe_proxy_session_id(&session.proxy_session_id),
            resource_id: format!("map:{:06}", map_file.proxy_map_id),
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
        return CacheObjectLookup::Missing;
    }
    let proxy_session_id = session.proxy_session_id.clone();
    let key = super::TransientPassthroughState::transient_object_key(
        &proxy_session_id,
        &resource_file.resource_id,
        resource_file.extension.clone(),
    );
    let Some(entry) = session.transient.ready_object(&key, now_ms) else {
        return match session.transient.object_cache.get(&key).map(|entry| &entry.status) {
            Some(super::TransientObjectCacheStatus::Fetching { .. } | super::TransientObjectCacheStatus::Failed { .. }) => {
                CacheObjectLookup::NotReady
            }
            Some(super::TransientObjectCacheStatus::Ready { .. }) | None => CacheObjectLookup::Missing,
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
        },
        repair_context: Some(HlsSegmentRepairObjectContext {
            source: HlsSegmentRepairSource::Transient,
            proxy_session_id,
            hls_access_lease_id: Some(hls_access_lease_id.clone()),
            rendered_object_id: HlsRepairRenderedObjectId::Transient { resource_id: resource_file.resource_id.0.clone() },
            resource_id: resource_file.resource_id.0.clone(),
            file_ext: resource_file.extension.clone(),
            normalized_origin_uri: resource_file.resource_id.0.clone(),
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

fn service_unavailable_not_ready_response() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static(NOT_READY_RETRY_AFTER_SECS));
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
    first_chunk_logged: bool,
    terminal_logged: bool,
    bytes_yielded: u64,
}

impl ActiveReaderStream {
    fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        guard: CacheReadGuard,
        context: CacheBodyLogContext,
    ) -> Self {
        Self {
            inner,
            _guard: guard,
            context,
            started_at: Instant::now(),
            last_yield_at: Instant::now(),
            first_chunk_logged: false,
            terminal_logged: false,
            bytes_yielded: 0,
        }
    }
}

impl Stream for ActiveReaderStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if self.first_chunk_logged {
                    let idle_ms = self.last_yield_at.elapsed().as_millis();
                    if idle_ms >= BODY_CHUNK_YIELD_DELAY_LOG_THRESHOLD_MS {
                        debug!(
                            "HLS body chunk yield delayed: body_id={} lease={} session={} resource={} idle_ms={} bytes_yielded={} expected_bytes={}",
                            self.context.body_id,
                            self.context.lease,
                            self.context.session,
                            self.context.resource_id,
                            idle_ms,
                            self.bytes_yielded,
                            self.context.content_length
                        );
                    }
                } else {
                    self.first_chunk_logged = true;
                    debug!(
                        "HLS body first chunk yielded: body_id={} lease={} session={} resource={} elapsed_ms={} chunk_len={}",
                        self.context.body_id,
                        self.context.lease,
                        self.context.session,
                        self.context.resource_id,
                        self.started_at.elapsed().as_millis(),
                        chunk.len()
                    );
                }
                self.last_yield_at = Instant::now();
                self.bytes_yielded = self.bytes_yielded.saturating_add(chunk.len() as u64);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.terminal_logged = true;
                debug!(
                    "HLS body stream error: body_id={} lease={} session={} resource={} elapsed_ms={} bytes_yielded={} expected_bytes={} error={}",
                    self.context.body_id,
                    self.context.lease,
                    self.context.session,
                    self.context.resource_id,
                    self.started_at.elapsed().as_millis(),
                    self.bytes_yielded,
                    self.context.content_length,
                    err
                );
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                self.terminal_logged = true;
                debug!(
                    "HLS body source exhausted: body_id={} lease={} session={} resource={} elapsed_ms={} bytes_yielded={} expected_bytes={} client_delivery=unknown",
                    self.context.body_id,
                    self.context.lease,
                    self.context.session,
                    self.context.resource_id,
                    self.started_at.elapsed().as_millis(),
                    self.bytes_yielded,
                    self.context.content_length
                );
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ActiveReaderStream {
    fn drop(&mut self) {
        if self.terminal_logged {
            return;
        }
        if self.bytes_yielded >= self.context.content_length {
            debug!(
                "HLS body stream dropped after source exhausted: body_id={} lease={} session={} resource={} elapsed_ms={} bytes_yielded={} expected_bytes={} client_delivery=unknown",
                self.context.body_id,
                self.context.lease,
                self.context.session,
                self.context.resource_id,
                self.started_at.elapsed().as_millis(),
                self.bytes_yielded,
                self.context.content_length
            );
        } else {
            debug!(
                "HLS body stream dropped before source exhausted: body_id={} lease={} session={} resource={} elapsed_ms={} bytes_yielded={} expected_bytes={}",
                self.context.body_id,
                self.context.lease,
                self.context.session,
                self.context.resource_id,
                self.started_at.elapsed().as_millis(),
                self.bytes_yielded,
                self.context.content_length
            );
        }
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
        "HLS body reader wait: lease={} session={} resource={} wait_for={} elapsed_ms={}",
        context.lease, context.session, context.resource_id, wait_for, elapsed_ms
    );
}

fn next_hls_body_log_id() -> String {
    let value = NEXT_HLS_BODY_LOG_ID.fetch_add(1, Ordering::Relaxed);
    format!("{value:08x}")
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_range, serve_cache_object, CacheObject, CacheObjectLogContext, RangeDecision,
    };
    use crate::{
        api::model::{CacheAccessState, HlsSegmentCache, HlsSegmentRepairManager, ProxySessionId, SegmentCacheKey},
        model::{HlsSegmentRepairConfig, HlsSegmentRepairMode},
    };
    use axum::http::{HeaderValue, StatusCode};
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use std::sync::Arc;

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
            content_type: "video/MP2T".to_string(),
            log_context: CacheObjectLogContext {
                lease: "lease-a".to_string(),
                session: "proxy-se...".to_string(),
                resource_id: "000012".to_string(),
            },
            repair_context: None,
        };

        let first = serve_cache_object(
            Arc::clone(&segment_cache),
            object.clone(),
            Some(header("bytes=0-")),
            300,
            None,
            test_segment_repair_manager(),
            1,
        )
        .await;
        let second = serve_cache_object(
            segment_cache,
            CacheObject {
                log_context: CacheObjectLogContext {
                    lease: "lease-b".to_string(),
                    session: "proxy-se...".to_string(),
                    resource_id: "000012".to_string(),
                },
                ..object
            },
            Some(header("bytes=0-")),
            300,
            None,
            test_segment_repair_manager(),
            2,
        )
        .await;

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(access.active_readers(), 2);

        let (first_body, second_body) = tokio::join!(
            first.into_body().collect(),
            second.into_body().collect(),
        );

        assert_eq!(first_body.expect("first body").to_bytes(), Bytes::from_static(b"0123456789"));
        assert_eq!(second_body.expect("second body").to_bytes(), Bytes::from_static(b"0123456789"));
        assert_eq!(access.active_readers(), 0);
        assert_eq!(access.last_accessed_at_ms(), 2);
    }
}
