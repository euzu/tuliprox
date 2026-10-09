use super::{
    activate_test_hls_access_lease, create_active_hls_user_session, create_unbound_hls_test_session, enable_hls_cache,
    hls_custom_video_test_user, prepare_pending_test_hls_access_lease, publish_owner_handoff_test_manifest,
    response_body, single_hls_provider_input, spawn_test_segment_origin, store_test_sources_with_target,
    test_app_state_with_hls_proxy_and_inputs, test_app_state_with_inputs, test_custom_video_buffer, test_fingerprint,
    test_hls_access_context, CanonicalOwnerHandoffFixture, HlsCanonicalOwnerRegistrationKind,
};
use crate::{
    api::model::{
        build_hls_custom_video_manifest_body, build_proxy_session_id, AppState, CustomVideoStreamType,
        HlsAccessLeaseId, HlsAccessLeaseState, HlsAvailabilityReevaluationFinishReason,
        HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationRegistration, HlsProxyManager, SegmentCacheStatus,
        TransportStreamBuffer,
    },
    model::{ConfigInput, ConfigTarget, CustomStreamResponse},
    processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
};
use axum::http::{header, HeaderMap, StatusCode};
use shared::model::{ConfigTargetDto, InputType};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[tokio::test]
async fn already_owned_work_is_joined_without_duplicate_refresh() {
    let fixture = CanonicalOwnerHandoffFixture::new(&["first-lease", "second-lease"]).await;
    let owner_key = fixture
        .app_state
        .hls
        .proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let coordinator = fixture.app_state.hls.proxy.availability_reevaluations();
    let release = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task_completed = Arc::clone(&completed);
    let task_session = Arc::clone(&fixture.session);
    let task_app_state = Arc::clone(&fixture.app_state);
    let task_proxy_session_id = fixture.proxy_session_id.clone();
    let task_owner_key = owner_key.clone();
    assert_eq!(
        coordinator.register(
            owner_key.clone(),
            HlsAvailabilityReevaluationMode::RecoveryPressure,
            move |ownership| async move {
                task_release.notified().await;
                publish_owner_handoff_test_manifest(&task_session).await;
                task_app_state.hls.proxy.notify_session_evidence_changed(&task_proxy_session_id);
                let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
                task_completed.notify_one();
            },
        ),
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    let duplicate_owner_runs = Arc::new(AtomicUsize::new(0));
    let task_duplicate_owner_runs = Arc::clone(&duplicate_owner_runs);
    assert_eq!(
        coordinator.register(owner_key, HlsAvailabilityReevaluationMode::RecoveryPressure, move |_| async move {
            task_duplicate_owner_runs.fetch_add(1, Ordering::SeqCst);
        },),
        HlsAvailabilityReevaluationRegistration::AlreadyOwned
    );
    let deadline_ms = super::super::current_time_millis().saturating_add(60_000);
    let mut first = Box::pin(super::super::join_hls_canonical_manifest_owner(
        fixture.handoff_context(0, fixture.safe_session().await, deadline_ms),
        HlsCanonicalOwnerRegistrationKind::AlreadyOwned,
    ));
    let mut second = Box::pin(super::super::join_hls_canonical_manifest_owner(
        fixture.handoff_context(1, fixture.safe_session().await, deadline_ms),
        HlsCanonicalOwnerRegistrationKind::AlreadyOwned,
    ));
    assert!(matches!(futures::poll!(first.as_mut()), std::task::Poll::Pending));
    assert!(matches!(futures::poll!(second.as_mut()), std::task::Poll::Pending));

    release.notify_one();
    let (first, second) = tokio::join!(first, second);

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first_body = String::from_utf8(response_body(first).await.to_vec()).expect("first manifest utf8");
    let second_body = String::from_utf8(response_body(second).await.to_vec()).expect("second manifest utf8");
    assert!(first_body.contains("/first-lease/"));
    assert!(!first_body.contains("/second-lease/"));
    assert!(second_body.contains("/second-lease/"));
    assert!(!second_body.contains("/first-lease/"));
    assert_eq!(duplicate_owner_runs.load(Ordering::SeqCst), 0);
    completed.notified().await;
    tokio::task::yield_now().await;
    assert_eq!(coordinator.owner_count(), 0);
}

#[tokio::test]
async fn canonical_owner_join_preserves_bounded_deadline_failure() {
    let fixture = CanonicalOwnerHandoffFixture::new(&["deadline-lease"]).await;
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.leases[0].0,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("pending deadline lease");
    assert_eq!(
        super::super::hls_canonical_owner_request_deadline_ms(
            &lease,
            Duration::ZERO,
            super::super::current_time_millis(),
        ),
        lease.pending_deadline_ms().expect("pending lease deadline")
    );
    let owner_key = fixture
        .app_state
        .hls
        .proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("owner key");
    let coordinator = fixture.app_state.hls.proxy.availability_reevaluations();
    assert_eq!(
        coordinator.register(owner_key, HlsAvailabilityReevaluationMode::RecoveryPressure, |ownership| async move {
            ownership.cancelled().await;
        },),
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    let response = super::super::join_hls_canonical_manifest_owner(
        fixture.handoff_context(0, fixture.safe_session().await, super::super::current_time_millis().saturating_sub(1)),
        HlsCanonicalOwnerRegistrationKind::Scheduled,
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    coordinator.cancel_session(&fixture.proxy_session_id);
}

#[test]
fn hls_custom_video_manifest_body_is_none_for_non_provisioning() {
    let user = hls_custom_video_test_user();
    let manifest = build_hls_custom_video_manifest_body(
        "https://example.test/iptv/",
        &user,
        CustomVideoStreamType::UserConnectionsExhausted,
    );

    assert!(manifest.is_none(), "non-provisioning custom video types have no static manifest body");
}

#[test]
fn hls_custom_video_manifest_uses_live_six_segment_window_for_provisioning() {
    let user = hls_custom_video_test_user();
    let manifest =
        build_hls_custom_video_manifest_body("https://example.test/iptv", &user, CustomVideoStreamType::Provisioning)
            .expect("provisioning manifest does not depend on one looping asset duration");

    assert!(manifest.contains("#EXT-X-TARGETDURATION:2"));
    assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n"));
    assert!(!manifest.contains("#EXT-X-DISCONTINUITY-SEQUENCE"));
    assert!(!manifest.contains("#EXT-X-SESSION-DATA"));
    assert!(!manifest.contains("#EXT-X-ENDLIST"));
    assert!(!manifest.contains("#EXT-X-DISCONTINUITY\n"));
    assert_eq!(manifest.matches("#EXTINF:2.000000,").count(), 6);
    for index in 0..6 {
        assert!(
            manifest.contains(&format!("https://example.test/iptv/cvs/hls/viewer/secret/provisioning_{index:03}.ts"))
        );
    }
    assert!(!manifest.contains("https://example.test/iptv/cvs/hls/viewer/secret/provisioning.ts"));
    assert_eq!(
        crate::api::model::hls_panel_provisioning_manifest_path(&user, 80510),
        "/cvs/hls/viewer/secret/provisioning.m3u8?id=80510"
    );
    assert!(!manifest.contains("provisioning.ts?"));
    assert!(!manifest.contains("virtual_id"));
    assert!(!manifest.contains("&seq="));
}

pub(in crate::api::endpoints::hls_api::tests) fn enable_provider_exhausted_custom_response(app_state: &Arc<AppState>) {
    app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: None,
        user_connections_exhausted: None,
        provider_connections_exhausted: Some(test_custom_video_buffer()),
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
}

pub(in crate::api::endpoints::hls_api::tests) fn enable_hls_provisioning_custom_response(app_state: &Arc<AppState>) {
    let mut ts_packet = vec![0_u8; 188];
    ts_packet[0] = 0x47;
    let provisioning_segments = (0..6)
        .map(|index| {
            let mut packet = ts_packet.clone();
            packet[1] = u8::try_from(index).expect("test index fits");
            TransportStreamBuffer::new(packet)
        })
        .collect();
    app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: None,
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: provisioning_segments,
    })));
}

