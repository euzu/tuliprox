use super::{
    fetch_hls_origin_manifest_request, manifest_body, no_delay_policy, spawn_test_origin, test_app_config,
    test_recovery_timing_policy, test_session, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest,
    LiveHlsOriginEntry,
};
use crate::origin_telemetry::{OriginFetchOutcome, OriginFetchPhaseError, OriginResourceKind, OriginTelemetryRecorder};
use axum::http::HeaderMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tuliprox_core::model::{HlsManifestRecoveryBurstConfig, HlsOriginTelemetryConfig};

#[tokio::test]
async fn manifest_retry_yields_one_record_per_request_attempt() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = Arc::clone(&hits);
    let server = spawn_test_origin(Arc::new(move |_path| {
        if hits_for_handler.fetch_add(1, Ordering::SeqCst) == 0 {
            (503, Vec::new(), "busy".to_string())
        } else {
            (200, Vec::new(), manifest_body())
        }
    }))
    .await;
    let (recorder, mut rx) = OriginTelemetryRecorder::with_channel(&HlsOriginTelemetryConfig {
        enabled: true,
        queue_capacity: 16,
        freshness_secs: 60,
        window_secs: 300,
    });
    let context = HlsOriginManifestFetchContext {
        origin_account_snapshot: None,
        attempt: None,
        app_config: test_app_config(),
        session: test_session(),
        availability_runtime: None,
        origin_entry: LiveHlsOriginEntry::parse(&format!("{}/live/user/pass/12345.m3u8", server.base_url))
            .expect("entry url"),
        headers: HeaderMap::new(),
        client: reqwest::Client::builder().no_proxy().build().expect("client"),
        no_redirect_client: reqwest::Client::builder().no_proxy().build().expect("client"),
        use_manual_redirects: false,
        origin_manifest_timeout_ms: 2_000,
        manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
        retry_policy: no_delay_policy(),
        recovery_timing_policy: test_recovery_timing_policy(2_000),
        acceptance_timing_seed: None,
        origin_telemetry: recorder,
    };

    let fetched = fetch_hls_origin_manifest_request(HlsOriginManifestFetchRequest::initial_global_policy(&context))
        .await
        .expect("second attempt succeeds");

    assert_eq!(fetched.body, manifest_body());
    let records: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.resource_kind == OriginResourceKind::Manifest));
    assert_eq!((records[0].attempt_no, records[1].attempt_no), (0, 1));
    assert_eq!(
        (records[0].outcome, records[0].http_status, records[0].error_phase),
        (OriginFetchOutcome::HttpError, Some(503), Some(OriginFetchPhaseError::Headers))
    );
    assert_eq!((records[1].outcome, records[1].http_status), (OriginFetchOutcome::Ok, Some(200)));
    assert_eq!(records[1].bytes_read, manifest_body().len() as u64);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
