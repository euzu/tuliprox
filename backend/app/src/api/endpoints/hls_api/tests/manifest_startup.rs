use super::{
    access_lease_id_from_variant_uri, access_lease_session_token, assert_no_hls_cache_stream_registered,
    cache_recording_test_item, cache_test_m3u_hls_item, configure_default_test_server,
    configure_recording_test_listener, create_active_hls_user_session, disable_custom_stream_response,
    enable_channel_unavailable_custom_response, enable_hls_cache, get_response, get_status,
    hls_availability_reevaluation_registration_failure_response, hls_canonical_owner_registration, hls_proxy_uri,
    hls_session_last_media_at_ms, legacy_manifest_test_client_headers, legacy_manifest_test_input,
    manifest_media_sequence, map_hls_map, map_ready_segment_without_lease, media_uri_count, normal_manifest,
    normal_manifest_body, path_has_extension, prepare_pending_test_hls_access_lease, proxy_session_id_from_variant_uri,
    publish_ready_test_manifest_for_lease, record_test_normal_manifest_commit, recording_test_response,
    recording_test_router, recording_test_url, response_body, single_hls_provider_input,
    single_variant_master_playlist, single_variant_uri, spawn_recording_header_origin,
    spawn_test_encoded_manifest_origin, spawn_test_segment_origin, spawn_test_transient_origin_with_delayed_response,
    store_normal_manifest_body, store_test_sources_with_target, test_addr_with_port, test_app_state,
    test_app_state_with_hls_proxy, test_app_state_with_hls_proxy_and_inputs, test_app_state_with_inputs,
    test_fingerprint, test_fingerprint_with_addr, test_hls_access_context, test_hls_entry_stream_context,
    test_hls_input, test_hls_share_target, test_m3u_hls_item, test_m3u_hls_share_target, test_segment_entry,
    transient_manifest_body_from_sequence, try_test_hls_cached_manifest_response, HlsCanonicalOwnerRegistration,
    HlsCanonicalOwnerRegistrationFailure, HlsCanonicalOwnerRegistrationKind, TestSegmentOrigin,
};
use crate::{
    api::model::{
        build_proxy_session_id, AppState, ConnectionKind, HlsAccessContext, HlsAccessLeaseId, HlsAccessLeaseState,
        HlsAvailabilityReevaluationRegistration, HlsFreshManifestRequiredReason, HlsLeasePlaybackMode,
        HlsManifestCommitIdentity, HlsManifestCommitRequirement, HlsOriginAccountBinding, HlsOriginSourceKind,
        HlsPlaybackFamilyKey, HlsProxyManager, HlsSession, HlsSessionHandle, HlsSessionKey, HlsSessionStoreOutcome,
        MapCacheStatus, MapEntry, OriginMapKey, ProxyMapId, ProxySessionId, RenderedManifest, SegmentCacheStatus,
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, HlsCacheConfig, ProxyUserCredentials, StripConfig},
};
use axum::{
    body::Body,
    http::{header, HeaderMap, Response, StatusCode},
    response::IntoResponse,
};
use shared::model::{
    HlsCacheConfigDto, HlsStripMode, InputType, PlaylistItemType, UserConnectionPermission, XtreamCluster,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tuliprox_hls::api::MAX_HLS_MANIFEST_BYTES;

#[test]
fn canonical_manifest_joins_authoritative_owner_and_fails_closed_without_one() {
    assert_eq!(
        hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::Scheduled),
        HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::Scheduled)
    );
    assert_eq!(
        hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::AlreadyOwned),
        HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
    );
    assert_eq!(
        hls_canonical_owner_registration(HlsAvailabilityReevaluationRegistration::Superseded),
        HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
    );
    for (registration, failure) in [
        (
            HlsAvailabilityReevaluationRegistration::CapacityExceeded,
            HlsCanonicalOwnerRegistrationFailure::CapacityExceeded,
        ),
        (
            HlsAvailabilityReevaluationRegistration::RuntimeUnavailable,
            HlsCanonicalOwnerRegistrationFailure::RuntimeUnavailable,
        ),
    ] {
        assert_eq!(hls_canonical_owner_registration(registration), HlsCanonicalOwnerRegistration::FailClosed(failure));
        let response = hls_availability_reevaluation_registration_failure_response(failure);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
    }
}

#[tokio::test]
async fn hls_initial_manifest_decision_wait_timeout_defaults_to_ninety_seconds() {
    let app_state = test_app_state();
    assert_eq!(super::super::hls_initial_manifest_decision_wait_timeout(&app_state), Duration::from_secs(90));
}

#[tokio::test]
async fn hls_manifest_channel_unavailable_renders_inline_without_redirect() {
    let app_state = test_app_state();
    enable_channel_unavailable_custom_response(&app_state);

    let response = super::super::hls_manifest_channel_unavailable_response_for_username(&app_state, "hls-user").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
    assert!(body.contains("#EXT-X-ENDLIST"));
}

