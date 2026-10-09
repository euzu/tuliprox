use super::{
    hls_client_body_send_deadline,
    reader::{
        insert_cache_control, insert_header_value, insert_u64_header, next_hls_body_log_id,
        range_not_satisfiable_response,
    },
    refresh_hls_client_body_send_deadline, HlsCacheResponseContext, HlsResourceServeFailure, HlsResourceServeOutcome,
    HlsSegmentFile, HlsSessionHandle, RevisionResponseStream, ACCEPT_RANGES_VALUE,
};
use crate::{CachedSegmentMetadata, SegmentRevision, SegmentRevisionState};
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use futures::Stream;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::sleep;
use tuliprox_core::utils::{
    byte_range::{resolve_single_byte_range, SingleByteRange},
    current_time_millis,
    response_compression::mark_response_as_uncompressed,
};

pub(super) async fn serve_hls_revision_outcome(
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    range: Option<&HeaderValue>,
    context: &HlsCacheResponseContext,
) -> HlsResourceServeOutcome {
    let Some(marker) = &context.media_activity_marker else {
        return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Expired);
    };
    let deadline =
        remaining_wait_deadline(context.now_ms, marker.manager.segment_fetch_policy().origin_segment_timeout_ms);
    match tokio::time::timeout_at(
        deadline,
        prepare_hls_revision_response(session, segment_file, range, context, deadline),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            HlsResourceServeOutcome::Failure(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms: 1000 })
        }
    }
}

async fn prepare_hls_revision_response(
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    range: Option<&HeaderValue>,
    context: &HlsCacheResponseContext,
    deadline: tokio::time::Instant,
) -> HlsResourceServeOutcome {
    let Some(marker) = &context.media_activity_marker else {
        return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Expired);
    };
    let (lifetime, wait_ms) = {
        let session = session.read().await;
        let Some(startup) = &session.startup else {
            return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Expired);
        };
        (Duration::from_secs(startup.config.max_progressive_reader_lifetime_secs.get()), startup.first_data_timeout_ms)
    };
    let deadline = deadline.min(remaining_wait_deadline(context.now_ms, wait_ms));
    let (revision, progressive) = {
        let mut leases = marker.manager.access_leases().write().await;
        let Some(revision) = leases.published_segment_revision(&context.hls_access_lease_id, segment_file.proxy_seq)
        else {
            return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Missing);
        };
        let full_request = range.is_none_or(|range| range.as_bytes() == b"bytes=0-");
        let progressive = revision.revision().key.kind == super::super::SegmentRevisionKind::Raw
            && full_request
            && leases.try_claim_progressive_startup(
                &context.hls_access_lease_id,
                segment_file.proxy_seq,
                context.now_ms,
            );
        (revision, progressive)
    };
    let metadata = match revision_metadata(revision.revision(), progressive, deadline).await {
        Ok(metadata) => metadata,
        Err(failure) => return HlsResourceServeOutcome::Failure(failure),
    };
    let authorized_at_ms = current_time_millis();
    if !marker.manager.access_leases().write().await.media_identity_is_current(
        &marker.lease_id,
        &marker.proxy_session_id,
        marker.lease_identity,
        authorized_at_ms,
    ) {
        return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Expired);
    }
    let Some(body_range) = RevisionBodyRange::resolve(metadata.as_ref(), range) else {
        let size = metadata.as_ref().map_or(0, |metadata| metadata.size);
        return HlsResourceServeOutcome::Ready(range_not_satisfiable_response(size));
    };
    // Reject an unexposable progressive revision before the playback cursor and
    // startup observations record this request. `expose()` below still decides,
    // because the producer may exhaust its replay in between.
    if metadata.is_none() && !revision.revision().can_expose() {
        return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::TemporaryUnavailable {
            retry_after_ms: 1000,
        });
    }
    let completion = if body_range.full {
        let Some(cursor_marker) = marker.clone().for_segment_request(segment_file.proxy_seq, authorized_at_ms).await
        else {
            return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Expired);
        };
        cursor_marker.completed_segment_marker()
    } else {
        None
    };
    marker.mark_at(authorized_at_ms).await;
    marker.record_startup_segment_request(segment_file.proxy_seq, context.now_ms);
    marker.record_startup_repair_decision(segment_file.proxy_seq, current_time_millis());
    if metadata.is_none() && !revision.revision().expose() {
        return HlsResourceServeOutcome::Failure(HlsResourceServeFailure::TemporaryUnavailable {
            retry_after_ms: 1000,
        });
    }
    let observation =
        marker.begin_startup_cache_response(segment_file.proxy_seq, &next_hls_body_log_id(), current_time_millis());
    let stream = RevisionResponseStream {
        inner: super::super::progressive_startup::revision_body(revision, lifetime, body_range.start, body_range.end),
        qos_meter: Arc::clone(&context.qos_meter),
        completion,
        observation,
        first_chunk: true,
        finished: false,
        idle_deadline: Box::pin(sleep(hls_client_body_send_deadline())),
    };
    let mut response = Response::new(Body::from_stream(stream));
    body_range.apply_headers(&mut response, metadata.as_ref(), context.cache_duration_seconds);
    mark_response_as_uncompressed(&mut response);
    context.metrics.record_cache_hit();
    HlsResourceServeOutcome::Ready(marker.confirm_media_response(response))
}

