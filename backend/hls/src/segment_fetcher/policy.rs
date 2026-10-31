use super::{
    HlsSegmentFetchWorkload, SegmentFetchPolicy, DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES, DEFAULT_MAX_PREFETCH_QUEUE_DEPTH,
    DEFAULT_MAX_SESSION_SEGMENT_FETCHES, SEGMENT_FETCH_SCHEDULING_MARGIN_MS,
};
use std::time::Duration;

impl SegmentFetchPolicy {
    pub(crate) fn resource_retry_budget_ms(
        &self,
        kind: crate::HlsResourceFetchKind,
        source: crate::HlsResourceFetchSource,
        target_duration: Option<u32>,
    ) -> u64 {
        let config = self.timeouts.config();
        let transfer = config.snapshot(kind, target_duration).effective_ms;
        let postprocess =
            if source == crate::HlsResourceFetchSource::Normal && kind == crate::HlsResourceFetchKind::Segment {
                self.effective_repair_postprocess_timeout_ms
            } else {
                0
            };
        self.retry_chain_budget_ms(transfer, config.connect_ms, postprocess)
            .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS)
    }
    pub fn segment_timeout_ms(&self, target_duration: Option<u32>) -> u64 {
        self.timeouts.snapshot(crate::HlsResourceFetchKind::Segment, target_duration).effective_ms
    }
    pub fn manifest_timeout_ms(&self) -> u64 { self.timeouts.config().manifest_ms }

    fn retry_chain_budget_ms(&self, transfer_ms: u64, connect_ms: u64, postprocess_ms: u64) -> u64 {
        let attempts = u64::try_from(self.retry_delays_ms.len()).unwrap_or(u64::MAX);
        // Provider acquisition and connect are separate phases preceding the transfer.
        let per_attempt = transfer_ms.saturating_mul(2).saturating_add(connect_ms).saturating_add(postprocess_ms);
        let delays = self.retry_delays_ms.iter().copied().fold(0_u64, u64::saturating_add);
        attempts
            .saturating_mul(per_attempt)
            .saturating_add(delays)
            .saturating_add(attempts.saturating_mul(self.retry_jitter_max_ms))
    }

    pub fn workload_budget_ms_for(&self, workload: HlsSegmentFetchWorkload, target_duration: Option<u32>) -> u64 {
        let config = self.timeouts.config();
        let segment = config.snapshot(crate::HlsResourceFetchKind::Segment, target_duration).effective_ms;
        let media =
            self.retry_chain_budget_ms(segment, config.connect_ms, self.effective_repair_postprocess_timeout_ms);
        let key = if workload == HlsSegmentFetchWorkload::EncryptedWithKey {
            self.retry_chain_budget_ms(config.manifest_ms, config.connect_ms, 0)
        } else {
            0
        };
        media.saturating_add(key).saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS)
    }

    pub fn workload_budget_ms(&self, workload: HlsSegmentFetchWorkload) -> u64 {
        self.workload_budget_ms_for(workload, None)
    }
    pub fn demand_wait_timeout(&self) -> Duration {
        self.demand_wait_timeout_for(HlsSegmentFetchWorkload::EncryptedWithKey)
    }
    pub fn demand_wait_timeout_for(&self, workload: HlsSegmentFetchWorkload) -> Duration {
        self.demand_wait_timeout_for_session(workload, None)
    }
    pub fn demand_wait_timeout_for_session(
        &self,
        workload: HlsSegmentFetchWorkload,
        target_duration: Option<u32>,
    ) -> Duration {
        Duration::from_millis(self.workload_budget_ms_for(workload, target_duration))
    }
    pub fn transient_wait_timeout(&self, kind: crate::TransientResourceKind, target_duration: Option<u32>) -> Duration {
        let config = self.timeouts.config();
        let kind = crate::transient_fetcher::hls_transient_resource_fetch_kind(kind);
        let transfer = config.snapshot(kind, target_duration).effective_ms;
        Duration::from_millis(
            self.retry_chain_budget_ms(transfer, config.connect_ms, 0)
                .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS),
        )
    }

    pub fn origin_object_wait_timeout(&self) -> Duration {
        let config = self.timeouts.config();
        Duration::from_millis(
            self.retry_chain_budget_ms(config.manifest_ms, config.connect_ms, 0)
                .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS),
        )
    }

    pub fn retry_delay_ms(&self, attempt_index: usize) -> u64 {
        let base_delay_ms = self.retry_delays_ms[attempt_index];
        if self.retry_jitter_max_ms == 0 {
            return base_delay_ms;
        }
        let jitter_ms = fastrand::u64(0..=self.retry_jitter_max_ms);
        if fastrand::bool() {
            base_delay_ms.saturating_sub(jitter_ms)
        } else {
            base_delay_ms.saturating_add(jitter_ms)
        }
    }
}

impl Default for SegmentFetchPolicy {
    fn default() -> Self {
        Self {
            max_global_segment_fetches: DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES,
            max_session_segment_fetches: DEFAULT_MAX_SESSION_SEGMENT_FETCHES,
            max_prefetch_queue_depth: DEFAULT_MAX_PREFETCH_QUEUE_DEPTH,
            timeouts: crate::HlsOriginTimeoutPolicy::default(),
            effective_repair_postprocess_timeout_ms: 0,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            permanent_failure_segment_threshold: 3,
            telemetry: crate::origin_telemetry::OriginTelemetryRecorder::disabled(),
        }
    }
}
