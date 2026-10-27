use super::model::{OriginFetchOutcome, OriginFetchRecord};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Hard cap per input window; the oldest samples are dropped first.
const MAX_SAMPLES_PER_WINDOW: usize = 4096;

#[derive(Clone)]
struct Sample {
    ts_utc_ms: u64,
    outcome: OriginFetchOutcome,
    bytes_read: u64,
    queue_wait_ms: u64,
    first_byte_ms: Option<u64>,
    network_ms: Option<u64>,
    cache_write_ms: Option<u64>,
    postprocess_ms: Option<u64>,
    cache_ready_ms: Option<u64>,
    total_ms: u64,
}

pub(super) struct OriginAggregates {
    window_secs: u64,
    freshness_secs: u64,
    inputs: BTreeMap<String, VecDeque<Sample>>,
}

impl OriginAggregates {
    pub(super) fn new(window_secs: u64, freshness_secs: u64) -> Self {
        Self { window_secs, freshness_secs, inputs: BTreeMap::new() }
    }

    pub(super) fn record(&mut self, record: &OriginFetchRecord) {
        let window = self.inputs.entry(record.input_name.clone()).or_default();
        if window.len() >= MAX_SAMPLES_PER_WINDOW {
            window.pop_front();
        }
        window.push_back(Sample {
            ts_utc_ms: record.ts_utc_ms,
            outcome: record.outcome,
            bytes_read: record.bytes_read,
            queue_wait_ms: record.queue_wait_ms,
            first_byte_ms: record.first_byte_ms,
            network_ms: record.network_ms,
            cache_write_ms: record.cache_write_ms,
            postprocess_ms: record.postprocess_ms,
            cache_ready_ms: record.cache_ready_ms,
            total_ms: record.total_ms,
        });
    }

