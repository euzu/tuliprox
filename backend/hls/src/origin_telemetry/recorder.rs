use super::{
    aggregate::{OriginAggregates, OriginTelemetrySnapshot},
    model::{OriginFetchOutcome, OriginFetchPhaseError, OriginFetchRecord, OriginResourceKind},
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tuliprox_core::{model::HlsOriginTelemetryConfig, utils::current_time_millis};

/// Hex characters kept from the keyed hash; enough to tell segments apart, too short to be a cache key.
const SEGMENT_ID_HEX_LEN: usize = 16;

struct RecorderInner {
    tx: mpsc::Sender<OriginFetchRecord>,
    seq: AtomicU64,
    attempt_ids: AtomicU64,
    dropped_events: AtomicU64,
    queue_capacity: usize,
    hash_key: [u8; 32],
    aggregates: Arc<Mutex<OriginAggregates>>,
}

impl RecorderInner {
    fn segment_id(&self, cache_key: &str) -> String {
        let hash = blake3::keyed_hash(&self.hash_key, cache_key.as_bytes()).to_hex();
        hash[..SEGMENT_ID_HEX_LEN].to_string()
    }

    /// Never blocks: a full queue only increments `dropped_events`.
    fn submit(&self, mut record: OriginFetchRecord) {
        record.seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if self.tx.try_send(record).is_err() {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Cheap clonable handle. A disabled recorder holds no state and every call on it is a no-op.
#[derive(Clone, Default)]
pub struct OriginTelemetryRecorder {
    inner: Option<Arc<RecorderInner>>,
}

impl std::fmt::Debug for OriginTelemetryRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginTelemetryRecorder").field("enabled", &self.is_enabled()).finish()
    }
}

impl OriginTelemetryRecorder {
    #[must_use]
    pub fn disabled() -> Self { Self::default() }

    /// Builds the recorder and spawns the single consumer task. Disabled config yields a disabled recorder.
    /// The task ends when the last recorder handle is dropped.
    #[must_use]
    pub fn start(config: Option<&HlsOriginTelemetryConfig>) -> Self {
        let Some(config) = config.filter(|c| c.enabled) else {
            return Self::disabled();
        };
        let (recorder, mut rx) = Self::with_channel(config);
        let aggregates = recorder.inner.as_ref().map(|inner| Arc::clone(&inner.aggregates));
        if let Some(aggregates) = aggregates {
            tokio::spawn(async move {
                while let Some(record) = rx.recv().await {
                    if let Ok(mut guard) = aggregates.lock() {
                        guard.record(&record);
                    }
                }
            });
        }
        recorder
    }

    /// Builds an enabled recorder and hands back the receiving end without spawning a consumer.
    pub(crate) fn with_channel(config: &HlsOriginTelemetryConfig) -> (Self, mpsc::Receiver<OriginFetchRecord>) {
        let (tx, rx) = mpsc::channel(config.queue_capacity.max(1));
        let inner = RecorderInner {
            tx,
            seq: AtomicU64::new(0),
            attempt_ids: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            queue_capacity: config.queue_capacity,
            hash_key: rand::random(),
            aggregates: Arc::new(Mutex::new(OriginAggregates::new(config.window_secs, config.freshness_secs))),
        };
        (Self { inner: Some(Arc::new(inner)) }, rx)
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool { self.inner.is_some() }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.inner.as_ref().map_or(0, |inner| inner.dropped_events.load(Ordering::Relaxed))
    }

    /// `None` when telemetry is disabled.
    #[must_use]
    pub fn snapshot(&self) -> Option<OriginTelemetrySnapshot> { self.snapshot_at(current_time_millis()) }

    pub(super) fn snapshot_at(&self, now_ms: u64) -> Option<OriginTelemetrySnapshot> {
        let inner = self.inner.as_ref()?;
        // The lock only covers trimming and copying, so a snapshot cannot stall the consumer task.
        let windows = inner.aggregates.lock().ok()?.collect(now_ms);
        Some(windows.into_snapshot(now_ms, inner.dropped_events.load(Ordering::Relaxed), inner.queue_capacity))
    }

    /// Starts one probe per origin fetch attempt. Retries call this again with the same `cache_key`
    /// and a higher `attempt_no`, which yields the same `segment_id` and a new `attempt_id`.
    #[must_use]
    pub fn begin(
        &self,
        input_name: &str,
        resource_kind: OriginResourceKind,
        cache_key: Option<&str>,
        attempt_no: u32,
    ) -> FetchProbe {
        let Some(inner) = self.inner.as_ref() else {
            return FetchProbe::default();
        };
        FetchProbe {
            state: Some(Box::new(ProbeState {
                recorder: Arc::clone(inner),
                started: Instant::now(),
                input_name: input_name.to_string(),
                resource_kind,
                segment_id: cache_key.map(|key| inner.segment_id(key)),
                attempt_id: inner.attempt_ids.fetch_add(1, Ordering::Relaxed),
                attempt_no,
                http_status: None,
                bytes_read: 0,
                content_length: None,
                media_duration_ms: None,
                queue_wait_ms: 0,
                headers_ms: None,
                first_byte_ms: None,
                body_complete_ms: None,
                cache_write: Span::default(),
                postprocess: Span::default(),
                cache_ready_ms: None,
            })),
        }
    }
}

#[derive(Default)]
struct Span {
    started: Option<Instant>,
    ms: Option<u64>,
}

impl Span {
    fn start(&mut self) { self.started = Some(Instant::now()); }

    fn end(&mut self) {
        if let Some(started) = self.started.take() {
            self.ms = Some(duration_ms(started.elapsed()));
        }
    }
}

struct ProbeState {
    recorder: Arc<RecorderInner>,
    started: Instant,
    input_name: String,
    resource_kind: OriginResourceKind,
    segment_id: Option<String>,
    attempt_id: u64,
    attempt_no: u32,
    http_status: Option<u16>,
    bytes_read: u64,
    content_length: Option<u64>,
    media_duration_ms: Option<u64>,
    queue_wait_ms: u64,
    headers_ms: Option<u64>,
    first_byte_ms: Option<u64>,
    body_complete_ms: Option<u64>,
    cache_write: Span,
    postprocess: Span,
    cache_ready_ms: Option<u64>,
}

impl ProbeState {
    fn elapsed_ms(&self) -> u64 { duration_ms(self.started.elapsed()) }

    /// The phase reached so far, used when the caller does not name one.
    fn inferred_phase(&self) -> Option<OriginFetchPhaseError> {
        if self.headers_ms.is_none() {
            Some(OriginFetchPhaseError::Connect)
        } else if self.body_complete_ms.is_none() {
            Some(OriginFetchPhaseError::Body)
        } else {
            None
        }
    }

    fn into_record(
        self,
        outcome: OriginFetchOutcome,
        error_phase: Option<OriginFetchPhaseError>,
    ) -> (Arc<RecorderInner>, OriginFetchRecord) {
        let record = OriginFetchRecord {
            seq: 0,
            ts_utc_ms: current_time_millis(),
            input_name: self.input_name,
            resource_kind: self.resource_kind,
            segment_id: self.segment_id,
            attempt_id: self.attempt_id,
            attempt_no: self.attempt_no,
            http_status: self.http_status,
            bytes_read: self.bytes_read,
            content_length: self.content_length,
            media_duration_ms: self.media_duration_ms,
            queue_wait_ms: self.queue_wait_ms,
            headers_ms: self.headers_ms,
            first_byte_ms: self.first_byte_ms,
            body_complete_ms: self.body_complete_ms,
            // Start to last byte; cache write and postprocess are separate fields.
            network_ms: self.body_complete_ms,
            cache_write_ms: self.cache_write.ms,
            postprocess_ms: self.postprocess.ms,
            cache_ready_ms: self.cache_ready_ms,
            total_ms: duration_ms(self.started.elapsed()),
            outcome,
            error_phase,
        };
        (self.recorder, record)
    }
}

fn duration_ms(duration: Duration) -> u64 { u64::try_from(duration.as_millis()).unwrap_or(u64::MAX) }

/// Observes one origin fetch attempt. It only records timestamps; it never changes fetch behavior.
/// Dropping it without `finish` records the attempt as `Cancelled`. A default probe is a no-op.
#[derive(Default)]
pub struct FetchProbe {
    state: Option<Box<ProbeState>>,
}

impl FetchProbe {
    /// Time spent waiting for the semaphore or account lease before the request started.
    pub fn set_queue_wait(&mut self, wait: Duration) {
        if let Some(state) = self.state.as_mut() {
            state.queue_wait_ms = duration_ms(wait);
        }
    }

    pub fn mark_headers(&mut self, http_status: u16, content_length: Option<u64>) {
        if let Some(state) = self.state.as_mut() {
            state.headers_ms.get_or_insert_with(|| duration_ms(state.started.elapsed()));
            state.http_status = Some(http_status);
            state.content_length = content_length;
        }
    }

    /// Counts body bytes; the first non-empty chunk also marks the first byte.
    pub fn add_bytes(&mut self, bytes: u64) {
        if let Some(state) = self.state.as_mut() {
            if bytes > 0 && state.first_byte_ms.is_none() {
                state.first_byte_ms = Some(state.elapsed_ms());
            }
            state.bytes_read = state.bytes_read.saturating_add(bytes);
        }
    }

    pub fn mark_body_complete(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.body_complete_ms.get_or_insert_with(|| duration_ms(state.started.elapsed()));
        }
    }

    pub fn start_cache_write(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.cache_write.start();
        }
    }

    pub fn end_cache_write(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.cache_write.end();
        }
    }

    pub fn start_postprocess(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.postprocess.start();
        }
    }

    pub fn end_postprocess(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.postprocess.end();
        }
    }

    /// The object became servable from cache. Reported separately from body completion.
    pub fn mark_cache_ready(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.cache_ready_ms.get_or_insert_with(|| duration_ms(state.started.elapsed()));
        }
    }

    pub fn set_media_duration_ms(&mut self, media_duration_ms: u64) {
        if let Some(state) = self.state.as_mut() {
            state.media_duration_ms = Some(media_duration_ms);
        }
    }

    /// Emits the record. `error_phase` is inferred from the marks reached when `None` and the outcome is not `Ok`.
    pub fn finish(mut self, outcome: OriginFetchOutcome, error_phase: Option<OriginFetchPhaseError>) {
        self.emit(outcome, error_phase);
    }

    fn emit(&mut self, outcome: OriginFetchOutcome, error_phase: Option<OriginFetchPhaseError>) {
        if let Some(state) = self.state.take() {
            let error_phase =
                if outcome == OriginFetchOutcome::Ok { None } else { error_phase.or_else(|| state.inferred_phase()) };
            let (recorder, record) = state.into_record(outcome, error_phase);
            recorder.submit(record);
        }
    }
}

impl Drop for FetchProbe {
    fn drop(&mut self) { self.emit(OriginFetchOutcome::Cancelled, None); }
}
