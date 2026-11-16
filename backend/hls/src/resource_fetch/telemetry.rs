//! Origin-transfer telemetry for the shared retry loop.
//!
//! One `AttemptTelemetry` observes exactly one origin attempt. It wraps the decoded body in a counting
//! reader and classifies the final result; it never changes timeouts, retries or error values.
//! A disabled recorder yields a disabled `AttemptTelemetry` that wraps and records nothing.

use super::HlsOriginResourceFetchError;
use crate::origin_telemetry::{
    FetchProbe, OriginDelivery, OriginFetchOutcome, OriginFetchPhaseError, OriginResourceKind, OriginTelemetryRecorder,
};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    time::Instant,
};
use tuliprox_core::utils::{content_coding::DecodedHttpResponse, network::DynReader};

tokio::task_local! {
    /// The attempt whose commit callback is running, so repair code can mark its postprocess span.
    static CURRENT_ATTEMPT: AttemptTelemetry;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    /// No observed body exists yet.
    Absent,
    /// Read by the commit callback.
    Attached,
    /// The commit callback returned the body unread (direct stream); the reader finishes the record.
    Detached,
    Dropped,
}

struct AttemptState {
    /// `None` once the record has been emitted.
    probe: Option<FetchProbe>,
    headers_seen: bool,
    /// Decoded body length announced by the origin, when it is reliable.
    expected_len: Option<u64>,
    bytes: u64,
    body_done: bool,
    reader: ReaderState,
    was_detached: bool,
}

impl AttemptState {
    fn short_body(&self) -> bool { self.expected_len.is_some_and(|expected| self.bytes < expected) }

    fn delivery(&self) -> OriginDelivery {
        if self.was_detached || self.reader == ReaderState::Detached {
            OriginDelivery::Direct
        } else {
            OriginDelivery::Cache
        }
    }

    fn flush(&mut self, bytes: u64, first_byte_at: Option<Instant>, origin_wait: Duration) {
        let delta = bytes.saturating_sub(self.bytes);
        self.bytes = bytes;
        if let Some(probe) = self.probe.as_mut() {
            if let Some(first_byte_at) = first_byte_at {
                probe.mark_first_byte_at(first_byte_at);
            }
            if delta > 0 {
                probe.add_bytes(delta);
            }
            probe.set_body_origin_wait(origin_wait);
        }
    }

    /// The phase an interruption happened in.
    fn phase(&self) -> OriginFetchPhaseError {
        if !self.headers_seen {
            OriginFetchPhaseError::Headers
        } else if !self.body_done {
            OriginFetchPhaseError::Body
        } else {
            OriginFetchPhaseError::CacheWrite
        }
    }

    fn finish_complete(&mut self, cache_ready: bool) {
        let Some(mut probe) = self.probe.take() else {
            return;
        };
        probe.set_delivery(self.delivery());
        probe.mark_body_complete();
        probe.end_cache_write();
        if cache_ready {
            probe.mark_cache_ready();
        }
        if self.short_body() {
            probe.finish(OriginFetchOutcome::Truncated, Some(OriginFetchPhaseError::Body));
        } else {
            probe.finish(OriginFetchOutcome::Ok, None);
        }
    }

    fn finish_failed(&mut self, error: &HlsOriginResourceFetchError) {
        let Some(mut probe) = self.probe.take() else {
            return;
        };
        probe.set_delivery(self.delivery());
        if let (Some(status), false) = (error_status(error), self.headers_seen) {
            probe.mark_headers(status, None);
            self.headers_seen = true;
        }
        probe.end_cache_write();
        let (outcome, phase) =
            classify_error(error, self.phase(), self.headers_seen && !self.body_done && self.short_body());
        probe.finish(outcome, Some(phase));
    }
}

fn error_status(error: &HlsOriginResourceFetchError) -> Option<u16> {
    match error {
        HlsOriginResourceFetchError::PermanentStatus(status)
        | HlsOriginResourceFetchError::RetryableStatus(status)
        | HlsOriginResourceFetchError::NonRetryableStatus(status) => Some(status.as_u16()),
        _ => None,
    }
}

fn classify_error(
    error: &HlsOriginResourceFetchError,
    phase: OriginFetchPhaseError,
    truncated: bool,
) -> (OriginFetchOutcome, OriginFetchPhaseError) {
    use HlsOriginResourceFetchError as E;
    match error {
        E::PermanentStatus(_) | E::RetryableStatus(_) | E::NonRetryableStatus(_) | E::UnexpectedByteRangeStatus => {
            (OriginFetchOutcome::HttpError, OriginFetchPhaseError::Headers)
        }
        E::Timeout => (OriginFetchOutcome::Timeout, phase),
        E::Superseded => (OriginFetchOutcome::Cancelled, phase),
        E::Transport(_) if truncated => (OriginFetchOutcome::Truncated, OriginFetchPhaseError::Body),
        E::Transport(_) | E::Redirect => {
            // Transport errors before any header are connect failures.
            let phase = if phase == OriginFetchPhaseError::Headers { OriginFetchPhaseError::Connect } else { phase };
            (OriginFetchOutcome::TransportError, phase)
        }
        E::InvalidOriginUrl | E::InvalidByteRange | E::UnsupportedStartupMedia | E::ProviderUnavailable(_) => {
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Local)
        }
        E::ContentCoding(_) | E::ContentDecoding(_) => {
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::Body)
        }
        E::CacheObjectLimit { .. } | E::LocalCacheCapacity { .. } | E::CacheCommit(_) => {
            (OriginFetchOutcome::TransportError, OriginFetchPhaseError::CacheWrite)
        }
    }
}

