use super::{
    manifest_acceptance::HlsManifestAcceptanceTrigger,
    media_reserve::HlsLeasePlaybackCursor,
    origin_progress::{HlsOriginPathCondition, HlsOriginProgressPhase},
    HlsAccessLeaseId, HlsSession, HlsSessionKey, ProxySessionId,
};
use sha2::{Digest, Sha256};
use shared::utils::sanitize_sensitive_info;
use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
pub use tuliprox_core::utils::content_coding::{
    log_hls_origin_content_coding, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
};

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

/// Canonical HLS log identity source. Raw identifiers remain private and are
/// converted to safe short values only at the logging boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct HlsLogIdentity {
    session_key: Arc<str>,
    proxy_session_id: Arc<str>,
}

impl HlsLogIdentity {
    pub fn new(session_key: &HlsSessionKey, proxy_session_id: &ProxySessionId) -> Self {
        Self {
            session_key: Arc::from(session_key.stable_value()),
            proxy_session_id: Arc::from(proxy_session_id.0.as_str()),
        }
    }

    pub fn from_session(session: &HlsSession) -> Self { Self::new(&session.key, &session.proxy_session_id) }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(session: impl Into<String>, proxy_session: impl Into<String>) -> Self {
        Self { session_key: Arc::from(session.into()), proxy_session_id: Arc::from(proxy_session.into()) }
    }

    pub fn session(&self) -> String { short_hash(&self.session_key) }

    pub fn proxy_session(&self) -> String { short_hash(&self.proxy_session_id) }
}

impl fmt::Debug for HlsLogIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HlsLogIdentity")
            .field("session", &self.session())
            .field("proxy_session", &self.proxy_session())
            .finish()
    }
}

/// Applies the process-wide logging policy without adding HLS-specific
/// hashing, truncation, parsing, or forced redaction.
pub fn hls_origin_log_value(value: impl AsRef<str>) -> String { sanitize_sensitive_info(value.as_ref()).into_owned() }

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsRecoveryTriggerSource {
    ProvisioningHandoff,
    HardFetchFailure,
    OtherRedirectHost,
    TimelineRejection,
    PublicationLate,
    ReservePressure,
    Other,
}

