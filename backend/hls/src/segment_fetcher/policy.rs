use super::{
    HlsSegmentFetchWorkload, SegmentFetchPolicy, DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES, DEFAULT_MAX_PREFETCH_QUEUE_DEPTH,
    DEFAULT_MAX_SESSION_SEGMENT_FETCHES, DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS, SEGMENT_FETCH_SCHEDULING_MARGIN_MS,
};
use std::time::Duration;

impl HlsSegmentFetchWorkload {
    const fn serialized_origin_objects(self) -> u64 {
        match self {
            Self::Clear => 1,
            Self::EncryptedWithKey => 2,
        }
    }
}

impl SegmentFetchPolicy {
    fn origin_object_retry_chain_budget_ms(&self) -> u64 {
        let attempts = u64::try_from(self.retry_delays_ms.len()).unwrap_or(u64::MAX);
        let per_attempt_budget =
            self.origin_segment_timeout_ms.saturating_add(self.effective_repair_postprocess_timeout_ms);
        let retry_delay_budget = self.retry_delays_ms.iter().copied().fold(0_u64, u64::saturating_add);
        let jitter_budget = attempts.saturating_mul(self.retry_jitter_max_ms);
        attempts.saturating_mul(per_attempt_budget).saturating_add(retry_delay_budget).saturating_add(jitter_budget)
    }

    pub fn workload_budget_ms(&self, workload: HlsSegmentFetchWorkload) -> u64 {
        self.origin_object_retry_chain_budget_ms()
            .saturating_mul(workload.serialized_origin_objects())
            .saturating_add(SEGMENT_FETCH_SCHEDULING_MARGIN_MS)
    }

    /// Conservative public wait bound for a segment whose encryption state is not known at the call site.
    pub fn demand_wait_timeout(&self) -> Duration {
        self.demand_wait_timeout_for(HlsSegmentFetchWorkload::EncryptedWithKey)
    }

    pub fn demand_wait_timeout_for(&self, workload: HlsSegmentFetchWorkload) -> Duration {
        Duration::from_millis(self.workload_budget_ms(workload))
    }

    /// Wait bound for one transient origin object such as a key or MAP.
    pub fn origin_object_wait_timeout(&self) -> Duration {
        self.demand_wait_timeout_for(HlsSegmentFetchWorkload::Clear)
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
            origin_segment_timeout_ms: DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS,
            effective_repair_postprocess_timeout_ms: 0,
            retry_delays_ms: [0, 100, 250, 500, 750],
            retry_jitter_max_ms: 100,
            permanent_failure_segment_threshold: 3,
        }
    }
}
