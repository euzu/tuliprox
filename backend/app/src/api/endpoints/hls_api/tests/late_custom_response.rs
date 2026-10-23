use super::*;

async fn exhausted_fixture() -> (terminal_tail::PublicationLateFixture, String) {
    let fixture = terminal_tail::publication_late_fixture().await;
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let previous = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    let manager = &fixture.app_state.hls.proxy;
    let now_ms = super::super::current_time_millis();
    let lease = manager
        .access_lease_response_snapshot(&fixture.access_lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .unwrap();
    let identity = lease.media_identity().unwrap();
    let first = lease.last_manifest_snapshot.as_ref().unwrap().first_proxy_seq;
    // Virtual established playback: the media clock begins with delivered bytes,
    // and has consumed the complete finite READY horizon before this outage.
    let started_at_ms = now_ms - 120_000;
    let token = manager
        .record_access_lease_segment_request_started_if_identity_matches(
            &fixture.access_lease_id,
            &fixture.proxy_session_id,
            identity,
            first,
            started_at_ms,
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .mark_delivered_media_for_lease(
                &fixture.session,
                &fixture.access_lease_id,
                &fixture.proxy_session_id,
                identity,
                Some(token),
                started_at_ms
            )
            .await,
        tuliprox_hls::api::HlsMediaActivityCommitOutcome::Committed
    );
    let mut session = fixture.session.write().await;
    session.origin_control.path_condition = crate::api::model::HlsOriginPathCondition::AcceptanceConflict;
    session.origin_refresh.mark_started(now_ms);
    drop(session);
    (fixture, previous)
}

#[tokio::test]
async fn expired_splice_deadline_serves_exact_last_live_uri_and_immutable_terminal_tail() {
    let (fixture, previous) = exhausted_fixture().await;
    let last_live = previous.lines().rfind(|line| !line.is_empty() && !line.starts_with('#')).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let body = loop {
        let response = get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None).await;
        if response.status() == StatusCode::OK {
            let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
            if body.contains("#EXT-X-ENDLIST") {
                break body;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "terminal owner must finish");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert!(!body.contains("/cvs/hls/"));
    let media = body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect::<Vec<_>>();
    let first_csv = media.iter().position(|uri| uri.contains("/terminal/")).unwrap();
    assert!(first_csv > 0);
    assert_eq!(media[first_csv - 1], last_live);
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    let live = get_response(Arc::clone(&fixture.app_state), last_live, None).await;
    assert_eq!(live.status(), StatusCode::OK);
    assert!(!response_body(live).await.is_empty());
    let csv = get_response(Arc::clone(&fixture.app_state), media[first_csv], None).await;
    assert_eq!(csv.status(), StatusCode::OK);
    assert!(!response_body(csv).await.is_empty());
    fixture.app_state.app_config.custom_stream_response.store(None);
    let (a, b) = tokio::join!(
        get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None),
        get_response(Arc::clone(&fixture.app_state), &fixture.media_playlist_uri, None)
    );
    assert_eq!(response_body(a).await, body.as_bytes());
    assert_eq!(response_body(b).await, body.as_bytes());
    assert!(fixture.session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn missing_live_base_fails_explicitly_instead_of_publishing_csv_only() {
    let (fixture, _) = exhausted_fixture().await;
    fixture.session.write().await.segments.clear();
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
        .unwrap();
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}

#[tokio::test]
async fn missing_csv_asset_keeps_the_explicit_error_response() {
    let (fixture, _) = exhausted_fixture().await;
    fixture.app_state.app_config.custom_stream_response.store(None);
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
        .unwrap();
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}
