use super::{
    hls_client_body_send_deadline, refresh_hls_client_body_send_deadline, ActiveReaderStream, CacheAccessState,
    CacheBodyLogContext, CacheObjectLogContext, CacheReadGuard, HlsMediaActivityMarker, HlsStartupBodyObservation,
    PreparedBytesStream, TransientResourceKind, ACCEPT_RANGES_VALUE, BODY_READER_WAIT_LOG_THRESHOLD_MS,
    NEXT_HLS_BODY_LOG_ID, PREPARED_MEDIA_CHUNK_SIZE,
};
use arc_swap::ArcSwapOption;
use axum::{
    body::Body,
    http::{header, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use futures::Stream;
use log::debug;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tuliprox_core::utils::{current_time_millis, response_compression::mark_response_as_uncompressed};
use tuliprox_session::StreamMeterHandle;

pub(super) fn empty_ok_response(content_type: &str, cache_duration_seconds: u64) -> Response<Body> {
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

pub(super) fn range_not_satisfiable_response(full_size: u64) -> Response<Body> {
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

pub(super) fn service_unavailable_not_ready_response(retry_after_ms: u64) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    insert_header_value(
        headers,
        header::RETRY_AFTER,
        &super::super::retry_after_secs_from_ms(retry_after_ms).to_string(),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    mark_response_as_uncompressed(&mut response);
    response
}

pub(super) fn insert_cache_control(headers: &mut axum::http::HeaderMap, cache_duration_seconds: u64) {
    insert_header_value(
        headers,
        header::CACHE_CONTROL,
        &format!("public, max-age={cache_duration_seconds}, immutable"),
    );
}

pub(super) fn insert_u64_header(headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: u64) {
    insert_header_value(headers, name, &value.to_string());
}

pub(super) fn insert_header_value(headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

impl PreparedBytesStream {
    pub(super) fn new(bytes: Bytes) -> Self { Self { remaining: bytes } }
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
    pub(super) fn new(
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

    pub(super) fn log_completed(&mut self, outcome: &'static str) {
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

impl CacheReadGuard {
    pub(super) fn new(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for CacheReadGuard {
    fn drop(&mut self) { self.access.reader_finished(); }
}

pub(super) fn log_body_reader_wait_if_slow(context: &CacheObjectLogContext, wait_for: &'static str, elapsed_ms: u128) {
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

pub(super) fn transient_body_object_kind(
    resource_kind: Option<TransientResourceKind>,
    extension: &str,
) -> &'static str {
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

pub(super) fn next_hls_body_log_id() -> String {
    let value = NEXT_HLS_BODY_LOG_ID.fetch_add(1, Ordering::Relaxed);
    format!("{value:08x}")
}
