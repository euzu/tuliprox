use super::{
    reader::{insert_header_value, insert_u64_header, next_hls_body_log_id, range_not_satisfiable_response},
    ActiveReaderStream, CacheBodyLogContext, FiniteBytesSelection, HlsCacheResponseContext, HlsMediaActivityMarker,
    PreparedBytesStream, ProxySessionId, ACCEPT_RANGES_VALUE,
};
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use std::sync::Arc;
use tuliprox_core::utils::{
    byte_range::{resolve_single_byte_range, SingleByteRange},
    response_compression::mark_response_as_uncompressed,
};

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