#[tokio::test]
async fn hls_provider_exhausted_without_provisioning_returns_custom_manifest() {
    let input = single_hls_provider_input("provider-exhausted-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    store_test_sources_with_target(
        &app_state,
        input.clone(),
        ConfigTarget::from(&ConfigTargetDto { id: 1, name: "default".to_string(), ..Default::default() }),
    );
    enable_provider_exhausted_custom_response(&app_state);
    let session = create_unbound_hls_test_session(&app_state, &input, "provider-exhausted-session", 1_000).await;
    let access_lease_id = HlsAccessLeaseId("provider-exhausted-lease".to_string());
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
    let strip = app_state.hls.proxy.strip();

    let response = super::super::hls_shared_provisioning_or_provider_exhausted_response(
        &app_state,
        &session,
        "hls-user",
        &input,
        59,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    let manifest = String::from_utf8(body.to_vec()).expect("manifest is utf8");
    assert!(manifest.contains("#EXTM3U"));
    assert!(manifest.contains(&format!("/cvs/hls/{}/", access_lease_id.0)));
    assert!(!manifest.contains("/hls-user/"));
    assert!(!manifest.contains("/hls-pass/"));
    assert!(!manifest.contains("/provider_connections_exhausted/"));
}

#[tokio::test]
async fn shared_provisioning_timeline_manifest_uses_canonical_hls_session_segments() {
    let input = single_hls_provider_input("shared-provisioning-timeline-input");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy_and_inputs(hls_proxy, vec![Arc::new(input.clone())]);
    enable_hls_provisioning_custom_response(&app_state);
    let session = create_unbound_hls_test_session(&app_state, &input, "shared-prov-timeline-12345", 1_000).await;
    let access_lease_id = HlsAccessLeaseId("timeline-lease".to_string());
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    activate_test_hls_access_lease(
        &app_state,
        &proxy_session_id,
        &access_lease_id.0,
        super::super::current_time_millis(),
        60_000,
    )
    .await;
    let strip = app_state.hls.proxy.strip();

    let response = super::super::hls_shared_provisioning_timeline_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
    )
    .await
    .expect("provisioning manifest should render");

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest is utf8");
    assert!(body.contains("#EXT-X-VERSION:7\n"));
    assert!(body.contains("#EXT-X-INDEPENDENT-SEGMENTS\n"));
    assert!(body.contains("#EXT-X-TARGETDURATION:2\n"));
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
    assert!(body.contains("#EXTINF:2.000,\n"));
    assert!(!body.contains("#EXTINF:12.000,\n"));
    assert!(body.contains("/hls/shared/live/"));
    assert!(body.contains("/000000.ts?pseq=0"));
    assert!(body.contains("/000001.ts?pseq=1"));
    assert!(body.contains("/000002.ts?pseq=2"));
    assert!(body.matches("#EXTINF:").count() <= 6);
    assert!(!body.contains("/cvs/hls/"));
    {
        let session = session.read().await;
        assert_eq!(session.proxy_next_seq, Some(3));
        assert_eq!(session.publishable_origin_head_proxy_seq, Some(0));
        assert_eq!(session.publishable_origin_tail_proxy_seq, Some(2));
        assert_eq!(session.segments.len(), 3);
    }

    let response = super::super::hls_shared_provisioning_timeline_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
    )
    .await
    .expect("subsequent manifest should append one segment");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest is utf8");
    assert!(body.contains("/000003.ts?pseq=3"));
    assert!(body.matches("#EXTINF:").count() <= 6);
    assert_eq!(session.read().await.proxy_next_seq, Some(4));
}