impl HlsRecoveryTriggerSource {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::ProvisioningHandoff => "provisioning_handoff",
            Self::HardFetchFailure => "hard_fetch_failure",
            Self::OtherRedirectHost => "other_redirect_host",
            Self::TimelineRejection => "timeline_rejection",
            Self::PublicationLate => "publication_late",
            Self::ReservePressure => "reserve_pressure",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsRecoveryAvailabilityLogEvidence {
    pub progress_phase_before: HlsOriginProgressPhase,
    pub progress_condition_before: HlsOriginPathCondition,
    pub progress_phase_after: HlsOriginProgressPhase,
    pub progress_condition_after: HlsOriginPathCondition,
    pub controlling_lease_id: HlsAccessLeaseId,
    pub cursor: HlsLeasePlaybackCursor,
    pub guaranteed_reserve_ms: u64,
    pub recovery_required: bool,
    pub cutover_required: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsRecoveryTriggerDiagnostic {
    source: HlsRecoveryTriggerSource,
    availability: Option<HlsRecoveryAvailabilityLogEvidence>,
    pinned_host: Option<String>,
    candidate_host: Option<String>,
}

impl HlsRecoveryTriggerDiagnostic {
    pub const fn new(source: HlsRecoveryTriggerSource) -> Self {
        Self { source, availability: None, pinned_host: None, candidate_host: None }
    }

    pub fn availability(source: HlsRecoveryTriggerSource, evidence: HlsRecoveryAvailabilityLogEvidence) -> Self {
        Self { source, availability: Some(evidence), pinned_host: None, candidate_host: None }
    }

    pub fn other_redirect_host(pinned_host: Option<String>, candidate_host: Option<String>) -> Self {
        Self { source: HlsRecoveryTriggerSource::OtherRedirectHost, availability: None, pinned_host, candidate_host }
    }
}

pub fn hls_manifest_recovery_log_fields(
    identity: &HlsLogIdentity,
    trigger: HlsManifestAcceptanceTrigger,
    diagnostic: &HlsRecoveryTriggerDiagnostic,
) -> Option<String> {
    if !trigger.starts_episode() {
        return None;
    }
    let availability = diagnostic.availability.as_ref();
    Some(format!(
        "session={} proxy_session={} trigger={} trigger_source={} progress_phase_before={} progress_condition_before={} progress_phase_after={} progress_condition_after={} controlling_lease={} cursor_generation={} first_requested_proxy_seq={} highest_contiguous_completed_proxy_seq={} last_requested_proxy_seq={} guaranteed_reserve_ms={} recovery_required={} cutover_required={} pinned_host={} candidate_host={}",
        identity.session(),
        identity.proxy_session(),
        trigger.as_log_value(),
        diagnostic.source.as_log_value(),
        availability.map_or("none", |evidence| progress_phase_log_value(evidence.progress_phase_before)),
        availability.map_or("none", |evidence| progress_condition_log_value(evidence.progress_condition_before)),
        availability.map_or("none", |evidence| progress_phase_log_value(evidence.progress_phase_after)),
        availability.map_or("none", |evidence| progress_condition_log_value(evidence.progress_condition_after)),
        availability.map_or_else(
            || "none".to_string(),
            |evidence| safe_hls_access_lease_id(&evidence.controlling_lease_id),
        ),
        availability.map_or_else(|| "none".to_string(), |evidence| evidence.cursor.cursor_generation.to_string()),
        format_optional_u64(availability.and_then(|evidence| evidence.cursor.first_requested_proxy_seq)),
        format_optional_u64(
            availability.and_then(|evidence| evidence.cursor.highest_contiguous_completed_proxy_seq),
        ),
        format_optional_u64(availability.and_then(|evidence| evidence.cursor.last_requested_proxy_seq)),
        availability.map_or_else(|| "none".to_string(), |evidence| evidence.guaranteed_reserve_ms.to_string()),
        availability.map_or_else(|| "none".to_string(), |evidence| evidence.recovery_required.to_string()),
        availability.map_or_else(|| "none".to_string(), |evidence| evidence.cutover_required.to_string()),
        diagnostic
            .pinned_host
            .as_deref()
            .map_or_else(|| "none".to_string(), hls_origin_log_value),
        diagnostic
            .candidate_host
            .as_deref()
            .map_or_else(|| "none".to_string(), hls_origin_log_value),
    ))
}

const fn progress_phase_log_value(phase: HlsOriginProgressPhase) -> &'static str {
    match phase {
        HlsOriginProgressPhase::Cold => "cold",
        HlsOriginProgressPhase::Fresh => "fresh",
        HlsOriginProgressPhase::PublicationLate => "publication_late",
        HlsOriginProgressPhase::RecoveryRequired => "recovery_required",
        HlsOriginProgressPhase::Recovering => "recovering",
        HlsOriginProgressPhase::Critical => "critical",
        HlsOriginProgressPhase::TerminalPartial => "terminal_partial",
        HlsOriginProgressPhase::Terminal => "terminal",
    }
}

const fn progress_condition_log_value(condition: HlsOriginPathCondition) -> &'static str {
    match condition {
        HlsOriginPathCondition::ProgressExpected => "progress_expected",
        HlsOriginPathCondition::PublicationLate => "publication_late",
        HlsOriginPathCondition::RetryableFetchFailure => "retryable_fetch_failure",
        HlsOriginPathCondition::HardFetchFailure => "hard_fetch_failure",
        HlsOriginPathCondition::AcceptanceConflict => "acceptance_conflict",
        HlsOriginPathCondition::SegmentReadinessFailure => "segment_readiness_failure",
    }
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_string(), |value| value.to_string())
}

fn increment(counter: &AtomicU64, count: u64) { counter.fetch_add(count, Ordering::Relaxed); }

fn load(counter: &AtomicU64) -> u64 { counter.load(Ordering::Relaxed) }

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let value = digest.iter().take(4).fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
    format!("{value:08x}")
}