    /// Trims expired samples and copies what is left, so the caller can drop its lock before the percentile work.
    pub(super) fn collect(&mut self, now_ms: u64) -> OriginWindows {
        let window_ms = self.window_secs.saturating_mul(1000);
        self.inputs.retain(|_, window| {
            while window.front().is_some_and(|s| now_ms.saturating_sub(s.ts_utc_ms) > window_ms) {
                window.pop_front();
            }
            !window.is_empty()
        });
        OriginWindows {
            window_secs: self.window_secs,
            freshness_secs: self.freshness_secs,
            inputs: self.inputs.iter().map(|(name, window)| (name.clone(), window.iter().cloned().collect())).collect(),
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(
        &mut self,
        now_ms: u64,
        dropped_events: u64,
        queue_capacity: usize,
    ) -> OriginTelemetrySnapshot {
        self.collect(now_ms).into_snapshot(now_ms, dropped_events, queue_capacity)
    }
}

/// Copy of the rolling windows at one instant; building the snapshot from it needs no lock.
pub(super) struct OriginWindows {
    window_secs: u64,
    freshness_secs: u64,
    inputs: Vec<(String, Vec<Sample>)>,
}

impl OriginWindows {
    pub(super) fn into_snapshot(
        self,
        now_ms: u64,
        dropped_events: u64,
        queue_capacity: usize,
    ) -> OriginTelemetrySnapshot {
        let freshness_ms = self.freshness_secs.saturating_mul(1000);
        let inputs = self
            .inputs
            .into_iter()
            .map(|(input_name, window)| OriginInputSnapshot {
                availability: availability(&window, now_ms, self.window_secs),
                transfer_quality: transfer_quality(&window, now_ms, self.window_secs, freshness_ms),
                input_name,
            })
            .collect();
        OriginTelemetrySnapshot { scope: "source", dropped_events, queue_capacity, inputs }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OriginTelemetrySnapshot {
    pub scope: &'static str,
    pub dropped_events: u64,
    pub queue_capacity: usize,
    pub inputs: Vec<OriginInputSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OriginInputSnapshot {
    pub input_name: String,
    pub availability: OriginAvailability,
    pub transfer_quality: OriginTransferQuality,
}

/// Fetch outcomes in the window. Cancelled fetches are counted but excluded from `success_ratio`.
#[derive(Debug, Clone, Serialize)]
pub struct OriginAvailability {
    pub sample_count: usize,
    pub window_secs: u64,
    pub last_sample_age_ms: Option<u64>,
    /// Age of the oldest sample kept. Smaller than `window_secs` when the sample cap trimmed the window.
    pub window_covered_ms: Option<u64>,
    pub ok: usize,
    pub http_error: usize,
    pub timeout: usize,
    pub truncated: usize,
    pub cancelled: usize,
    pub transport_error: usize,
    pub success_ratio: Option<f64>,
}

/// Quality of completed transfers. Every value is `null` while there is no fresh sample, never `0`.
#[derive(Debug, Clone, Serialize)]
pub struct OriginTransferQuality {
    pub sample_count: usize,
    pub window_secs: u64,
    pub last_sample_age_ms: Option<u64>,
    pub network_ms_p50: Option<u64>,
    pub network_ms_p95: Option<u64>,
    pub first_byte_ms_p50: Option<u64>,
    pub first_byte_ms_p95: Option<u64>,
    pub queue_wait_ms_p95: Option<u64>,
    /// Cache-side time of completed fetches, reported apart from the origin transfer above.
    pub cache_write_ms_p95: Option<u64>,
    pub postprocess_ms_p95: Option<u64>,
    pub cache_ready_ms_p95: Option<u64>,
    /// Start-to-end time over every attempt except cancelled ones, so timeouts and truncations count.
    pub attempt_ms_p95_incl_failed: Option<u64>,
    pub throughput_kbps: Option<f64>,
}

fn availability(window: &[Sample], now_ms: u64, window_secs: u64) -> OriginAvailability {
    let (mut ok, mut http_error, mut timeout, mut truncated, mut cancelled, mut transport_error) = (0, 0, 0, 0, 0, 0);
    for sample in window {
        match sample.outcome {
            OriginFetchOutcome::Ok => ok += 1,
            OriginFetchOutcome::HttpError => http_error += 1,
            OriginFetchOutcome::Timeout => timeout += 1,
            OriginFetchOutcome::Truncated => truncated += 1,
            OriginFetchOutcome::Cancelled => cancelled += 1,
            OriginFetchOutcome::TransportError => transport_error += 1,
        }
    }
    let judged = ok + http_error + timeout + truncated + transport_error;
    OriginAvailability {
        sample_count: window.len(),
        window_secs,
        last_sample_age_ms: window.last().map(|s| now_ms.saturating_sub(s.ts_utc_ms)),
        window_covered_ms: window.first().map(|s| now_ms.saturating_sub(s.ts_utc_ms)),
        ok,
        http_error,
        timeout,
        truncated,
        cancelled,
        transport_error,
        #[allow(clippy::cast_precision_loss)]
        success_ratio: (judged > 0).then(|| ok as f64 / judged as f64),
    }
}

fn transfer_quality(window: &[Sample], now_ms: u64, window_secs: u64, freshness_ms: u64) -> OriginTransferQuality {
    let completed: Vec<&Sample> =
        window.iter().filter(|s| s.outcome == OriginFetchOutcome::Ok && s.network_ms.is_some()).collect();
    let last_sample_age_ms = completed.last().map(|s| now_ms.saturating_sub(s.ts_utc_ms));
    let mut quality = OriginTransferQuality {
        sample_count: completed.len(),
        window_secs,
        last_sample_age_ms,
        network_ms_p50: None,
        network_ms_p95: None,
        first_byte_ms_p50: None,
        first_byte_ms_p95: None,
        queue_wait_ms_p95: None,
        cache_write_ms_p95: None,
        postprocess_ms_p95: None,
        cache_ready_ms_p95: None,
        attempt_ms_p95_incl_failed: None,
        throughput_kbps: None,
    };
    // A run of failures must show up even when no transfer completed, so this has its own freshness check.
    let is_attempt = |s: &&Sample| s.outcome != OriginFetchOutcome::Cancelled;
    if window.iter().rfind(is_attempt).is_some_and(|s| now_ms.saturating_sub(s.ts_utc_ms) <= freshness_ms) {
        let mut totals: Vec<u64> = window.iter().filter(is_attempt).map(|s| s.total_ms).collect();
        quality.attempt_ms_p95_incl_failed = percentile(&mut totals, 95);
    }
    if last_sample_age_ms.is_none_or(|age| age > freshness_ms) {
        return quality;
    }
    let mut network: Vec<u64> = completed.iter().filter_map(|s| s.network_ms).collect();
    let mut first_byte: Vec<u64> = completed.iter().filter_map(|s| s.first_byte_ms).collect();
    let mut queue_wait: Vec<u64> = completed.iter().map(|s| s.queue_wait_ms).collect();
    quality.network_ms_p50 = percentile(&mut network, 50);
    quality.network_ms_p95 = percentile(&mut network, 95);
    quality.first_byte_ms_p50 = percentile(&mut first_byte, 50);
    quality.first_byte_ms_p95 = percentile(&mut first_byte, 95);
    quality.queue_wait_ms_p95 = percentile(&mut queue_wait, 95);
    quality.cache_write_ms_p95 = percentile_of(completed.iter().filter_map(|s| s.cache_write_ms), 95);
    quality.postprocess_ms_p95 = percentile_of(completed.iter().filter_map(|s| s.postprocess_ms), 95);
    quality.cache_ready_ms_p95 = percentile_of(completed.iter().filter_map(|s| s.cache_ready_ms), 95);
    let total_ms: u64 = network.iter().sum();
    let total_bytes: u64 = completed.iter().map(|s| s.bytes_read).sum();
    // bytes * 8 / ms == kbit/s
    #[allow(clippy::cast_precision_loss)]
    if total_ms > 0 {
        quality.throughput_kbps = Some(total_bytes as f64 * 8.0 / total_ms as f64);
    }
    quality
}

fn percentile_of(values: impl Iterator<Item = u64>, pct: usize) -> Option<u64> {
    percentile(&mut values.collect::<Vec<_>>(), pct)
}

/// Nearest-rank percentile; sorts `values` in place.
fn percentile(values: &mut [u64], pct: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * pct).div_ceil(100).max(1);
    values.get(rank - 1).copied()
}