/// Cheap clonable handle shared by the retry loop and the body reader.
#[derive(Clone, Default)]
pub(super) struct AttemptTelemetry(Option<Arc<Mutex<AttemptState>>>);

impl AttemptTelemetry {
    /// Starts the probe for one attempt; a disabled recorder yields a handle that records nothing.
    pub(super) fn begin(
        recorder: &OriginTelemetryRecorder,
        input_name: &str,
        kind: OriginResourceKind,
        cache_key: Option<&str>,
        attempt_index: usize,
    ) -> Self {
        let enabled = cache_key.is_some();
        let attempt_no = u32::try_from(attempt_index).unwrap_or(u32::MAX);
        Self::new(recorder.begin(input_name, kind, cache_key, attempt_no), enabled)
    }

    fn new(probe: FetchProbe, enabled: bool) -> Self {
        if !enabled {
            return Self::default();
        }
        Self(Some(Arc::new(Mutex::new(AttemptState {
            probe: Some(probe),
            headers_seen: false,
            expected_len: None,
            bytes: 0,
            body_done: false,
            reader: ReaderState::Absent,
            was_detached: false,
        }))))
    }

    fn update(&self, apply: impl FnOnce(&mut AttemptState)) {
        if let Some(mut state) = self.0.as_ref().and_then(|state| state.lock().ok()) {
            apply(&mut state);
        }
    }

    /// Records what is known before the request is sent.
    pub(super) fn start(&self, queue_wait: Duration, media_duration_ms: Option<u64>) {
        self.update(|state| {
            if let Some(probe) = state.probe.as_mut() {
                probe.set_queue_wait(queue_wait);
                if let Some(media_duration_ms) = media_duration_ms {
                    probe.set_media_duration_ms(media_duration_ms);
                }
            }
        });
    }

    pub(super) fn mark_headers(&self, status: u16, content_length: Option<u64>) {
        self.update(|state| {
            state.headers_seen = true;
            if let Some(probe) = state.probe.as_mut() {
                probe.mark_headers(status, content_length);
            }
        });
    }

    /// The commit callback returned.
    pub(super) fn finish<T>(&self, result: &Result<T, HlsOriginResourceFetchError>) {
        self.update(|state| match result {
            Err(error) => state.finish_failed(error),
            Ok(_) if state.reader == ReaderState::Attached && !state.body_done => {
                state.reader = ReaderState::Detached;
                state.was_detached = true;
            }
            Ok(_) => state.finish_complete(true),
        });
    }

    /// The attempt failed before a commit callback ran.
    pub(super) fn finish_failed(&self, error: &HlsOriginResourceFetchError) {
        self.update(|state| state.finish_failed(error));
    }

    /// Runs the commit callback with this attempt visible to `PostprocessSpan`.
    pub(super) async fn scope_commit<F: std::future::Future>(&self, commit: F) -> F::Output {
        match self.0 {
            Some(_) => CURRENT_ATTEMPT.scope(self.clone(), commit).await,
            None => commit.await,
        }
    }

    pub(super) fn wrap(&self, body: DynReader, expected_len: Option<u64>) -> DynReader {
        self.update(|state| {
            state.reader = ReaderState::Attached;
            state.expected_len = expected_len;
            if let Some(probe) = state.probe.as_mut() {
                probe.mark_body_observed();
            }
        });
        Box::pin(ObservedBody {
            inner: body,
            telemetry: self.clone(),
            expected_len,
            bytes: 0,
            first_byte_at: None,
            announced_end_at: None,
            pending_since: None,
            origin_wait: Duration::ZERO,
        })
    }

    fn on_eof(&self, bytes: u64, first_byte_at: Option<Instant>, origin_wait: Duration) {
        self.update(|state| {
            state.flush(bytes, first_byte_at, origin_wait);
            state.body_done = true;
            if let Some(probe) = state.probe.as_mut() {
                probe.mark_body_complete();
                // A direct stream has no cache write.
                if state.reader != ReaderState::Detached {
                    probe.start_cache_write();
                }
            }
            if state.reader == ReaderState::Detached {
                state.finish_complete(false);
            }
        });
    }

    fn on_read_error(&self, error: &io::Error, bytes: u64, first_byte_at: Option<Instant>, origin_wait: Duration) {
        self.update(|state| {
            state.flush(bytes, first_byte_at, origin_wait);
            if state.reader == ReaderState::Detached {
                state.finish_failed(&HlsOriginResourceFetchError::from_body_read_error(error));
            }
        });
    }