#[tokio::test]
async fn stale_provisioning_segments_do_not_trigger_canonical_handoff() {
    let input = single_hls_provider_input("stale-provisioning-handoff-input");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy_and_inputs(hls_proxy, vec![Arc::new(input.clone())]);
    enable_hls_provisioning_custom_response(&app_state);
    let session = create_unbound_hls_test_session(&app_state, &input, "54321", 1_000).await;
    let initial_lease_id = HlsAccessLeaseId("initial-provisioning-lease".to_string());
    let strip = app_state.hls.proxy.strip();

    super::super::hls_shared_provisioning_timeline_manifest_response(
        &app_state,
        &session,
        &initial_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
    )
    .await
    .expect("provisioning manifest should render local segments");
    {
        let session_guard = session.read().await;
        assert!(session_guard.segments.values().any(crate::api::model::is_hls_provisioning_segment));
        assert_eq!(session_guard.segments.len(), 3);
        assert_eq!(session_guard.pending_handoff_discontinuity_sequence, None);
    }

    let new_lease_id = HlsAccessLeaseId("new-playback-lease".to_string());
    let previous_rendered_at = super::super::maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
        &app_state,
        &session,
        &input,
        54321,
        &new_lease_id,
        2_000,
    )
    .await;

    assert_eq!(previous_rendered_at, None);
    let session_guard = session.read().await;
    assert_eq!(session_guard.segments.len(), 3);
    assert_eq!(session_guard.pending_handoff_discontinuity_sequence, None);
}