#[tokio::test]
async fn hls_manifest_channel_unavailable_falls_back_to_not_found_when_custom_response_is_disabled() {
    let app_state = test_app_state();
    disable_custom_stream_response(&app_state);

    let response = super::super::hls_manifest_channel_unavailable_response_for_username(&app_state, "hls-user").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn legacy_hls_manifest_deadline_includes_full_body_read() {
    let origin = spawn_test_encoded_manifest_origin(None, b"#EXTM3U\n".to_vec(), Duration::from_millis(100)).await;
    let input = legacy_manifest_test_input(&origin);
    let hls_config = HlsCacheConfig::from(&HlsCacheConfigDto {
        origin_manifest_timeout_ms: shared::model::Millis::new(10),
        ..Default::default()
    });
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));

    let error = super::super::download_legacy_hls_manifest(&app_state, &input, &legacy_manifest_test_client_headers())
        .await
        .expect_err("complete manifest body read must honor the deadline");

    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn legacy_hls_manifest_distinguishes_invalid_utf8_from_decoder_failure() {
    let invalid_utf8_origin = spawn_test_encoded_manifest_origin(None, vec![0xff], Duration::ZERO).await;
    let invalid_utf8_input = legacy_manifest_test_input(&invalid_utf8_origin);
    let invalid_utf8 = super::super::download_legacy_hls_manifest(
        &test_app_state(),
        &invalid_utf8_input,
        &legacy_manifest_test_client_headers(),
    )
    .await
    .expect_err("invalid UTF-8 must fail");

    let corrupt_origin =
        spawn_test_encoded_manifest_origin(Some("gzip"), vec![0x1f, 0x8b, 0x08, 0x00], Duration::ZERO).await;
    let corrupt_input = legacy_manifest_test_input(&corrupt_origin);
    let decoder_failure = super::super::download_legacy_hls_manifest(
        &test_app_state(),
        &corrupt_input,
        &legacy_manifest_test_client_headers(),
    )
    .await
    .expect_err("corrupt gzip must fail");

    assert_eq!(invalid_utf8.kind(), std::io::ErrorKind::InvalidData);
    assert!(crate::utils::content_coding::content_decoding_error_from_io(&decoder_failure).is_some());
}

#[test]
fn hls_manifest_extension_helper_does_not_create_double_dot_urls() {
    assert_eq!(
        super::super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123.m3u8"),
        "http://origin.example.com/live/user/pass/1025123.m3u8"
    );
    assert_eq!(
        super::super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..m3u8"),
        "http://origin.example.com/live/user/pass/1025123.m3u8"
    );
    assert_eq!(
        super::super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..?token=1"),
        "http://origin.example.com/live/user/pass/1025123.m3u8?token=1"
    );
    assert_eq!(
        super::super::ensure_hls_manifest_extension("provider://mirror/live/user/pass/1025123..m3u8"),
        "provider://mirror/live/user/pass/1025123.m3u8"
    );
}

#[tokio::test]
async fn lease_snapshot_limit_rejection_is_controlled_and_observable() {
    let app_state = test_app_state();
    let proxy_session_id = ProxySessionId("snapshot-limit-session".to_string());
    let access_lease_id = HlsAccessLeaseId("snapshot-limit-lease".to_string());
    let oversized_uri = "x".repeat(MAX_HLS_MANIFEST_BYTES + 1);
    let body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\n{oversized_uri}\n");
    let derivation = super::derive_hls_lease_manifest_snapshot(
        &super::HlsLeaseManifestSnapshotInput::TransientPassthrough {
            materialized_body: &body,
            source_commit_identity: HlsManifestCommitIdentity::committed(1, 1),
            finalized_manifest_generation: None,
        },
        2,
    );

    assert!(super::super::observe_hls_lease_manifest_snapshot_derivation(
        &app_state,
        &proxy_session_id,
        &access_lease_id,
        derivation,
    )
    .is_err());
    assert_eq!(app_state.hls.proxy.metrics().snapshot().manifest_limit_rejections, 1);
}

#[tokio::test]
async fn hls_hard_manifest_failure_forces_next_fresh_commit() {
    let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
    {
        let mut session = session.write().await;
        session.last_rendered_manifest = Some(RenderedManifest {
            body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
            first_proxy_seq: 1,
            last_proxy_seq: 1,
            playlist_duration_ms: 4_000,
            valid_until_ms: 5_000,
            render_gap_segments: 0,
            rendered_at_ms: 1_000,
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            segment_proxy_seqs: vec![1],
        });
        session.require_fresh_manifest_commit(HlsFreshManifestRequiredReason::PreviousHardManifestFailure);
    }

    assert_eq!(
        super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, 2_000).await,
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::PreviousHardManifestFailure
        }
    );
}

#[tokio::test]
async fn hls_normal_expired_session_allows_committed_manifest_while_manifest_valid() {
    let now_ms = 100_000;
    let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
    {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.clone();
        session.target_duration = Some(10);
        session.mark_authorized_media_access(1_000);
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::from("test-input"),
            Arc::from("test-account"),
            &proxy_session_id,
            now_ms,
        ));
        session.last_rendered_manifest = Some(RenderedManifest {
            body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
            first_proxy_seq: 1,
            last_proxy_seq: 1,
            playlist_duration_ms: 4_000,
            valid_until_ms: now_ms.saturating_add(10_000),
            render_gap_segments: 0,
            rendered_at_ms: now_ms.saturating_sub(1_000),
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            segment_proxy_seqs: vec![1],
        });
    }

    assert_eq!(
        super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, now_ms).await,
        HlsManifestCommitRequirement::CommittedManifestAllowed
    );
}

