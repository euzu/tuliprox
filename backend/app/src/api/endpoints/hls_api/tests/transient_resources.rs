use super::{
    assert_no_hls_cache_stream_registered, disable_custom_stream_response, enable_hls_cache, encode_test_manifest,
    get_response, get_status, hls_proxy_uri, hls_session_last_media_at_ms, map_transient_resource, media_uri_count,
    overlap_provider_input, publish_owner_handoff_test_manifest, response_body, spawn_test_transient_origin,
    spawn_test_transient_origin_with_delayed_binary_response, spawn_test_transient_origin_with_delayed_response,
    spawn_test_transient_origin_with_response, test_app_state, test_app_state_with_hls_proxy,
    test_app_state_with_inputs, transient_manifest_body, try_test_hls_cached_manifest_response,
    wait_for_provider_connection_count, CanonicalOwnerHandoffFixture, HlsCanonicalOwnerRegistrationKind,
};
use crate::{
    api::model::{
        HlsAccessLeaseId, HlsAccessLeaseState, HlsAvailabilityReevaluationFinishReason,
        HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationRegistration, HlsOriginAccountBinding,
        HlsProxyManager, HlsSessionKey, HlsSessionMode, ProxySessionId, TransientObjectCacheKey,
        TransientObjectCacheStatus, TransientResourceId,
    },
    model::StripConfig,
};
use axum::http::{header, HeaderValue, StatusCode};
use http_body_util::BodyExt;
use shared::model::HlsStripMode;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn scheduled_owner_does_not_return_transient_503() {
    let fixture = CanonicalOwnerHandoffFixture::new(&["scheduled-lease"]).await;
    let owner_key = fixture
        .app_state
        .hls
        .proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let coordinator = fixture.app_state.hls.proxy.availability_reevaluations();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());
    let task_started = Arc::clone(&started);
    let task_release = Arc::clone(&release);
    let task_completed = Arc::clone(&completed);
    let task_session = Arc::clone(&fixture.session);
    let task_app_state = Arc::clone(&fixture.app_state);
    let task_proxy_session_id = fixture.proxy_session_id.clone();
    let task_owner_key = owner_key.clone();
    assert_eq!(
        coordinator.register(
            owner_key,
            HlsAvailabilityReevaluationMode::RecoveryPressure,
            move |ownership| async move {
                task_started.notify_one();
                task_release.notified().await;
                publish_owner_handoff_test_manifest(&task_session).await;
                task_app_state.hls.proxy.notify_session_evidence_changed(&task_proxy_session_id);
                let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                task_completed.notify_one();
            },
        ),
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    started.notified().await;
    let deadline_ms = super::super::current_time_millis().saturating_add(60_000);
    let safe_session = fixture.safe_session().await;
    let mut response = Box::pin(super::super::join_hls_canonical_manifest_owner(
        fixture.handoff_context(0, safe_session, deadline_ms),
        HlsCanonicalOwnerRegistrationKind::Scheduled,
    ));

    assert!(matches!(futures::poll!(response.as_mut()), std::task::Poll::Pending));
    release.notify_one();

    let response = response.await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("live manifest utf8");
    assert!(body.contains("/scheduled-lease/"));
    completed.notified().await;
}

#[tokio::test]
async fn hls_cache_pending_transient_manifest_applies_initial_strip_without_mutating_shared_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        session.transient.replace_manifest_with_semantics(transient_manifest_body(&proxy_session_id), 100, None);
        session.mark_authorized_media_access(super::super::current_time_millis());
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    let stored_before = session.read().await.transient.last_manifest_body.clone().expect("transient manifest body");

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    assert_eq!(session.read().await.transient.last_manifest_body.as_ref().expect("stored manifest"), &stored_before);
    assert_eq!(media_uri_count(&stored_before), 6);
}

#[tokio::test]
async fn hls_cache_activated_transient_manifest_skips_initial_strip() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        session.transient.replace_manifest_with_semantics(transient_manifest_body(&proxy_session_id), 100, None);
        session.mark_authorized_media_access(super::super::current_time_millis().saturating_sub(16_000));
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 6);
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
}

