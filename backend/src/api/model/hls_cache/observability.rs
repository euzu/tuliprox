use super::{HlsAccessLeaseId, HlsSessionKey, ProxySessionId};
use sha2::{Digest, Sha256};
use shared::utils::sanitize_sensitive_info;
use std::sync::atomic::{AtomicU64, Ordering};

/// In-memory counters for the live HLS cache path.
#[derive(Debug, Default)]
pub struct HlsCacheMetrics {
    sessions_created: AtomicU64,
    sessions_reused: AtomicU64,
    lease_granted: AtomicU64,
    lease_denied: AtomicU64,
    refresh_started: AtomicU64,
    refresh_skipped: AtomicU64,
    refresh_completed: AtomicU64,
    refresh_retried: AtomicU64,
    refresh_failed: AtomicU64,
    transient_switches: AtomicU64,
    manifest_rendered: AtomicU64,
    manifest_render_skipped: AtomicU64,
    cache_hits: AtomicU64,
    cache_range_hits: AtomicU64,
    demand_fetch_started: AtomicU64,
    prefetch_queued: AtomicU64,
    prefetch_skipped: AtomicU64,
    segments_cached: AtomicU64,
    gc_runs: AtomicU64,
    segments_removed: AtomicU64,
    maps_removed: AtomicU64,
    secret_marker_mismatch: AtomicU64,
    secret_invalidation_deferred: AtomicU64,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct HlsCacheMetricsSnapshot {
    pub sessions_created: u64,
    pub sessions_reused: u64,
    pub lease_granted: u64,
    pub lease_denied: u64,
    pub refresh_started: u64,
    pub refresh_skipped: u64,
    pub refresh_completed: u64,
    pub refresh_retried: u64,
    pub refresh_failed: u64,
    pub transient_switches: u64,
    pub manifest_rendered: u64,
    pub manifest_render_skipped: u64,
    pub cache_hits: u64,
    pub cache_range_hits: u64,
    pub demand_fetch_started: u64,
    pub prefetch_queued: u64,
    pub prefetch_skipped: u64,
    pub segments_cached: u64,
    pub gc_runs: u64,
    pub segments_removed: u64,
    pub maps_removed: u64,
    pub secret_marker_mismatch: u64,
    pub secret_invalidation_deferred: u64,
}

impl HlsCacheMetrics {
    pub fn record_session_created(&self) { increment(&self.sessions_created, 1); }
    pub fn record_session_reused(&self) { increment(&self.sessions_reused, 1); }
    pub fn record_lease_granted(&self) { increment(&self.lease_granted, 1); }
    pub fn record_lease_denied(&self) { increment(&self.lease_denied, 1); }
    pub fn record_refresh_started(&self) { increment(&self.refresh_started, 1); }
    pub fn record_refresh_skipped(&self) { increment(&self.refresh_skipped, 1); }
    pub fn record_refresh_completed(&self) { increment(&self.refresh_completed, 1); }
    pub fn record_refresh_retried(&self) { increment(&self.refresh_retried, 1); }
    pub fn record_refresh_failed(&self) { increment(&self.refresh_failed, 1); }
    pub fn record_transient_switch(&self) { increment(&self.transient_switches, 1); }
    pub fn record_manifest_rendered(&self) { increment(&self.manifest_rendered, 1); }
    pub fn record_manifest_render_skipped(&self) { increment(&self.manifest_render_skipped, 1); }
    pub fn record_cache_hit(&self) { increment(&self.cache_hits, 1); }
    pub fn record_cache_range_hit(&self) { increment(&self.cache_range_hits, 1); }
    pub fn record_demand_fetch_started(&self) { increment(&self.demand_fetch_started, 1); }
    pub fn record_prefetch_queued(&self, count: usize) { increment(&self.prefetch_queued, count as u64); }
    pub fn record_prefetch_skipped(&self, count: usize) { increment(&self.prefetch_skipped, count as u64); }
    pub fn record_segment_cached(&self) { increment(&self.segments_cached, 1); }
    pub fn record_gc_run(&self) { increment(&self.gc_runs, 1); }
    pub fn record_segments_removed(&self, count: usize) { increment(&self.segments_removed, count as u64); }
    pub fn record_maps_removed(&self, count: usize) { increment(&self.maps_removed, count as u64); }
    pub fn record_secret_marker_mismatch(&self) { increment(&self.secret_marker_mismatch, 1); }
    pub fn record_secret_invalidation_deferred(&self) { increment(&self.secret_invalidation_deferred, 1); }