#[tokio::test]
async fn hls_normal_expired_session_requires_fresh_commit_after_manifest_validity() {
    let now_ms = 100_000;
    let session = Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0)));
    {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.clone();
        session.target_duration = Some(10);
        session.mark_authorized_media_access(1_000);
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::from("test-input"),
            Arc::from("test-account"),
            &proxy_session_id,
            now_ms,
        ));
        session.last_rendered_manifest = Some(RenderedManifest {
            body: "#EXTM3U\n#EXTINF:4.0,\n000001.ts\n".to_string(),
            first_proxy_seq: 1,
            last_proxy_seq: 1,
            playlist_duration_ms: 4_000,
            valid_until_ms: now_ms.saturating_sub(1),
            render_gap_segments: 0,
            rendered_at_ms: now_ms.saturating_sub(10_000),
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            segment_proxy_seqs: vec![1],
        });
    }

    assert_eq!(
        super::hls_manifest_commit_requirement(&session, HlsSessionStoreOutcome::Reused, None, now_ms).await,
        HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::ExpiredRevalidation
        }
    );
}

#[tokio::test]
async fn hls_cache_manifest_cold_start_synchronously_returns_initial_manifest() {
    let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
    let input_name = Arc::<str>::from("test-input");
    let input = ConfigInput {
        id: 1,
        name: Arc::clone(&input_name),
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
    .expect("hls cache should handle valid live hls entrypoint");

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(body.contains(&format!("/hls/shared/live/{}/{}/000000.ts", proxy_session_id.0, access_lease_id.0)));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_key(&session_key)
        .await
        .expect("cold start should create shared hls session");
    {
        let session = session.read().await;
        assert!(session.last_rendered_manifest.is_some());
        let binding = session.origin_account_binding.as_ref().expect("plain http input still has account binding");
        assert_eq!(binding.input_name.as_ref(), "test-input");
        assert_eq!(binding.account_name.as_ref(), "test-input");
    }
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn hls_cache_manifest_cold_start_supports_m3u_hls_origin_source() {
    let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:77\n#EXTINF:4.0,\nseg-77.ts\n#EXTINF:4.0,\nseg-78.ts\n#EXTINF:4.0,\nseg-79.ts\n",
        )
        .await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("m3u-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/channel/index.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

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
        "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("hls cache should handle m3u hls media playlists");

    assert_eq!(response.status(), StatusCode::OK);
    let session =
        app_state.hls.proxy.sessions().get_by_key(&session_key).await.expect("m3u hls should create shared session");
    let session = session.read().await;
    assert_eq!(session.origin_source.source_kind, HlsOriginSourceKind::M3uMediaPlaylist);
    let binding = session.origin_account_binding.as_ref().expect("m3u hls input still has account binding");
    assert_eq!(binding.input_name.as_ref(), "m3u-input");
    assert_eq!(binding.account_name.as_ref(), "m3u-input");
}

#[tokio::test]
async fn hls_cache_manifest_cold_start_client_abort_does_not_leave_refresh_in_flight() {
    let origin = spawn_test_transient_origin_with_delayed_response(
        "200 OK",
        &[("Content-Type", "application/vnd.apple.mpegurl")],
        "#EXTM3U\n#EXT-X-VERSION:3\n",
        Duration::from_millis(100),
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
    let hls_dto =
        HlsCacheConfigDto { origin_manifest_timeout_ms: shared::model::Millis::new(1_000), ..Default::default() };
    let hls_config = HlsCacheConfig::from(&hls_dto);
    let app_state = test_app_state_with_hls_proxy_and_inputs(
        Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)),
        vec![Arc::new(input.clone())],
    );
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

    let app_state_for_request = Arc::clone(&app_state);
    let request_handle = tokio::spawn(async move {
        super::super::try_hls_cache_canonical_manifest_response(
            &app_state_for_request,
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
    });

    let session = wait_for_hls_test_session(&app_state, &session_key).await;
    wait_for_hls_refresh_in_flight(&session).await;
    request_handle.abort();
    let _ = request_handle.await;

    for _ in 0..200 {
        if !session.read().await.origin_refresh.in_flight {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let session = session.read().await;
    assert!(!session.origin_refresh.in_flight);
    assert!(session.origin_refresh.last_fetch_finished_at_ms.is_some());
}

#[tokio::test]
async fn hls_cache_canonical_prepare_service_unavailable_sets_retry_after() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("alias-input"),
        aliases: Some(vec![crate::model::ConfigInputAlias {
            id: 2,
            name: Arc::from("alias-account"),
            url: "http://alias.example.com".to_string(),
            username: Some("alias-user".to_string()),
            password: Some("alias-pass".to_string()),
            priority: 0,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());

    let response = super::super::try_hls_cache_canonical_manifest_response(
        &app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(request_url),
            input: &input,
            origin_source,
        },
        HeaderMap::new(),
        None,
        "/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("canonical hls cache response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
}

pub(in crate::api::endpoints::hls_api::tests) struct SharedRequestFlowFixture {
    pub(in crate::api::endpoints::hls_api::tests) _origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) input: ConfigInput,
    pub(in crate::api::endpoints::hls_api::tests) target: ConfigTarget,
    pub(in crate::api::endpoints::hls_api::tests) user: ProxyUserCredentials,
    pub(in crate::api::endpoints::hls_api::tests) origin_manifest_url: String,
    pub(in crate::api::endpoints::hls_api::tests) entry_path: String,
}

pub(in crate::api::endpoints::hls_api::tests) async fn shared_request_flow_fixture() -> SharedRequestFlowFixture {
    const MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n\
            #EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n\
            #EXTINF:4.0,\n000125.ts\n#EXTINF:4.0,\n000126.ts\n#EXTINF:4.0,\n000127.ts\n\
            #EXTINF:4.0,\n000128.ts\n";
    let origin = spawn_test_segment_origin(MANIFEST).await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("request-flow-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    configure_default_test_server(&app_state);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let origin_manifest_url = format!("{}/channel/index.m3u8", origin.base_url);
    cache_test_m3u_hls_item(&app_state, &target, test_m3u_hls_item(&input, 12345, "channel-a", &origin_manifest_url))
        .await;
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let entry_path = super::super::build_virtual_hls_entry_path(&target, &input, &user, 12345);
    SharedRequestFlowFixture { _origin: origin, app_state, input, target, user, origin_manifest_url, entry_path }
}

pub(in crate::api::endpoints::hls_api::tests) async fn shared_request_flow_entry(
    fixture: &SharedRequestFlowFixture,
    fingerprint: &Fingerprint,
) -> Response<Body> {
    super::super::handle_hls_stream_request(
        fingerprint,
        &fixture.app_state,
        &fixture.user,
        &fixture.target,
        None,
        None,
        &fixture.origin_manifest_url,
        None,
        test_hls_entry_stream_context(12345, "channel-a", Some(2_500_000)),
        &fixture.input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &fixture.entry_path,
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response()
}

#[tokio::test]
async fn shared_hls_request_flow_keeps_media_playlist_lease_bound_across_reloads() {
    let fixture = shared_request_flow_fixture().await;
    let app_state = &fixture.app_state;
    let entry_response = shared_request_flow_entry(&fixture, &test_fingerprint()).await;

    assert_eq!(entry_response.status(), StatusCode::OK);
    assert!(!entry_response.headers().contains_key(header::LOCATION));
    let (_, media_playlist_uri) = single_variant_master_playlist(entry_response).await;
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());

    let first_media_response = get_response(Arc::clone(app_state), &media_playlist_uri, None).await;
    assert_eq!(first_media_response.status(), StatusCode::OK);
    assert!(!first_media_response.headers().contains_key(header::LOCATION));
    let first_media_body =
        String::from_utf8(response_body(first_media_response).await.to_vec()).expect("media playlist utf8");
    let segment_uri = first_media_body
        .lines()
        .find(|line| line.starts_with("/hls/shared/live/") && path_has_extension(line, "ts"))
        .expect("lease-bound segment URI")
        .to_string();
    let lease_path = format!("/{}/{}/", proxy_session_id.0, access_lease_id.0);
    assert!(segment_uri.contains(&lease_path));

    let reloaded_media_response = get_response(Arc::clone(app_state), &media_playlist_uri, None).await;
    assert_eq!(reloaded_media_response.status(), StatusCode::OK);
    assert!(!reloaded_media_response.headers().contains_key(header::LOCATION));
    let reloaded_media_body =
        String::from_utf8(response_body(reloaded_media_response).await.to_vec()).expect("reloaded media playlist utf8");
    assert_eq!(manifest_media_sequence(&reloaded_media_body), manifest_media_sequence(&first_media_body));
    assert!(reloaded_media_body.lines().any(|line| line == segment_uri));
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 1);

    let segment_response = get_response(Arc::clone(app_state), &segment_uri, None).await;
    assert_eq!(segment_response.status(), StatusCode::OK);
    assert!(!response_body(segment_response).await.is_empty());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("request-flow access lease");
    assert_eq!(lease.state, HlsAccessLeaseState::Activated);
    assert!(lease.last_manifest_snapshot.is_some());

    let second_entry_response =
        shared_request_flow_entry(&fixture, &test_fingerprint_with_addr(test_addr_with_port(55131))).await;
    assert_eq!(second_entry_response.status(), StatusCode::OK);
    let (_, second_media_playlist_uri) = single_variant_master_playlist(second_entry_response).await;
    let second_proxy_session_id =
        ProxySessionId(proxy_session_id_from_variant_uri(&second_media_playlist_uri).to_string());
    let second_access_lease_id =
        HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_media_playlist_uri).to_string());
    assert_eq!(second_proxy_session_id, proxy_session_id);
    assert_ne!(second_access_lease_id, access_lease_id);
    let second_pending_lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &second_access_lease_id,
            &second_proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("second pending request-flow lease");
    assert_eq!(second_pending_lease.state, HlsAccessLeaseState::Pending);
    assert!(second_pending_lease.last_manifest_snapshot.is_none());
    assert_ne!(second_pending_lease.user_session_token, lease.user_session_token);

    let second_media_response = get_response(Arc::clone(app_state), &second_media_playlist_uri, None).await;
    assert_eq!(second_media_response.status(), StatusCode::OK);
    assert!(!second_media_response.headers().contains_key(header::LOCATION));
    let second_media_body =
        String::from_utf8(response_body(second_media_response).await.to_vec()).expect("second media playlist utf8");
    assert_eq!(manifest_media_sequence(&second_media_body), manifest_media_sequence(&first_media_body));
    let second_published_lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &second_access_lease_id,
            &second_proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("second published request-flow lease");
    assert!(second_published_lease.last_manifest_snapshot.is_some());
    assert_eq!(second_published_lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(app_state.hls.proxy.sessions().len().await, 1);
}

#[tokio::test]
async fn hls_virtual_source_resolver_rejects_missing_input_stream_id_with_service_unavailable() {
    let input = single_hls_provider_input("missing-origin-id-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let target = Arc::new(test_m3u_hls_share_target());
    let item =
        test_m3u_hls_item(&input, 1001, "", "http://account.example.com/live/account-user/account-pass/channel.m3u8");
    cache_test_m3u_hls_item(&app_state, &target, item).await;

    let status = super::super::resolve_hls_virtual_source_for_target(&app_state, &target, 1001)
        .await
        .expect_err("missing input stream identity must fail safely");

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn recording_m3u_dash_keeps_the_redirect_for_relative_media_urls() -> Result<(), Box<dyn std::error::Error>> {
    let (origin_url, requests, origin_task) = spawn_recording_header_origin().await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("recording-input"),
        input_type: InputType::M3u,
        url: origin_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let storage = tempfile::tempdir()?;
    configure_recording_test_listener(&app_state, storage.path());
    let router = recording_test_router(&app_state);
    let manifest_url = format!("{origin_url}/live/manifest.mpd");
    for item_type in [PlaylistItemType::LiveDash, PlaylistItemType::Live] {
        let mut item = test_m3u_hls_item(&input, 12345, "12345", &manifest_url);
        item.item_type = item_type;
        cache_recording_test_item(&app_state, &target, item).await?;
        let response =
            recording_test_response(&router, &recording_test_url(&app_state, &target, &input, XtreamCluster::Live)?)
                .await?;
        assert!(response.status().is_redirection(), "{item_type:?}: {}", response.status());
        assert_eq!(
            response.headers().get(header::LOCATION).ok_or("redirect location missing")?.to_str()?,
            manifest_url
        );
    }
    assert!(requests.lock().map_err(|error| error.to_string())?.is_empty());
    origin_task.abort();
    Ok(())
}

#[tokio::test]
async fn hls_cache_canonical_manifest_rejects_when_target_hls_share_disabled() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let input = test_hls_input();
    let target = test_hls_share_target(false);
    store_test_sources_with_target(&app_state, input, target);
    let access_context = HlsAccessContext {
        username: "hls-user".to_string(),
        user_session_token: "hls-session-token".to_string(),
        proxy_session_id: ProxySessionId("proxy-session".to_string()),
        input_id: 1,
        stream_ref: "12345".to_string(),
        virtual_id: 12345,
        known_bitrate_bps: None,
        lease_id: HlsAccessLeaseId("access-lease".to_string()),
        family_key: HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
        epg_reference_ts: None,
        archive_origin_url: None,
    };

    let Err(err) =
        super::super::resolve_hls_playback_manifest_request_context(&app_state, &access_context, &HeaderMap::new())
            .await
    else {
        panic!("disabled target hls sharing should reject canonical cache path");
    };

    assert_eq!(err, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hls_cache_parallel_real_playbacks_same_virtual_id_register_distinct_streams() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

    let first_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source.clone(),
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let first_variant_uri = single_variant_uri(first_response).await;
    let first_lease_id = access_lease_id_from_variant_uri(&first_variant_uri);
    assert_eq!(proxy_session_id_from_variant_uri(&first_variant_uri), proxy_session_id);
    let proxy_session = ProxySessionId(proxy_session_id.clone());
    let first_session_token =
        access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(first_lease_id.to_string())).await;
    publish_ready_test_manifest_for_lease(
        &app_state,
        &proxy_session,
        &HlsAccessLeaseId(first_lease_id.to_string()),
        4_000,
    )
    .await;
    let first_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{first_lease_id}/000123.ts");
    assert_eq!(get_status(Arc::clone(&app_state), &first_segment_uri).await, StatusCode::OK);

    let second_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let second_variant_uri = single_variant_uri(second_response).await;
    let second_lease_id = access_lease_id_from_variant_uri(&second_variant_uri);
    assert_ne!(first_lease_id, second_lease_id);
    assert_eq!(proxy_session_id_from_variant_uri(&second_variant_uri), proxy_session_id);
    let second_session_token =
        access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(second_lease_id.to_string())).await;
    assert_ne!(first_session_token, second_session_token);
    publish_ready_test_manifest_for_lease(
        &app_state,
        &proxy_session,
        &HlsAccessLeaseId(second_lease_id.to_string()),
        4_000,
    )
    .await;
    let second_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{second_lease_id}/000123.ts");
    assert_eq!(get_status(Arc::clone(&app_state), &second_segment_uri).await, StatusCode::OK);

    let streams = app_state.active_users.active_streams().await;
    assert_eq!(streams.len(), 2);
    let first_stream = streams
        .iter()
        .find(|stream| stream.session_token.as_deref() == Some(first_session_token.as_str()))
        .expect("first stream should be registered");
    let second_stream = streams
        .iter()
        .find(|stream| stream.session_token.as_deref() == Some(second_session_token.as_str()))
        .expect("second stream should be registered");
    let shared_stream_id = super::super::hls_cache_shared_stream_id(&proxy_session);
    assert_ne!(first_stream.session_token, second_stream.session_token);
    assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
    assert_eq!(second_stream.channel.shared_joined_existing, Some(true));
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_server_path_manifest_session(
    app_state: &Arc<AppState>,
) -> (HlsSessionHandle, ProxySessionId) {
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let proxy_session_id = {
        let mut session = session.write().await;
        session.origin_refresh.next_fetch_allowed_at_ms = u64::MAX;
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        let map_id = ProxyMapId(0);
        let mut map = MapEntry::new(
            &session.proxy_session_id,
            map_id,
            OriginMapKey {
                origin_epoch: 0,
                resolved_origin_uri: "http://origin.example.com/init.mp4".to_string(),
                byte_range: None,
            },
            "mp4".to_string(),
        );
        map.status = MapCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms };
        session.maps.insert(map_id, map);
        let mut segment = test_segment_entry(
            &session.proxy_session_id,
            123,
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
        );
        segment.map_ref = Some(map_id);
        session.segments.insert(123, segment);
        session.advance_media_readiness_generation();
        session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXT-X-MAP:URI=\"/hls/shared/live/{proxy_session_id}/{}/map/000000.mp4\"\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_id}/{}/000123.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 123,
                last_proxy_seq: 123,
                playlist_duration_ms: 4_000,
                valid_until_ms: rendered_at_ms.saturating_add(4_000),
                render_gap_segments: 0,
                rendered_at_ms,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![123],
            });
        record_test_normal_manifest_commit(&mut session, rendered_at_ms);
        ProxySessionId(proxy_session_id)
    };
    (session, proxy_session_id)
}

#[tokio::test]
async fn hls_cache_manifest_response_applies_current_users_server_path_without_mutating_session_body() {
    let input_name = Arc::<str>::from("test-input");
    let input = ConfigInput {
        id: 1,
        name: Arc::clone(&input_name),
        input_type: InputType::Xtream,
        url: "http://origin.example.com".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let (session, proxy_session_id) = prepare_server_path_manifest_session(&app_state).await;
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

    let response = super::super::try_hls_cache_canonical_manifest_response(
        &app_state,
        &test_fingerprint(),
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: "http://origin.example.com/live/user/pass/12345.m3u8",
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(
                "http://origin.example.com/live/user/pass/12345.m3u8",
            ),
            input: &input,
            origin_source: super::super::build_hls_origin_source(&input, "12345"),
        },
        HeaderMap::new(),
        Some("/iptv"),
        "/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("hls cache should handle valid live hls entrypoint");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");

    assert!(body.contains(&format!("/iptv/hls/shared/live/{}/", proxy_session_id.0)));
    assert!(body.contains("/map/000000.mp4"));
    assert!(body.contains("/000123.ts"));
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    let stored_body = session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body.clone();
    assert!(stored_body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    assert!(stored_body.contains(&format!("/hls/shared/live/{}/", proxy_session_id.0)));
    assert!(!stored_body.contains("/iptv/hls/shared/live/"));
    assert!(!stored_body.contains(&access_lease_id.0));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
}

#[test]
fn pending_playlist_type_manifest_ignores_three_to_six_window_and_strip() {
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    for segment_count in [2, 8] {
        let body = transient_manifest_body_from_sequence("proxy-session", 100, segment_count).replacen(
            "#EXTM3U\n",
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n",
            1,
        );
        let materialized = super::super::materialize_shared_hls_access_manifest(
            &body,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &StripConfig { mode: HlsStripMode::Segments, value: 5 },
            super::super::HlsManifestWindowPolicy::PreserveFullManifest,
            "transient",
            None,
        );

        assert_eq!(media_uri_count(&materialized.body), segment_count);
        assert!(materialized.body.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(!materialized.body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert_eq!(
            materialized.initial_strip_outcome,
            Some(super::HlsInitialStripOutcome::Skipped {
                reason: super::HlsInitialStripSkipReason::ManifestSemanticsPreserveFullManifest,
                visible_segments: segment_count,
            })
        );
    }
}

#[test]
fn pending_endlist_only_manifest_keeps_complete_body_despite_strip() {
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let mut body = transient_manifest_body_from_sequence("proxy-session", 100, 8);
    body.push_str("#EXT-X-ENDLIST\n");
    let window_policy =
        crate::processing::parser::hls::origin_manifest::parse_manifest_semantics(&body).window_policy();
    assert_eq!(window_policy, super::super::HlsManifestWindowPolicy::PreserveFullManifest);

    let materialized = super::super::materialize_shared_hls_access_manifest(
        &body,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 5 },
        window_policy,
        "transient",
        None,
    );

    assert_eq!(media_uri_count(&materialized.body), 8);
    assert!(materialized.body.contains("#EXT-X-ENDLIST"));
    assert_eq!(
        materialized.initial_strip_outcome,
        Some(super::HlsInitialStripOutcome::Skipped {
            reason: super::HlsInitialStripSkipReason::ManifestSemanticsPreserveFullManifest,
            visible_segments: 8,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn pending_strip_admission_timeout_does_not_commit_speculative_candidate() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let proxy_session_id = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.clone();
        let rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id.0), rendered_at_ms);
        session.segments.get_mut(&1).expect("visible test segment").status = SegmentCacheStatus::Discovered;
        session.advance_media_readiness_generation();
        session.mark_authorized_media_access(rendered_at_ms);
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::from_millis(75)),
    )
    .await
    .expect("timeout response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("pending lease remains available");
    assert!(lease.last_manifest_snapshot.is_none());
}

#[tokio::test]
async fn hls_cache_pending_normal_manifest_applies_initial_strip_without_mutating_shared_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
        session.mark_authorized_media_access(rendered_at_ms);
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    let stored_before = session.read().await.last_rendered_manifest.as_ref().expect("normal manifest").body.clone();

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
    .expect("normal manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
    assert!(body.contains("/000000.ts"));
    assert!(body.contains("/000001.ts"));
    assert!(body.contains("/000002.ts"));
    assert!(!body.contains("/000003.ts"));
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    assert_eq!(session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body, stored_before);
    assert_eq!(media_uri_count(&stored_before), 6);
}

#[tokio::test]
async fn hls_cache_activated_normal_manifest_skips_initial_strip() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
        session.mark_authorized_media_access(rendered_at_ms);
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
    .expect("normal manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 6);
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
}

#[tokio::test]
async fn hls_cache_fresh_required_normal_manifest_does_not_serve_stale_committed_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let rendered_at_ms = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
        session.mark_authorized_media_access(rendered_at_ms);
        rendered_at_ms
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
        super::HlsCachedManifestOptions::initial(Duration::ZERO).requiring_newer_manifest(rendered_at_ms),
    )
    .await;

    assert!(response.is_none());
}

#[tokio::test]
async fn hls_cache_fresh_required_normal_manifest_waits_for_newer_commit() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let (proxy_session_id, old_rendered_at_ms) = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.clone();
        let old_rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id.0), old_rendered_at_ms);
        session.origin_refresh.in_flight = true;
        session.mark_authorized_media_access(old_rendered_at_ms);
        (proxy_session_id, old_rendered_at_ms)
    };
    let session_for_commit = Arc::clone(&session);
    let proxy_session_for_body = proxy_session_id.0.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut session = session_for_commit.write().await;
        let rendered_at_ms = super::super::current_time_millis();
        let mut entry = test_segment_entry(
            &session.proxy_session_id,
            100,
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
        );
        entry.duration_ms = 4_000;
        session.segments.insert(100, entry);
        session.advance_media_readiness_generation();
        session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_for_body}/{}/000100.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 100,
                last_proxy_seq: 100,
                playlist_duration_ms: 4_000,
                valid_until_ms: rendered_at_ms.saturating_add(4_000),
                render_gap_segments: 0,
                rendered_at_ms,
                discontinuity_sequence: 0,
                target_duration_ms: 4_000,
                segment_proxy_seqs: vec![100],
            });
        record_test_normal_manifest_commit(&mut session, rendered_at_ms);
        session.origin_refresh.in_flight = false;
    });
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &strip,
        None,
        super::HlsCachedManifestOptions::initial(Duration::from_millis(200))
            .requiring_newer_manifest(old_rendered_at_ms),
    )
    .await
    .expect("fresh manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert!(body.contains("000100.ts"));
    assert!(!body.contains("000000.ts"));
    assert!(body.contains(&access_lease_id.0));
}