#[tokio::test]
async fn hls_cache_transient_manifest_without_media_activity_is_not_served_from_committed_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_id),
            rendered_at_ms,
            Some(60_000),
        );
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await;

    assert!(response.is_none());
}

#[tokio::test]
async fn hls_cache_no_media_yet_transient_manifest_is_served_for_initial_canonical_response() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_id),
            rendered_at_ms,
            Some(60_000),
        );
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &strip,
        None,
        super::HlsCachedManifestOptions::initial(Duration::ZERO),
    )
    .await
    .expect("initial transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
}

#[tokio::test]
async fn hls_cache_transient_manifest_outside_soft_window_is_not_served_from_committed_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        session.transient.replace_manifest_with_semantics(transient_manifest_body(&proxy_session_id), 100, None);
        session.mark_authorized_media_access(super::super::current_time_millis().saturating_sub(60_000));
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await;

    assert!(response.is_none());
}

#[tokio::test]
async fn hls_cache_no_media_yet_waits_for_first_transient_manifest_commit() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        session.origin_refresh.in_flight = true;
        session.proxy_session_id.clone()
    };
    let session_for_commit = Arc::clone(&session);
    let proxy_session_for_body = proxy_session_id.0.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut session = session_for_commit.write().await;
        let rendered_at_ms = super::super::current_time_millis();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_for_body),
            rendered_at_ms,
            Some(60_000),
        );
        session.origin_refresh.in_flight = false;
    });
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &strip,
        None,
        super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
    )
    .await
    .expect("initial transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
}

#[tokio::test]
async fn hls_cache_expired_transient_manifest_waits_for_revalidation_commit() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        session.mark_authorized_media_access(super::super::current_time_millis().saturating_sub(60_000));
        session.origin_refresh.in_flight = true;
        session.proxy_session_id.clone()
    };
    let session_for_commit = Arc::clone(&session);
    let proxy_session_for_body = proxy_session_id.0.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut session = session_for_commit.write().await;
        let rendered_at_ms = super::super::current_time_millis();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_for_body),
            rendered_at_ms,
            Some(60_000),
        );
        session.origin_refresh.in_flight = false;
    });
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &strip,
        None,
        super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
    )
    .await
    .expect("revalidated transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
}

#[test]
fn transient_full_object_cacheable_request_accepts_open_zero_range() {
    use crate::api::model::is_hls_transient_full_object_cacheable_request;

    assert!(is_hls_transient_full_object_cacheable_request(None));
    assert!(is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-"))));
    assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=4-"))));
    assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=-4"))));
    assert!(!is_hls_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-1,4-5"))));
}

#[tokio::test]
async fn transient_resource_without_lease_returns_not_found() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", false).await;

    let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/r/{resource_id}.ts")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn transient_resource_without_range_is_cached_after_first_fetch() {
    let app_state = test_app_state();
    let origin =
        spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/mp2t")], "0123456789").await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let first = get_response(Arc::clone(&app_state), &uri, None).await;
    let second = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
    assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
    assert_eq!(origin.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn transient_resource_range_from_zero_is_cached_as_full_object() {
    let app_state = test_app_state();
    let origin =
        spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/mp2t")], "0123456789").await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
    let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=4-")).await;

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
    assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"456789"));
    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_ascii_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(!request.contains("\r\nrange:"));
}

#[tokio::test]
async fn transient_cache_fill_rejects_identity_partial_without_ready_object_or_temp_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(Arc::clone(&hls_proxy));
    disable_custom_stream_response(&app_state);
    let origin = spawn_test_transient_origin_with_response(
        "206 Partial Content",
        &[("Content-Type", "video/mp2t"), ("Content-Range", "bytes 0-3/10")],
        "part",
    )
    .await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let proxy_session_id = ProxySessionId(proxy_session_id);
    let cache_key = TransientObjectCacheKey::new(proxy_session_id.clone(), TransientResourceId(resource_id), "ts");
    let session = hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.expect("session should exist");
    let status =
        session.read().await.transient.object_cache.get(&cache_key).expect("transient cache entry").status.clone();
    assert!(matches!(status, TransientObjectCacheStatus::FailedPermanent { status: None, .. }));
    assert!(hls_proxy.segment_cache().metadata(&cache_key).await.expect("cache metadata reads").is_none());
    assert!(!hls_proxy.segment_cache().has_active_temp_files());
    assert_eq!(std::fs::read_dir(temp_dir.path()).expect("cache root reads").count(), 0);

    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_ascii_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(!request.contains("\r\nrange:"));
}