    fn on_reader_dropped(&self, read: ObservedRead) {
        self.update(|state| {
            state.flush(read.bytes, read.first_byte_at, read.origin_wait);
            let detached = state.reader == ReaderState::Detached || state.was_detached;
            state.reader = ReaderState::Dropped;
            if detached && state.probe.is_some() {
                // A client that stopped after the announced length still received the whole object.
                // It completed when the last announced byte was read, not when the reader was dropped.
                if state.expected_len.is_some() && !state.short_body() {
                    if let (Some(probe), Some(at)) = (state.probe.as_mut(), read.announced_end_at) {
                        probe.mark_body_complete_at(at);
                    }
                    state.finish_complete(false);
                } else if let Some(mut probe) = state.probe.take() {
                    probe.set_delivery(OriginDelivery::Direct);
                    probe.finish(OriginFetchOutcome::Cancelled, Some(state.phase()));
                }
            }
        });
    }
}

/// What the body reader observed, handed to the shared state when the reader is dropped.
#[derive(Clone, Copy)]
struct ObservedRead {
    bytes: u64,
    first_byte_at: Option<Instant>,
    /// When the announced length was reached; a client may drop the reader later without polling EOF.
    announced_end_at: Option<Instant>,
    origin_wait: Duration,
}

/// Counts body bytes and marks body completion for one attempt.
struct ObservedBody {
    inner: DynReader,
    telemetry: AttemptTelemetry,
    expected_len: Option<u64>,
    bytes: u64,
    first_byte_at: Option<Instant>,
    announced_end_at: Option<Instant>,
    pending_since: Option<Instant>,
    origin_wait: Duration,
}

impl AsyncRead for ObservedBody {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = self.inner.as_mut().poll_read(cx, buf);
        match &poll {
            Poll::Ready(Ok(())) => {
                if let Some(pending_since) = self.pending_since.take() {
                    self.origin_wait = self.origin_wait.saturating_add(pending_since.elapsed());
                }
                let read = buf.filled().len().saturating_sub(before) as u64;
                if read > 0 {
                    if self.first_byte_at.is_none() {
                        self.first_byte_at = Some(Instant::now());
                    }
                    self.bytes = self.bytes.saturating_add(read);
                    if self.announced_end_at.is_none() && self.expected_len.is_some_and(|len| self.bytes >= len) {
                        self.announced_end_at = Some(Instant::now());
                    }
                } else if buf.remaining() > 0 {
                    self.telemetry.on_eof(self.bytes, self.first_byte_at, self.origin_wait);
                }
            }
            Poll::Ready(Err(error)) => {
                if let Some(pending_since) = self.pending_since.take() {
                    self.origin_wait = self.origin_wait.saturating_add(pending_since.elapsed());
                }
                self.telemetry.on_read_error(error, self.bytes, self.first_byte_at, self.origin_wait);
            }
            Poll::Pending => {
                if self.pending_since.is_none() {
                    self.pending_since = Some(Instant::now());
                }
            }
        }
        poll
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        // After the announced last byte the transfer counts as complete at that moment, so a later open wait
        // is not origin wait. Incomplete and cancelled transfers keep it.
        if let Some(pending_since) = self.pending_since.take().filter(|_| self.announced_end_at.is_none()) {
            self.origin_wait = self.origin_wait.saturating_add(pending_since.elapsed());
        }
        self.telemetry.on_reader_dropped(ObservedRead {
            bytes: self.bytes,
            first_byte_at: self.first_byte_at,
            announced_end_at: self.announced_end_at,
            origin_wait: self.origin_wait,
        });
    }
}

/// Origin responses whose body the retry loop can observe.
pub(super) trait ObservableOriginResponse {
    fn observe_body(self, telemetry: &AttemptTelemetry) -> Self;
}

impl ObservableOriginResponse for DecodedHttpResponse {
    fn observe_body(mut self, telemetry: &AttemptTelemetry) -> Self {
        if telemetry.0.is_none() {
            return self;
        }
        let expected_len = crate::segment_fetcher::reliable_decoded_content_length(&self);
        let body = std::mem::replace(&mut self.body, Box::pin(tokio::io::empty()));
        self.body = telemetry.wrap(body, expected_len);
        self
    }
}

/// Marks the cache post-processing span of the origin attempt running in this task. Does nothing otherwise.
pub(crate) struct PostprocessSpan(Option<AttemptTelemetry>);

impl PostprocessSpan {
    pub(crate) fn start() -> Self {
        let attempt = CURRENT_ATTEMPT.try_with(Clone::clone).ok();
        if let Some(attempt) = &attempt {
            attempt.update(|state| {
                if let Some(probe) = state.probe.as_mut() {
                    // Postprocess follows the staged write; the write span ends where it starts.
                    probe.end_cache_write();
                    probe.start_postprocess();
                }
            });
        }
        Self(attempt)
    }
}

impl Drop for PostprocessSpan {
    fn drop(&mut self) {
        if let Some(attempt) = &self.0 {
            attempt.update(|state| {
                if let Some(probe) = state.probe.as_mut() {
                    probe.end_postprocess();
                }
            });
        }
    }
}