#[tokio::test]
async fn hls_cache_no_media_yet_waits_for_first_normal_manifest_commit() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let proxy_session_id = {
        let mut session = session.write().await;
        session.origin_refresh.in_flight = true;
        session.proxy_session_id.clone()
    };
    let session_for_commit = Arc::clone(&session);
    let proxy_session_for_body = proxy_session_id.0.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut session = session_for_commit.write().await;
        let rendered_at_ms = super::super::current_time_millis();
        let mut entry = test_segment_entry(
            &session.proxy_session_id,
            100,
            SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: rendered_at_ms },
        );
        entry.duration_ms = 4_000;
        session.segments.insert(100, entry);
        session.advance_media_readiness_generation();
        session.last_rendered_manifest = Some(RenderedManifest {
                    body: format!(
                        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n/hls/shared/live/{proxy_session_for_body}/{}/000100.ts\n",
                        crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                    ),
                    first_proxy_seq: 100,
                    last_proxy_seq: 100,
                    playlist_duration_ms: 4_000,
                    valid_until_ms: rendered_at_ms.saturating_add(4_000),
                    render_gap_segments: 0,
                    rendered_at_ms,
                    discontinuity_sequence: 0,
                    target_duration_ms: 4_000,
                    segment_proxy_seqs: vec![100],
                });
        record_test_normal_manifest_commit(&mut session, rendered_at_ms);
        session.origin_refresh.in_flight = false;
    });
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };

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
    .expect("initial normal manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert!(body.contains(&access_lease_id.0));
    assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
}

