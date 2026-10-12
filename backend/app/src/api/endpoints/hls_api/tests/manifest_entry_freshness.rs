use super::{
    create_active_hls_user_session, manifest_media_sequence, path_has_extension, prepare_pending_test_hls_access_lease,
    response_body, spawn_test_binary_origin, test_app_state_with_inputs, test_fingerprint, test_hls_access_context,
    TestBinaryOriginResponse, TestSegmentOrigin,
};
use crate::{
    api::model::{build_proxy_session_id, AppState, HlsAccessLeaseId, HlsAccessLeaseState, HlsSessionKey},
    model::{Config, ConfigInput, ReverseProxyConfig},
};
use axum::http::{HeaderMap, StatusCode};
use shared::model::{HlsCacheConfigDto, HlsStartupConfigDto, HlsStartupMode, InputType, ReverseProxyConfigDto};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

const ORIGIN_WINDOW_SEGMENTS: u64 = 6;
const SEGMENT_BODY: &[u8] = &[0x47; 8_192];

fn origin_playlist(head: u64) -> Vec<u8> {
    use std::fmt::Write;
    let mut body = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{head}\n");
    for sequence in head..head + ORIGIN_WINDOW_SEGMENTS {
        let _ = write!(body, "#EXTINF:2.0,\nseg-{sequence}.ts\n");
    }
    body.into_bytes()
}

async fn spawn_moving_origin(head: Arc<AtomicU64>) -> TestSegmentOrigin {
    spawn_test_binary_origin(Arc::new(move |path| {
        let body: Arc<[u8]> = if path_has_extension(path, "m3u8") {
            Arc::from(origin_playlist(head.load(Ordering::SeqCst)))
        } else {
            Arc::from(SEGMENT_BODY)
        };
        TestBinaryOriginResponse::new(StatusCode::OK, body)
    }))
    .await
}

async fn enable_hls_cache_with_startup_mode(app_state: &Arc<AppState>, mode: HlsStartupMode) {
    let config = Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(HlsCacheConfigDto {
                startup: HlsStartupConfigDto { mode, ..HlsStartupConfigDto::default() },
                ..HlsCacheConfigDto::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    };
    app_state.app_config.config.store(Arc::new(config));
    app_state.hls.proxy.update_config(&app_state.app_config).await;
}

/// Opens a new playback lease and returns the origin sequence behind the first
/// published media segment.
async fn first_origin_sequence_for_new_lease(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    request_url: &str,
    lease: &str,
) -> u64 {
    let origin_source = super::super::build_hls_origin_source(input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId(lease.to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(app_state, &proxy_session_id, &access_lease_id).await;
    let response = super::super::try_hls_cache_canonical_manifest_response(
        app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(request_url),
            input,
            origin_source,
        },
        HeaderMap::new(),
        None,
        "/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("hls cache should handle the live entrypoint");
    assert_eq!(response.status(), StatusCode::OK, "lease {lease}");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
    let first_proxy_seq = manifest_media_sequence(&body);
    let session = app_state.hls.proxy.sessions().get_by_key(&session_key).await.expect("shared hls session");
    let session = session.read().await;
    session.segments.get(&first_proxy_seq).expect("published segment is mapped").origin_key.host_local_sequence
}

/// Moves the retained session's last origin confirmation `idle_ms` into the
/// past. Media protection either stays active (short absence) or lapses with
/// the idle time (long absence, origin rebase).
async fn age_retained_session(app_state: &Arc<AppState>, session_key: &HlsSessionKey, idle_ms: u64, protected: bool) {
    let session = app_state.hls.proxy.sessions().get_by_key(session_key).await.expect("retained hls session");
    let mut session = session.write().await;
    let now_ms = super::super::current_time_millis();
    session.activity.last_authorized_media_at_ms =
        Some(if protected { now_ms } else { now_ms.saturating_sub(idle_ms) });
    session.origin_refresh.last_success_at_ms =
        session.origin_refresh.last_success_at_ms.map(|confirmed_at_ms| confirmed_at_ms.saturating_sub(idle_ms));
}

#[tokio::test]
async fn new_playback_on_retained_session_starts_on_current_origin_window() {
    for (mode, idle_ms, protected) in [
        (HlsStartupMode::Progressive, 16_000, true),
        (HlsStartupMode::FirstReady, 16_000, true),
        (HlsStartupMode::Conservative, 16_000, true),
        (HlsStartupMode::Optimistic, 16_000, true),
        (HlsStartupMode::Progressive, 300_000, false),
        (HlsStartupMode::FirstReady, 300_000, false),
        (HlsStartupMode::Conservative, 300_000, false),
        (HlsStartupMode::Optimistic, 300_000, false),
    ] {
        let head = Arc::new(AtomicU64::new(0));
        let origin = spawn_moving_origin(Arc::clone(&head)).await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache_with_startup_mode(&app_state, mode).await;
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let session_key = super::super::build_hls_origin_source(&input, "12345").session_key();
        let case = format!("{mode:?} idle_ms={idle_ms} protected={protected}");

        let first = first_origin_sequence_for_new_lease(&app_state, &input, &request_url, "lease-first").await;
        assert!(first < 20, "{case}: first playback starts on the initial window, got origin sequence {first}");

        head.store(60, Ordering::SeqCst);
        age_retained_session(&app_state, &session_key, idle_ms, protected).await;

        let reentry = first_origin_sequence_for_new_lease(&app_state, &input, &request_url, "lease-reentry").await;
        assert!(reentry >= 60, "{case}: re-entry started on stale origin sequence {reentry}");
    }
}
