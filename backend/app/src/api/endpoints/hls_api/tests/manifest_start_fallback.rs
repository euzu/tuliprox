use super::*;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::{
    api::{cold_start_retry_after_seconds, HlsManifestCommitRequirement},
    HlsManifestAttemptContext,
};

async fn attach_start_attempt(
    app: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    failed: bool,
) {
    let attempt =
        Arc::new(HlsManifestAttemptContext::new(HlsManifestCommitRequirement::CommittedManifestAllowed, false, None));
    if failed {
        attempt.finish(true);
    }
    let mut lease = app
        .hls
        .proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, current_time_millis())
        .await
        .expect("pending test lease");
    lease.manifest_start_attempt = Some(attempt);
    app.hls.proxy.prepare_access_lease(lease).await;
}

#[tokio::test]
async fn unpublished_lease_open_start_fallback_returns_service_unavailable_with_retry_after() {
    let app = test_app_state();
    disable_custom_stream_response(&app);
    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("open-start-fallback".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
        Some(cold_start_retry_after_seconds().to_string().as_str())
    );
    assert_eq!(
        response.headers().get(shared::model::HEADER_TULIPROX_FALLBACK).and_then(|v| v.to_str().ok()),
        Some("channel_unavailable")
    );
    assert_eq!(response.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
}

#[tokio::test]
async fn unpublished_lease_open_start_with_active_attempt_returns_service_unavailable_with_retry_after() {
    let app = test_app_state();
    disable_custom_stream_response(&app);
    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("active-attempt-fallback".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    attach_start_attempt(&app, &proxy_session_id, &lease_id, false).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()),
        Some(cold_start_retry_after_seconds().to_string().as_str())
    );
    assert_eq!(
        response.headers().get(shared::model::HEADER_TULIPROX_FALLBACK).and_then(|v| v.to_str().ok()),
        Some("channel_unavailable")
    );
    assert_eq!(response.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
}

#[tokio::test]
async fn unpublished_lease_failed_start_fallback_returns_configured_error_status_without_retry_after() {
    let app = test_app_state();
    disable_custom_stream_response(&app);
    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("failed-start-fallback".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    attach_start_attempt(&app, &proxy_session_id, &lease_id, true).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(
        response.headers().get(shared::model::HEADER_TULIPROX_FALLBACK).and_then(|v| v.to_str().ok()),
        Some("channel_unavailable")
    );
    assert_eq!(response.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
}

#[tokio::test]
async fn unpublished_lease_failed_start_fallback_custom_5xx_status_without_retry_after() {
    let app = test_app_state();
    let mut config = app.app_config.config.load().as_ref().clone();
    config.custom_stream_response_enabled = false;
    config.custom_stream_response_error_status = 504;
    app.app_config.config.store(Arc::new(config));

    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("failed-start-504".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    attach_start_attempt(&app, &proxy_session_id, &lease_id, true).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(
        response.headers().get(shared::model::HEADER_TULIPROX_FALLBACK).and_then(|v| v.to_str().ok()),
        Some("channel_unavailable")
    );
    assert_eq!(response.headers().get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
}

#[tokio::test]
async fn unpublished_lease_open_start_custom_video_returns_manifest_without_retry_after() {
    let app = test_app_state();
    enable_channel_unavailable_custom_response(&app);
    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("open-start-video".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest body");
    assert!(body.contains("#EXT-X-ENDLIST"));
}

#[tokio::test]
async fn unpublished_lease_open_start_without_server_info_has_no_retry_hint() {
    let app = test_app_state();
    enable_channel_unavailable_custom_response(&app);
    let mut api_proxy = app.app_config.api_proxy.load_full().expect("api proxy").as_ref().clone();
    for target in &mut api_proxy.user {
        for credentials in &mut target.credentials {
            let mut user = credentials.as_ref().clone();
            user.server = Some("missing-server".to_string());
            *credentials = Arc::new(user);
        }
    }
    app.app_config.api_proxy.store(Some(Arc::new(api_proxy)));
    let session =
        app.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", current_time_millis()).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("open-start-missing-server".into());
    prepare_pending_test_hls_access_lease(&app, &proxy_session_id, &lease_id).await;

    let response =
        super::super::hls_unpublished_lease_channel_unavailable_response(&app, &proxy_session_id, &lease_id).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
}