    pub fn snapshot(&self) -> HlsCacheMetricsSnapshot {
        HlsCacheMetricsSnapshot {
            sessions_created: load(&self.sessions_created),
            sessions_reused: load(&self.sessions_reused),
            lease_granted: load(&self.lease_granted),
            lease_denied: load(&self.lease_denied),
            refresh_started: load(&self.refresh_started),
            refresh_skipped: load(&self.refresh_skipped),
            refresh_completed: load(&self.refresh_completed),
            refresh_retried: load(&self.refresh_retried),
            refresh_failed: load(&self.refresh_failed),
            transient_switches: load(&self.transient_switches),
            manifest_rendered: load(&self.manifest_rendered),
            manifest_render_skipped: load(&self.manifest_render_skipped),
            cache_hits: load(&self.cache_hits),
            cache_range_hits: load(&self.cache_range_hits),
            demand_fetch_started: load(&self.demand_fetch_started),
            prefetch_queued: load(&self.prefetch_queued),
            prefetch_skipped: load(&self.prefetch_skipped),
            segments_cached: load(&self.segments_cached),
            gc_runs: load(&self.gc_runs),
            segments_removed: load(&self.segments_removed),
            maps_removed: load(&self.maps_removed),
            secret_marker_mismatch: load(&self.secret_marker_mismatch),
            secret_invalidation_deferred: load(&self.secret_invalidation_deferred),
        }
    }
}

pub fn safe_session_key(key: &HlsSessionKey) -> String { short_hash(&key.stable_value()) }

pub fn safe_hls_access_lease_id(lease_id: &HlsAccessLeaseId) -> String { short_hash(&lease_id.0) }

pub fn safe_user_session_token(session_token: &str) -> String { short_hash(session_token) }

pub fn safe_proxy_session_id(proxy_session_id: &ProxySessionId) -> String { short_hash(&proxy_session_id.0) }

pub fn safe_origin_log_value(value: impl AsRef<str>) -> String { sanitize_sensitive_info(value.as_ref()).into_owned() }

fn increment(counter: &AtomicU64, count: u64) { counter.fetch_add(count, Ordering::Relaxed); }

fn load(counter: &AtomicU64) -> u64 { counter.load(Ordering::Relaxed) }

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..4].iter().fold(String::with_capacity(8), |mut result, byte| {
        use std::fmt::Write as _;
        write!(result, "{byte:02x}").expect("writing to String must not fail");
        result
    })
}

#[cfg(test)]
mod tests {
    use super::{safe_origin_log_value, safe_proxy_session_id, safe_session_key, HlsCacheMetrics};
    use crate::api::model::{HlsSessionKey, ProxySessionId};

    #[test]
    fn safe_log_helpers_do_not_emit_credentials_or_full_proxy_session_id() {
        let sanitized = safe_origin_log_value("http://user:password@example.com/live/user/password/123.ts");
        assert!(!sanitized.contains("user:password"));
        assert!(!sanitized.contains("/user/password/"));

        let proxy_session_id = ProxySessionId("a8f31c9eQ7sLk92pV0mTaw".to_string());
        assert_eq!(safe_proxy_session_id(&proxy_session_id).len(), 8);
        assert!(!safe_proxy_session_id(&proxy_session_id).contains("Lk92pV0mTaw"));
    }

    #[test]
    fn session_key_log_value_is_a_stable_hash_not_raw_key() {
        let key = HlsSessionKey::new(1, "12345");
        let safe = safe_session_key(&key);

        assert_eq!(safe.len(), 8);
        assert!(!safe.contains("origin.example.com"));
        assert_eq!(safe, safe_session_key(&key));
    }

    #[test]
    fn metrics_snapshot_reports_recorded_counts() {
        let metrics = HlsCacheMetrics::default();

        metrics.record_session_created();
        metrics.record_prefetch_queued(2);
        metrics.record_prefetch_skipped(3);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.sessions_created, 1);
        assert_eq!(snapshot.prefetch_queued, 2);
        assert_eq!(snapshot.prefetch_skipped, 3);
    }
}