pub(in crate::api::endpoints::hls_api::tests) async fn wait_for_hls_test_session(
    app_state: &Arc<AppState>,
    session_key: &HlsSessionKey,
) -> HlsSessionHandle {
    for _ in 0..50 {
        if let Some(session) = app_state.hls.proxy.sessions().get_by_key(session_key).await {
            return session;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("shared hls test session should be created");
}

pub(in crate::api::endpoints::hls_api::tests) async fn wait_for_hls_refresh_in_flight(session: &HlsSessionHandle) {
    for _ in 0..50 {
        if session.read().await.origin_refresh.in_flight {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("shared hls refresh should be in flight");
}

#[tokio::test]
async fn ready_hls_proxy_map_without_lease_returns_not_found() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_hls_map(&app_state, b"map-body", false).await;

    let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/map/000000.mp4")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hls_proxy_map_not_ready_with_valid_lease_returns_service_unavailable() {
    let app_state = test_app_state();
    let session =
        app_state.hls.proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"rewrite-secret", 100).await;
    let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
    let proxy_session_id = {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.proxy_session_id.0.clone()
    };
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn invalid_hls_proxy_file_names_return_not_found() {
    let app_state = test_app_state();

    assert_eq!(
        get_status(Arc::clone(&app_state), "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/123.ts").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_status(app_state, "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/map/000123.exe").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn legacy_hls_route_remains_registered() {
    let status = get_status(test_app_state(), "/hls/user/pass/1/2/3/not-a-token").await;

    assert_ne!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn terminate_failed_hls_manifest_session_preserves_shared_lease_when_request_id_available() {
    let mut input = single_hls_provider_input("shared-hls-input");
    input.max_connections = 2;
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let username = "testuser";
    let session_token = "testuser|stream-1|hls|0123456789abcdef";
    let provider_name = Arc::clone(&input.name);
    let addr = test_addr_with_port(55302);

    let handle1 = app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &provider_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(session_token, crate::model::PlaybackKind::LiveHls)),
        )
        .expect("handle1 should be acquired");

    let handle2 = app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &provider_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(session_token, crate::model::PlaybackKind::LiveHls)),
        )
        .expect("handle2 should be acquired");

    let binding_tag = handle1.binding_tag;
    let request_id1 = handle1.playback_request_id;
    let request_id2 = handle2.playback_request_id;
    assert!(request_id1.is_some() && request_id2.is_some() && request_id1 != request_id2);

    app_state.connection_manager.release_provider_handle(Some(handle1));

    super::super::segment::terminate_failed_hls_manifest_session(
        &app_state,
        username,
        session_token,
        Some(&provider_name),
        binding_tag,
        request_id1,
    )
    .await;

    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_some(),
        "shared lease must remain active for surviving request"
    );

    app_state.connection_manager.release_provider_handle(Some(handle2));
    super::super::segment::terminate_failed_hls_manifest_session(
        &app_state,
        username,
        session_token,
        Some(&provider_name),
        binding_tag,
        request_id2,
    )
    .await;

    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_none(),
        "lease should be cleared once all requests are terminated"
    );
}

#[tokio::test]
async fn terminate_failed_hls_manifest_session_clears_identified_reservation_without_request_id() {
    let input = single_hls_provider_input("no-request-id-hls-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let username = "testuser";
    let session_token = "testuser|stream-1|hls|0123456789abcdef";
    let provider_name = Arc::clone(&input.name);
    let addr = test_addr_with_port(55303);

    let handle = app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &provider_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(session_token, crate::model::PlaybackKind::LiveHls)),
        )
        .expect("handle should be acquired");

    let binding_tag = handle.binding_tag;
    assert!(binding_tag.is_some());

    app_state.connection_manager.release_provider_handle(Some(handle));

    assert!(app_state.active_provider.binding_tag_for_owner(session_token).is_some(), "lease should still be active");

    super::super::segment::terminate_failed_hls_manifest_session(
        &app_state,
        username,
        session_token,
        Some(&provider_name),
        binding_tag,
        None,
    )
    .await;

    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_none(),
        "lease should be cleared via binding tag when no request ID is available"
    );
}