#[tokio::test]
async fn provisioning_handoff_finds_shared_session_by_input_stream_id_not_virtual_id() {
    let input = single_hls_provider_input("origin-id-handoff-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    let origin_session = create_unbound_hls_test_session(&app_state, &input, "80510", 1_000).await;
    let virtual_id_session = create_unbound_hls_test_session(&app_state, &input, "1001", 1_000).await;
    let stream_identity = super::super::HlsEntryStreamIdentity::new(1001, "80510").expect("input stream identity");

    assert!(
        super::super::mark_hls_provisioning_handoff_discontinuity(&app_state, &input, &stream_identity, None, 2_000,)
            .await
    );

    assert!(origin_session.read().await.pending_handoff_discontinuity_sequence.is_some());
    assert_eq!(virtual_id_session.read().await.pending_handoff_discontinuity_sequence, None);
}

#[tokio::test]
async fn shared_provisioning_handoff_continues_proxy_sequence_for_origin_segments() {
    let input = single_hls_provider_input("shared-provisioning-handoff-input");
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy_and_inputs(hls_proxy, vec![Arc::new(input.clone())]);
    enable_hls_provisioning_custom_response(&app_state);
    let session = create_unbound_hls_test_session(&app_state, &input, "shared-handoff-12345", 1_000).await;
    let access_lease_id = HlsAccessLeaseId("handoff-lease".to_string());
    let strip = app_state.hls.proxy.strip();
    super::super::hls_shared_provisioning_timeline_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
    )
    .await
    .expect("provisioning manifest should render");

    let manifest = match parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:4025\n#EXTINF:4.0,\n4025.ts\n#EXTINF:4.0,\n4026.ts\n#EXTINF:4.0,\n4027.ts\n",
            "http://origin.example/live/stream.m3u8",
        ) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        };
    let rendered = {
        let mut session_guard = session.write().await;
        session_guard.mark_pending_handoff_discontinuity(0);
        drop(session_guard);
        assert!(
            super::super::ensure_shared_hls_provisioning_handoff_gap(&app_state, &session, 2_000).await,
            "handoff should append one gap segment"
        );
        let mut session_guard = session.write().await;
        session_guard.apply_origin_manifest(&manifest).expect("origin manifest should map");
        for proxy_seq in 4..=6 {
            session_guard.segments.get_mut(&proxy_seq).expect("origin segment").status =
                SegmentCacheStatus::Ready { content_length: 1024, ready_at_ms: 2_000 };
        }
        session_guard.render_and_store_manifest(2_000).expect("handoff manifest should render")
    };

    assert!(rendered.body.contains("#EXT-X-MEDIA-SEQUENCE:1\n"));
    assert!(rendered.body.contains("#EXT-X-TARGETDURATION:4\n"));
    assert!(rendered.body.contains("/000002.ts?pseq=2"));
    assert!(rendered.body.contains("/000004.ts"));
    assert!(rendered.body.contains("/000005.ts"));
    assert!(rendered.body.contains("/000006.ts"));
    assert!(!rendered.body.contains("/004025.ts"));
    let provisioning_tail = rendered.body.find("/000002.ts?pseq=2").expect("provisioning tail is rendered");
    let gap_tag = rendered.body.find("#EXT-X-GAP\n").expect("handoff gap tag is rendered");
    let gap_uri = rendered.body.find("/000003.ts?pseq=3").expect("handoff gap uri is rendered");
    let discontinuity = rendered
        .body
        .find("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,\n/hls/shared/live/")
        .expect("origin handoff discontinuity is rendered");
    let first_origin = rendered.body.find("/000004.ts").expect("first origin segment is rendered");
    assert!(provisioning_tail < gap_tag);
    assert!(gap_tag < gap_uri);
    assert!(gap_uri < discontinuity);
    assert!(discontinuity < first_origin);
}

#[tokio::test]
async fn canonical_recovery_from_provisioning_marks_normal_handoff_boundary() {
    let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
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
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
    app_state.hls.provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::super::current_time_millis());

    let response = super::super::try_hls_cache_canonical_manifest_response(
        &app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: &request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(&request_url),
            input: &input,
            origin_source,
        },
        HeaderMap::new(),
        None,
        "/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("canonical hls cache should recover from provisioning");

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
    assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,"));
    assert!(!app_state.hls.provisioning.has_consumer(&input.name, 12345, super::super::current_time_millis()));
}

#[tokio::test]
async fn canonical_recovery_from_provisioning_marks_transient_handoff_boundary() {
    let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.key\"\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
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
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
    app_state.hls.provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::super::current_time_millis());

    let response = super::super::try_hls_cache_canonical_manifest_response(
        &app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: &request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(&request_url),
            input: &input,
            origin_source,
        },
        HeaderMap::new(),
        None,
        "/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("canonical hls cache should recover from provisioning");

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
    assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,"));
    assert!(!app_state.hls.provisioning.has_consumer(&input.name, 12345, super::super::current_time_millis()));
}
