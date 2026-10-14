use super::*;

#[tokio::test]
async fn late_custom_response_serves_canonical_manifest_and_immutable_media_without_origin_requests() {
    let fixture = terminal_tail::publication_late_fixture().await;
    let now_ms = super::super::current_time_millis();
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&fixture.access_lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .expect("published live lease");
    let snapshot = lease.last_manifest_snapshot.expect("published snapshot");
    {
        let mut session = fixture.session.write().await;
        session.segments.clear();
        session.origin_control.path_condition = crate::api::model::HlsOriginPathCondition::AcceptanceConflict;
    }
    let requests_before = fixture.origin.manifest_request_count();
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("late manifest");
    assert!(body.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", snapshot.last_proxy_seq + 1)));
    assert!(body.contains(&format!("#EXT-X-TARGETDURATION:{}\n", snapshot.target_duration_ms / 1_000)));
    assert!(body.contains("#EXT-X-KEY:METHOD=NONE\n#EXT-X-DISCONTINUITY\n"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    assert!(!body.contains("/hls/shared/live/"));
    let media_uris = body.lines().filter(|line| line.contains("/cvs/hls/")).collect::<Vec<_>>();
    assert_eq!(media_uris.len(), 12);
    let (first, second) = tokio::join!(
        get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None),
        get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None),
    );
    for response in [first, second] {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, body.as_bytes());
    }
    let media = custom_media_response(Arc::clone(&fixture.app_state), media_uris[0], None).await;
    assert_eq!(media.status(), StatusCode::OK);
    assert_eq!(media.headers()[header::CONTENT_TYPE], "video/mp2t");
    let bytes = response_body(media).await;
    assert!(!bytes.is_empty());
    let range = custom_media_response(Arc::clone(&fixture.app_state), media_uris[0], Some("bytes=0-187")).await;
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response_body(range).await, bytes.slice(..188));
    let router =
        crate::api::endpoints::custom_video_stream_api::cvs_api_register().with_state(Arc::clone(&fixture.app_state));
    let head = router
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(media_uris[0])
                .extension(ConnectInfo(test_addr()))
                .body(Body::empty())
                .expect("HEAD request"),
        )
        .await
        .expect("HEAD response");
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CONTENT_LENGTH], bytes.len().to_string());
    assert!(response_body(head).await.is_empty());
    fixture.app_state.app_config.custom_stream_response.store(None);
    let replay = custom_media_response(Arc::clone(&fixture.app_state), media_uris[0], None).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(response_body(replay).await, bytes);
    let wrong_lease = media_uris[0].replace(&fixture.access_lease_id.0, "unknown-lease");
    assert_eq!(
        custom_media_response(Arc::clone(&fixture.app_state), &wrong_lease, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let wrong_index = media_uris[0].replace("/0.ts", "/12.ts");
    assert_eq!(
        custom_media_response(Arc::clone(&fixture.app_state), &wrong_index, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let late = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease");
    assert!(matches!(late.playback_mode, HlsLeasePlaybackMode::TerminalCustomResponse(_)));
    assert_eq!(late.playback_cursor, lease.playback_cursor);
    assert!(!fixture.session.read().await.has_terminal_tail_protections());
    assert_eq!(fixture.origin.manifest_request_count(), requests_before);
}

#[tokio::test]
async fn late_custom_response_without_configured_asset_retains_error_response() {
    let fixture = terminal_tail::publication_late_fixture().await;
    fixture.app_state.app_config.custom_stream_response.store(None);
    {
        let mut session = fixture.session.write().await;
        session.segments.clear();
        session.origin_control.path_condition = crate::api::model::HlsOriginPathCondition::AcceptanceConflict;
    }
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response_body(response).await.is_empty());
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal unavailable lease");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}

#[tokio::test]
async fn late_custom_media_get_head_and_range_renew_lease_without_manifest_polls() {
    let (fixture, media_uri) = committed_late_http_fixture().await;
    for (method, range, status) in [
        (Method::GET, None, StatusCode::OK),
        (Method::HEAD, None, StatusCode::OK),
        (Method::GET, Some("bytes=0-187"), StatusCode::PARTIAL_CONTENT),
    ] {
        let now_ms = super::super::current_time_millis();
        let previous_access_ms = now_ms.saturating_sub(50_000);
        assert!(fixture
            .app_state
            .hls
            .proxy
            .activate_access_lease(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                previous_access_ms,
                HlsAccessLeaseTiming { active_window_ms: 24_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        let before = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(&fixture.access_lease_id, &fixture.proxy_session_id, now_ms)
            .await
            .expect("valid late lease");
        let response =
            custom_media_response_with_method(Arc::clone(&fixture.app_state), &media_uri, range, method).await;
        assert_eq!(response.status(), status);
        let after = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                super::super::current_time_millis(),
            )
            .await
            .expect("renewed late lease");
        assert!(after.last_seen_at_ms >= now_ms);
        assert!(after.valid_until_ms > before.valid_until_ms);
        assert!(after.active_until_ms > before.active_until_ms);
        assert_eq!(after.playback_mode, before.playback_mode);
        assert_eq!(after.playback_cursor, before.playback_cursor);
        assert_eq!(after.admission_generation, before.admission_generation);
    }
}

#[tokio::test]
async fn late_custom_media_errors_and_denied_authorization_do_not_renew_lease() {
    let (fixture, media_uri) = committed_late_http_fixture().await;
    let now_ms = super::super::current_time_millis();
    assert!(fixture
        .app_state
        .hls
        .proxy
        .activate_access_lease(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            now_ms - 50_000,
            HlsAccessLeaseTiming { active_window_ms: 24_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    let before = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&fixture.access_lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .expect("valid late lease");
    let fingerprint = media_uri.rsplit('/').nth(1).expect("asset fingerprint");
    for (uri, range, expected) in [
        (media_uri.replace("/0.ts", "/12.ts"), None, StatusCode::NOT_FOUND),
        (media_uri.replace(fingerprint, "wrong-fingerprint"), None, StatusCode::NOT_FOUND),
        (media_uri.replace(&fixture.access_lease_id.0, "unknown-lease"), None, StatusCode::NOT_FOUND),
        (media_uri.clone(), Some("bytes=1000000000000-"), StatusCode::RANGE_NOT_SATISFIABLE),
    ] {
        let response = custom_media_response(Arc::clone(&fixture.app_state), &uri, range).await;
        assert_eq!(response.status(), expected);
    }
    fixture.app_state.app_config.api_proxy.store(None);
    let response = custom_media_response(Arc::clone(&fixture.app_state), &media_uri, None).await;
    assert_eq!(response.status(), fixture.app_state.app_config.get_auth_error_status());
    let after = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("unchanged late lease");
    assert_eq!(after.last_seen_at_ms, before.last_seen_at_ms);
    assert_eq!(after.valid_until_ms, before.valid_until_ms);
    assert_eq!(after.active_until_ms, before.active_until_ms);
}

async fn committed_late_http_fixture() -> (terminal_tail::PublicationLateFixture, String) {
    let fixture = terminal_tail::publication_late_fixture().await;
    {
        let mut session = fixture.session.write().await;
        session.segments.clear();
        session.origin_control.path_condition = crate::api::model::HlsOriginPathCondition::AcceptanceConflict;
    }
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("late manifest");
    let media_uri = body.lines().find(|line| line.contains("/cvs/hls/")).expect("custom segment URI").to_owned();
    (fixture, media_uri)
}

async fn custom_media_response(app_state: Arc<AppState>, uri: &str, range: Option<&str>) -> Response<Body> {
    custom_media_response_with_method(app_state, uri, range, Method::GET).await
}

async fn custom_media_response_with_method(
    app_state: Arc<AppState>,
    uri: &str,
    range: Option<&str>,
    method: Method,
) -> Response<Body> {
    let router = crate::api::endpoints::custom_video_stream_api::cvs_api_register().with_state(app_state);
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    router
        .oneshot(request.extension(ConnectInfo(test_addr())).body(Body::empty()).expect("custom media request"))
        .await
        .expect("custom media response")
}