#[tokio::test]
async fn transient_resource_range_from_zero_waits_for_inflight_object_cache_fetch() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin_with_delayed_response(
        "200 OK",
        &[("Content-Type", "video/mp2t")],
        "0123456789",
        Duration::from_millis(150),
    )
    .await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let first_app_state = Arc::clone(&app_state);
    let first_uri = uri.clone();
    let first = tokio::spawn(async move { get_response(first_app_state, &first_uri, Some("bytes=0-")).await });
    for _ in 0..50 {
        if origin.requests.lock().await.len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(origin.requests.lock().await.len(), 1);

    let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
    let first = first.await.expect("first request joins");

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
    assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
    assert_eq!(origin.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn transient_resource_range_from_offset_without_ready_object_is_not_cached() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;
    let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(origin.requests.lock().await.len(), 2);
}

#[tokio::test]
async fn transient_resource_origin_error_does_not_mark_media_activity() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin_with_response(
        "500 Internal Server Error",
        &[("Content-Type", "text/plain")],
        "origin-error",
    )
    .await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()), Some("1"));
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn transient_resource_permanent_origin_error_never_redirects_to_manifest() {
    let app_state = test_app_state();
    let origin =
        spawn_test_transient_origin_with_response("404 Not Found", &[("Content-Type", "text/plain")], "missing").await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn transient_resource_permanent_origin_error_returns_not_found_when_custom_response_disabled() {
    let app_state = test_app_state();
    disable_custom_stream_response(&app_state);
    let origin =
        spawn_test_transient_origin_with_response("404 Not Found", &[("Content-Type", "text/plain")], "missing").await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn transient_decoder_failure_releases_origin_and_access_guards_once() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let mut truncated = encode_test_manifest("gzip", b"transient decoder failure").await;
    truncated.truncate(truncated.len().saturating_sub(8));
    let origin = spawn_test_transient_origin_with_delayed_binary_response(
        "200 OK",
        &[("Content-Type", "video/mp2t"), ("Content-Encoding", "gzip")],
        truncated,
        Duration::ZERO,
    )
    .await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let proxy_session_id_value = ProxySessionId(proxy_session_id.clone());
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id_value)
        .await
        .expect("session should exist");
    {
        let mut session = session.write().await;
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("account-a"),
            &proxy_session_id_value,
            super::super::current_time_millis(),
        ));
    }
    let resource = session
        .read()
        .await
        .transient
        .resources
        .get(&TransientResourceId(resource_id.clone()))
        .cloned()
        .expect("transient resource exists");
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-")).await;

    assert_eq!(response.status(), StatusCode::OK);
    wait_for_provider_connection_count(&app_state, 1).await;
    assert_eq!(resource.active_readers(), 1);
    assert!(response.into_body().collect().await.is_err());
    wait_for_provider_connection_count(&app_state, 0).await;
    assert_eq!(resource.active_readers(), 0);
    tokio::task::yield_now().await;
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert_eq!(origin.requests.lock().await.len(), 1, "body failure must not start another origin request");
}

#[tokio::test]
async fn transient_unknown_resource_never_redirects_to_manifest() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, _) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "r/unknown.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn transient_unknown_resource_returns_not_found_when_custom_response_disabled() {
    let app_state = test_app_state();
    disable_custom_stream_response(&app_state);
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, _) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "r/unknown.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}