fn remaining_wait_deadline(requested_at_ms: u64, wait_ms: u64) -> tokio::time::Instant {
    let elapsed_ms = current_time_millis().saturating_sub(requested_at_ms);
    tokio::time::Instant::now() + Duration::from_millis(wait_ms.saturating_sub(elapsed_ms))
}

/// A progressive claim streams a publishable pending revision; every other
/// request waits for the complete revision file.
async fn revision_metadata(
    revision: &SegmentRevision,
    progressive: bool,
    deadline: tokio::time::Instant,
) -> Result<Option<CachedSegmentMetadata>, HlsResourceServeFailure> {
    if progressive {
        return match revision.state() {
            SegmentRevisionState::Complete(metadata) => Ok(Some(metadata)),
            SegmentRevisionState::Pending if revision.is_publishable() => Ok(None),
            _ => Err(HlsResourceServeFailure::PermanentFailed { status: None }),
        };
    }
    match revision.wait_complete(deadline).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            Err(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms: 1000 })
        }
        Err(_) => Err(HlsResourceServeFailure::PermanentFailed { status: None }),
    }
}

/// Status and byte window of one revision response.
struct RevisionBodyRange {
    status: StatusCode,
    start: u64,
    end: Option<u64>,
    length: Option<u64>,
    /// The response covers the whole object, so it may complete the segment.
    full: bool,
}

impl RevisionBodyRange {
    /// `None` means the range cannot be satisfied. A pending revision has no
    /// known size yet and is always streamed whole.
    fn resolve(metadata: Option<&CachedSegmentMetadata>, range: Option<&HeaderValue>) -> Option<Self> {
        let Some(metadata) = metadata else {
            return Some(Self { status: StatusCode::OK, start: 0, end: None, length: None, full: true });
        };
        match resolve_single_byte_range(range, metadata.size) {
            SingleByteRange::Full => Some(Self {
                status: StatusCode::OK,
                start: 0,
                end: Some(metadata.size),
                length: Some(metadata.size),
                full: true,
            }),
            SingleByteRange::Partial { start, end, length } => Some(Self {
                status: StatusCode::PARTIAL_CONTENT,
                start,
                end: Some(end.saturating_add(1)),
                length: Some(length),
                full: start == 0 && length == metadata.size,
            }),
            SingleByteRange::Unsatisfiable => None,
        }
    }

    fn apply_headers(
        &self,
        response: &mut Response<Body>,
        metadata: Option<&CachedSegmentMetadata>,
        cache_duration_seconds: u64,
    ) {
        *response.status_mut() = self.status;
        insert_header_value(response.headers_mut(), header::CONTENT_TYPE, "video/mp2t");
        insert_cache_control(response.headers_mut(), cache_duration_seconds);
        if let Some(length) = self.length {
            insert_u64_header(response.headers_mut(), header::CONTENT_LENGTH, length);
        }
        if let Some(metadata) = metadata {
            response.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static(ACCEPT_RANGES_VALUE));
            if self.status == StatusCode::PARTIAL_CONTENT {
                insert_header_value(
                    response.headers_mut(),
                    header::CONTENT_RANGE,
                    &format!(
                        "bytes {}-{}/{}",
                        self.start,
                        self.end.unwrap_or(metadata.size).saturating_sub(1),
                        metadata.size
                    ),
                );
            }
        }
    }
}

impl RevisionResponseStream {
    pub(super) fn finish(&mut self, outcome: &'static str) {
        if !self.finished {
            self.finished = true;
            if let Some(observation) = &self.observation {
                observation.finish(current_time_millis(), outcome);
            }
        }
    }
}

impl Stream for RevisionResponseStream {
    type Item = io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        if self.idle_deadline.as_mut().poll(cx).is_ready() {
            self.finish("timeout");
            return Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::TimedOut, "HLS revision client body stalled"))));
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                refresh_hls_client_body_send_deadline(self.idle_deadline.as_mut());
                if let Some(meter) = self.qos_meter.load_full() {
                    meter.record_bytes(chunk.len() as u64);
                }
                if self.first_chunk {
                    self.first_chunk = false;
                    if let Some(observation) = &self.observation {
                        observation.record_first_chunk(current_time_millis());
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish("error");
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(marker) = self.completion.take() {
                    marker.spawn_mark_completion_now();
                }
                self.finish("completed");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for RevisionResponseStream {
    fn drop(&mut self) { self.finish("dropped"); }
}