#[cfg(test)]
mod tests {
    use super::{
        hls_manifest_recovery_log_fields, hls_origin_log_value, safe_proxy_session_id, safe_session_key,
        HlsCacheMetrics, HlsLogIdentity, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
        HlsRecoveryAvailabilityLogEvidence, HlsRecoveryTriggerDiagnostic, HlsRecoveryTriggerSource,
    };
    use crate::{
        media_reserve::HlsLeasePlaybackCursor,
        origin_progress::{HlsOriginPathCondition, HlsOriginProgressPhase},
        HlsAccessLeaseId, HlsManifestAcceptanceTrigger, HlsSession, HlsSessionKey, ProxySessionId,
    };
    use reqwest::StatusCode;
    use shared::utils::{is_sanitize_sensitive_info_enabled, sanitize_sensitive_info, set_sanitize_sensitive_info};
    use std::sync::Mutex;
    use tuliprox_core::utils::content_coding::{hls_origin_content_coding_log_fields, ContentCodingObservation};

    static SANITIZATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct SanitizationSettingGuard(bool);

    impl SanitizationSettingGuard {
        fn set(enabled: bool) -> Self {
            let previous = is_sanitize_sensitive_info_enabled();
            set_sanitize_sensitive_info(enabled);
            Self(previous)
        }
    }

    impl Drop for SanitizationSettingGuard {
        fn drop(&mut self) { set_sanitize_sensitive_info(self.0); }
    }

    #[test]
    fn hls_origin_diagnostics_follow_global_sanitization_for_all_supported_value_shapes() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let values = [
            "http://user:password@example.com/live/user/password/123.ts?token=secret",
            "origin.example.com:8443",
            "http://192.0.2.10/live/user/password/123.ts",
            "http://[2001:db8::1]/live/user/password/123.ts",
            "provider://demo/live/user/password/123.m3u8",
            "not a valid URL user:password",
        ];

        let _guard = SanitizationSettingGuard::set(false);
        for value in values {
            assert_eq!(hls_origin_log_value(value), value);
        }

        set_sanitize_sensitive_info(true);
        for value in values {
            let logged = hls_origin_log_value(value);
            assert_eq!(logged, sanitize_sensitive_info(value));
            assert!(!logged.starts_with("origin#"));
        }
    }

    #[test]
    fn segment_fetch_diagnostic_identity_uses_content_session_key_and_separate_proxy_session() {
        let session = HlsSession::new(HlsSessionKey::new(7, "channel-a"), b"secret", 10);
        let identity = HlsLogIdentity::from_session(&session);

        let proxy_session_id = ProxySessionId("a8f31c9eQ7sLk92pV0mTaw".to_string());
        assert_eq!(safe_proxy_session_id(&proxy_session_id).len(), 8);
        assert!(!safe_proxy_session_id(&proxy_session_id).contains("Lk92pV0mTaw"));
        assert_eq!(identity.session(), safe_session_key(&session.key));
        assert_eq!(identity.proxy_session(), safe_proxy_session_id(&session.proxy_session_id));
        assert_ne!(identity.session(), identity.proxy_session());
    }

    #[test]
    fn recovery_diagnostic_requires_trigger_and_uses_frozen_cursor_evidence() {
        let identity = HlsLogIdentity::for_test("content-session", "proxy-session");
        let mut cursor = HlsLeasePlaybackCursor::default();
        cursor.first_requested_proxy_seq = Some(40);
        cursor.highest_contiguous_completed_proxy_seq = Some(42);
        cursor.last_requested_proxy_seq = Some(43);
        cursor.cursor_generation = 9;
        let diagnostic = HlsRecoveryTriggerDiagnostic::availability(
            HlsRecoveryTriggerSource::ReservePressure,
            HlsRecoveryAvailabilityLogEvidence {
                progress_phase_before: HlsOriginProgressPhase::PublicationLate,
                progress_condition_before: HlsOriginPathCondition::PublicationLate,
                progress_phase_after: HlsOriginProgressPhase::RecoveryRequired,
                progress_condition_after: HlsOriginPathCondition::PublicationLate,
                controlling_lease_id: HlsAccessLeaseId("lease-secret".to_string()),
                cursor,
                guaranteed_reserve_ms: 12_345,
                recovery_required: true,
                cutover_required: false,
            },
        );

        assert!(hls_manifest_recovery_log_fields(&identity, HlsManifestAcceptanceTrigger::None, &diagnostic,).is_none());
        let fields =
            hls_manifest_recovery_log_fields(&identity, HlsManifestAcceptanceTrigger::RecoveryRequired, &diagnostic)
                .expect("non-none trigger produces diagnostic");
        assert!(fields.contains(&format!("session={} proxy_session={}", identity.session(), identity.proxy_session())));
        assert!(fields.contains("trigger=recovery_required trigger_source=reserve_pressure"));
        assert!(fields.contains("progress_phase_before=publication_late"));
        assert!(fields.contains("cursor_generation=9 first_requested_proxy_seq=40"));
        assert!(fields.contains("highest_contiguous_completed_proxy_seq=42 last_requested_proxy_seq=43"));
        assert!(fields.contains("guaranteed_reserve_ms=12345 recovery_required=true cutover_required=false"));
    }

    #[test]
    fn other_redirect_host_diagnostic_uses_canonical_host_policy_without_hashes() {
        let _serial = SANITIZATION_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let identity = HlsLogIdentity::for_test("content-session", "proxy-session");
        let diagnostic = HlsRecoveryTriggerDiagnostic::other_redirect_host(
            Some("pinned.example.com".to_string()),
            Some("candidate.example.net".to_string()),
        );
        let _guard = SanitizationSettingGuard::set(false);
        let raw = hls_manifest_recovery_log_fields(&identity, HlsManifestAcceptanceTrigger::Observe, &diagnostic)
            .expect("observe trigger produces diagnostic");
        assert!(raw.contains("trigger_source=other_redirect_host"));
        assert!(raw.contains("pinned_host=pinned.example.com candidate_host=candidate.example.net"));
        assert!(!raw.contains("origin#"));

        set_sanitize_sensitive_info(true);
        let sanitized = hls_manifest_recovery_log_fields(&identity, HlsManifestAcceptanceTrigger::Observe, &diagnostic)
            .expect("observe trigger produces sanitized diagnostic");
        assert!(sanitized.contains(&format!(
            "pinned_host={} candidate_host={}",
            sanitize_sensitive_info("pinned.example.com"),
            sanitize_sensitive_info("candidate.example.net")
        )));
        assert!(!sanitized.contains("origin#"));
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

    #[test]
    fn content_coding_log_fields_are_fixed_redacted_and_complete() {
        let fields = hls_origin_content_coding_log_fields(
            ContentCodingObservation {
                content_encoding: "multiple",
                status: StatusCode::OK,
                content_length: Some(321),
            },
            HlsOriginContentCodingObjectKind::Manifest,
            false,
            HlsOriginContentCodingSource::Legacy,
        );

        assert_eq!(
            fields,
            "object_kind=manifest content_encoding=multiple status=200 requested_accept_encoding=identity decoded_to_identity=true content_length=321 range_requested=false source=legacy"
        );
        for sensitive_marker in ["http", "?", "cookie", "manifest-body", "signed-token"] {
            assert!(!fields.contains(sensitive_marker));
        }

        let unknown_length = hls_origin_content_coding_log_fields(
            ContentCodingObservation {
                content_encoding: "gzip",
                status: StatusCode::PARTIAL_CONTENT,
                content_length: None,
            },
            HlsOriginContentCodingObjectKind::Segment,
            true,
            HlsOriginContentCodingSource::Shared,
        );
        assert!(unknown_length.contains("content_length=unknown"));
        assert!(unknown_length.contains("range_requested=true source=shared"));

        let part = hls_origin_content_coding_log_fields(
            ContentCodingObservation { content_encoding: "br", status: StatusCode::OK, content_length: Some(17) },
            HlsOriginContentCodingObjectKind::Part,
            false,
            HlsOriginContentCodingSource::Shared,
        );
        assert!(part.starts_with("object_kind=part content_encoding=br status=200 "));
    }
}
