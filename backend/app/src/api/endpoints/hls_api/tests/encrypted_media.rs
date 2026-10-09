use super::{
    create_active_hls_user_session, enable_hls_cache, encrypt_test_aes128_cbc_pkcs7, get_response, hls_proxy_uri,
    map_transient_resource_with_kind, prepare_pending_test_hls_access_lease, response_body,
    spawn_test_encrypted_hls_origin, spawn_test_transient_origin_with_response, terminal_test_asset, test_app_state,
    test_app_state_with_hls_proxy_and_inputs, test_fingerprint, test_hls_access_context, test_hls_sequence_iv,
    try_test_hls_cached_manifest_response, TestSegmentOrigin, AES_TEST_KEY_BYTES, AES_TEST_MANIFEST,
    AES_TEST_PLAINTEXT_SEGMENT,
};
use crate::{
    api::model::{
        build_proxy_session_id, build_terminal_tail_plan, prepare_terminal_base_evidence, AppState, HlsAccessLease,
        HlsAccessLeaseId, HlsAccessLeaseState, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode, HlsLifecycleEvent,
        HlsLifecycleEventKey, HlsManifestDeliveryMode, HlsPlaybackFamilyKey, HlsProxyManager,
        HlsRuntimeCustomTailAssetIdentity, HlsSessionHandle, HlsSessionKey, HlsSessionMode, HlsTerminalAssetIdentity,
        HlsTerminalMediaAsset, HlsTerminalTailBuildInput, HlsTerminalTailCompatibility, HlsTerminalTailGeneration,
        HlsTerminalTailProtection, ProxySessionId, TransientObjectCacheKey, TransientResourceKind,
    },
    model::{ConfigInput, StripConfig},
};
use axum::http::{header, HeaderMap, StatusCode};
use shared::model::{HlsStripMode, InputType};
use std::{sync::Arc, time::Duration};

pub(in crate::api::endpoints::hls_api::tests) const AES_TEST_ROTATED_KEY_BYTES: &[u8] = b"fedcba9876543210";

pub(in crate::api::endpoints::hls_api::tests) struct AesEndpointFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp_dir: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) input: ConfigInput,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) request_url: String,
    pub(in crate::api::endpoints::hls_api::tests) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api::tests) proxy_session_id: ProxySessionId,
    pub(in crate::api::endpoints::hls_api::tests) access_lease_id: HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api::tests) key_uri: String,
    pub(in crate::api::endpoints::hls_api::tests) base_manifest: HlsLeaseManifestSnapshot,
    pub(in crate::api::endpoints::hls_api::tests) asset: Arc<HlsTerminalMediaAsset>,
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_aes_live_endpoint(
    app_state: &Arc<AppState>,
    origin: &TestSegmentOrigin,
    access_lease_id: &HlsAccessLeaseId,
    live_body: &str,
) -> String {
    let key_uri = live_body
        .lines()
        .find(|line| line.starts_with("#EXT-X-KEY:METHOD=AES-128"))
        .and_then(|line| line.split_once("URI=\"").map(|(_, tail)| tail))
        .and_then(|tail| tail.split_once('"').map(|(uri, _)| uri.to_string()))
        .expect("opaque key URI");
    assert!(key_uri.contains(&format!("/{}/r/", access_lease_id.0)));
    assert!(!live_body.contains("key.bin"));
    assert!(live_body.contains("#EXT-X-VERSION:5\n"));
    let proxy_media_sequence = live_body
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .and_then(|value| value.parse::<u64>().ok())
        .expect("proxy media sequence");
    assert_ne!(proxy_media_sequence, 77);
    assert!(live_body.contains("IV=0x0000000000000000000000000000004d"));
    assert_eq!(origin.key_request_count(), 1);
    let segment_uris = live_body
        .lines()
        .filter(|line| line.starts_with("/hls/shared/live/") && !line.contains("/r/"))
        .collect::<Vec<_>>();
    assert_eq!(segment_uris.len(), 6);
    for (index, segment_uri) in segment_uris.into_iter().enumerate() {
        let response = get_response(Arc::clone(app_state), segment_uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let served = response_body(response).await;
        let origin_sequence = 77_u64.saturating_add(u64::try_from(index).expect("test segment index"));
        let expected = encrypt_test_aes128_cbc_pkcs7(
            AES_TEST_PLAINTEXT_SEGMENT,
            AES_TEST_KEY_BYTES,
            test_hls_sequence_iv(origin_sequence),
        );
        assert_eq!(served.as_ref(), expected.as_slice());
        assert_ne!(served.as_ref(), AES_TEST_PLAINTEXT_SEGMENT);
        assert_eq!(origin.key_request_count(), 1);
    }
    key_uri
}

pub(in crate::api::endpoints::hls_api::tests) async fn aes_endpoint_fixture() -> AesEndpointFixture {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let origin = spawn_test_encrypted_hls_origin(
        AES_TEST_MANIFEST,
        Arc::from(AES_TEST_KEY_BYTES),
        Arc::from(AES_TEST_PLAINTEXT_SEGMENT),
    )
    .await;
    let input = ConfigInput {
        id: 1,
        name: Arc::from("encrypted-m3u-input"),
        input_type: InputType::M3u,
        url: origin.base_url.clone(),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    };
    let app_state = test_app_state_with_hls_proxy_and_inputs(
        Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)),
        vec![Arc::new(input.clone())],
    );
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let request_url = format!("{}/channel/index.m3u8", origin.base_url);
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("encrypted-normal-lease".to_string());
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
    .expect("compatible AES origin should enter normal HLS cache");
    assert_eq!(response.status(), StatusCode::OK);
    let live_body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
    let key_uri = assert_aes_live_endpoint(&app_state, &origin, &access_lease_id, &live_body).await;
    assert_eq!(origin.segment_request_count(), 6);
    let session = app_state.hls.proxy.sessions().get_by_key(&session_key).await.expect("normal encrypted session");
    assert_eq!(session.read().await.mode, HlsSessionMode::NormalCacheTimeline);
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("live encrypted lease snapshot");
    let base_manifest = lease.last_manifest_snapshot.expect("authoritative lease manifest");
    assert_eq!(base_manifest.delivery_mode, HlsManifestDeliveryMode::NormalCacheTimeline);
    assert_eq!(
        base_manifest.active_encryption.as_ref().and_then(|encryption| encryption.iv.as_deref()),
        Some("0x00000000000000000000000000000052")
    );
    let asset = terminal_test_asset();
    let evidence = prepare_terminal_base_evidence(
        &session,
        app_state.hls.proxy.segment_cache(),
        &base_manifest,
        super::super::current_time_millis(),
    )
    .await;
    assert_eq!(evidence.track_signature(), Some(asset.track_signature().clone()));
    assert_eq!(evidence.key_bindings().len(), 1);
    assert_eq!(origin.key_request_count(), 1);
    evidence.release();
    AesEndpointFixture {
        _temp_dir: temp_dir,
        origin,
        input,
        app_state,
        request_url,
        session,
        proxy_session_id,
        access_lease_id,
        key_uri,
        base_manifest,
        asset,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn install_aes_terminal_plan(
    fixture: &AesEndpointFixture,
) -> TransientObjectCacheKey {
    let key_response = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
    assert_eq!(key_response.status(), StatusCode::OK);
    assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_KEY_BYTES);
    assert_eq!(fixture.origin.key_request_count(), 1);
    let evidence_key_id = {
        let mut session = fixture.session.write().await;
        let key_id = session
            .transient
            .resources
            .values()
            .find(|resource| resource.kind == TransientResourceKind::Key)
            .map(|resource| resource.id.clone())
            .expect("normal key resource");
        session.last_rendered_manifest = None;
        session.transient.last_manifest_body = None;
        session.transient.resources.get_mut(&key_id).expect("key resource").expires_at_ms = 0;
        for (key, object) in &mut session.transient.object_cache {
            if key.transient_resource_id() == &key_id {
                object.expires_at_ms = 0;
            }
        }
        key_id
    };
    let evidence = prepare_terminal_base_evidence(
        &fixture.session,
        fixture.app_state.hls.proxy.segment_cache(),
        &fixture.base_manifest,
        super::super::current_time_millis(),
    )
    .await;
    assert_eq!(evidence.track_signature(), Some(fixture.asset.track_signature().clone()));
    assert_eq!(fixture.origin.key_request_count(), 1);
    fixture
        .app_state
        .hls
        .proxy
        .run_garbage_collection_once(super::super::current_time_millis())
        .await
        .expect("evidence-pinned GC");
    {
        let session = fixture.session.read().await;
        assert!(session.transient.resources.contains_key(&evidence_key_id));
        assert!(session.transient.object_cache.keys().any(|key| key.transient_resource_id() == &evidence_key_id));
    }
    let plan = build_terminal_tail_plan(HlsTerminalTailBuildInput {
        generation: HlsTerminalTailGeneration(23),
        created_at_ms: super::super::current_time_millis(),
        base_availability: evidence.availability(),
        base_track_signature: evidence.track_signature(),
        base_splice_evidence: evidence.splice_evidence().cloned(),
        terminal_splice_evidence: Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&fixture.asset)),
        base_timing: evidence.timing().cloned(),
        base_key_bindings: evidence.key_bindings(),
        expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(
            &fixture.asset,
        )),
        base_manifest: fixture.base_manifest.clone(),
        anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(
            &fixture.asset,
            fixture.base_manifest.target_duration_ms,
        ),
        asset: Arc::clone(&fixture.asset),
    })
    .expect("READY AES key permits safe terminal reset");
    let protection = HlsTerminalTailProtection {
        generation: plan.generation,
        base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
        key_bindings: plan.key_bindings(),
    };
    assert_eq!(protection.key_bindings.len(), 1);
    assert_eq!(protection.key_bindings[0].resource_id(), &evidence_key_id);
    let frozen_source_cache_key = protection.key_bindings[0].source_cache_key().clone();
    {
        let mut leases = fixture.app_state.hls.proxy.access_leases().write().await;
        let mut lease = leases.remove_access_lease(&fixture.access_lease_id).expect("live lease");
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::new(plan));
        leases.prepare_access_lease(lease);
    }
    fixture.session.write().await.install_terminal_tail_protection(fixture.access_lease_id.clone(), protection);
    evidence.release();
    frozen_source_cache_key
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_aes_terminal_endpoints(fixture: &AesEndpointFixture) {
    let manifest_uri =
        format!("/hls/shared/live/{}/{}/manifest.m3u8", fixture.proxy_session_id.0, fixture.access_lease_id.0);
    let response = get_response(Arc::clone(&fixture.app_state), &manifest_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal utf8");
    let key = body.find("#EXT-X-KEY:METHOD=AES-128").expect("base AES key");
    let reset = body.find("#EXT-X-KEY:METHOD=NONE").expect("clear key reset");
    let discontinuity = body[reset..].find("#EXT-X-DISCONTINUITY").expect("terminal discontinuity") + reset;
    assert!(key < reset && reset < discontinuity);
    assert!(body.contains("IV=0x00000000000000000000000000000052"));
    assert!(body.contains("#EXT-X-VERSION:5\n"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    let key_response = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
    assert_eq!(key_response.status(), StatusCode::OK);
    assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_KEY_BYTES);
    assert_eq!(fixture.origin.key_request_count(), 1);
    let range = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, Some("bytes=4-7")).await;
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response_body(range).await.as_ref(), &AES_TEST_KEY_BYTES[4..=7]);
    let unsatisfiable = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, Some("bytes=16-")).await;
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */16");
}

pub(in crate::api::endpoints::hls_api::tests) async fn assert_aes_rotated_live_key(
    fixture: &AesEndpointFixture,
    frozen_source_cache_key: &TransientObjectCacheKey,
) -> String {
    let manifest_requests_before = fixture.origin.manifest_request_count();
    let segment_requests_before = fixture.origin.segment_request_count();
    fixture.origin.set_key_bytes(Arc::from(AES_TEST_ROTATED_KEY_BYTES)).await;
    let live_lease_id = HlsAccessLeaseId("encrypted-rotated-live-lease".to_string());
    let access_context = test_hls_access_context(fixture.proxy_session_id.clone(), live_lease_id.clone());
    prepare_pending_test_hls_access_lease(&fixture.app_state, &fixture.proxy_session_id, &live_lease_id).await;
    let response = super::super::try_hls_cache_canonical_manifest_response(
        &fixture.app_state,
        &test_fingerprint(),
        &access_context,
        &fixture.proxy_session_id,
        &live_lease_id,
        HlsAccessLeaseState::Pending,
        super::super::HlsCacheManifestOrigin {
            raw_request_url: &fixture.request_url,
            session_entry_url: super::super::HlsOriginEntryUrl::direct_http(&fixture.request_url),
            input: &fixture.input,
            origin_source: super::super::build_hls_origin_source(&fixture.input, "12345"),
        },
        HeaderMap::new(),
        None,
        "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
        super::super::HlsManifestRefreshOrdering::Background,
    )
    .await
    .expect("a new live lease should reuse the recovered shared session");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.origin.manifest_request_count(), manifest_requests_before.saturating_add(1));
    assert_eq!(fixture.origin.segment_request_count(), segment_requests_before);
    let live_lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&live_lease_id, &fixture.proxy_session_id, super::super::current_time_millis())
        .await
        .expect("rotated live lease snapshot");
    assert_eq!(live_lease.playback_mode, HlsLeasePlaybackMode::Live);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("rotated manifest utf8");
    let rotated_key_uri = body
        .lines()
        .find(|line| line.starts_with("#EXT-X-KEY:METHOD=AES-128"))
        .and_then(|line| line.split_once("URI=\"").map(|(_, tail)| tail))
        .and_then(|tail| tail.split_once('"').map(|(uri, _)| uri.to_string()))
        .expect("rotated live key URI");
    let key_response = get_response(Arc::clone(&fixture.app_state), &rotated_key_uri, None).await;
    assert_eq!(key_response.status(), StatusCode::OK);
    assert_eq!(response_body(key_response).await.as_ref(), AES_TEST_ROTATED_KEY_BYTES);
    assert_eq!(fixture.origin.key_request_count(), 2);
    assert!(fixture
        .app_state
        .hls
        .proxy
        .segment_cache()
        .metadata(frozen_source_cache_key)
        .await
        .expect("A metadata lookup")
        .is_none());
    let terminal_key = get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await;
    assert_eq!(terminal_key.status(), StatusCode::OK);
    assert_eq!(response_body(terminal_key).await.as_ref(), AES_TEST_KEY_BYTES);
    assert_eq!(fixture.origin.key_request_count(), 2);
    rotated_key_uri
}

pub(in crate::api::endpoints::hls_api::tests) async fn expire_aes_terminal_lease(
    fixture: &AesEndpointFixture,
    rotated_key_uri: &str,
) {
    let expired_at_ms = super::super::current_time_millis().saturating_sub(1);
    {
        let mut leases = fixture.app_state.hls.proxy.access_leases().write().await;
        let mut lease = leases.remove_access_lease(&fixture.access_lease_id).expect("terminal lease");
        lease.valid_until_ms = expired_at_ms;
        leases.prepare_access_lease(lease);
    }
    fixture
        .app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &fixture.app_state.active_users,
            &fixture.app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: fixture.access_lease_id.clone(),
                    proxy_session_id: fixture.proxy_session_id.clone(),
                },
                due_at_ms: expired_at_ms,
            },
            super::super::current_time_millis(),
        )
        .await;
    fixture
        .app_state
        .hls
        .proxy
        .run_garbage_collection_once(super::super::current_time_millis())
        .await
        .expect("released GC");
    assert!(fixture.session.read().await.terminal_tail_protection(&fixture.access_lease_id).is_none());
    assert_eq!(
        get_response(Arc::clone(&fixture.app_state), &fixture.key_uri, None).await.status(),
        StatusCode::NOT_FOUND
    );
    let live_key = get_response(Arc::clone(&fixture.app_state), rotated_key_uri, None).await;
    assert_eq!(live_key.status(), StatusCode::OK);
    assert_eq!(response_body(live_key).await.as_ref(), AES_TEST_ROTATED_KEY_BYTES);
}

#[tokio::test]
async fn aes_128_normal_origin_key_and_terminal_lifecycle_are_endpoint_safe() {
    let fixture = aes_endpoint_fixture().await;
    let frozen_source_cache_key = install_aes_terminal_plan(&fixture).await;
    assert_aes_terminal_endpoints(&fixture).await;
    let rotated_key_uri = assert_aes_rotated_live_key(&fixture, &frozen_source_cache_key).await;
    expire_aes_terminal_lease(&fixture, &rotated_key_uri).await;
}

pub(in crate::api::endpoints::hls_api::tests) fn assert_encrypted_transient_terminal_incompatibility(
    snapshot: HlsLeaseManifestSnapshot,
    now_ms: u64,
) {
    let asset = terminal_test_asset();
    let bundle_target_duration_ms = snapshot.target_duration_ms.max(asset.duration_ms());
    let base_timing = Some(HlsTerminalTailBuildInput::base_timing_for_test(&asset, &snapshot));
    let base_splice_evidence = Some(HlsTerminalTailBuildInput::compatible_splice_evidence_for_test(&asset));
    let terminal_splice_evidence = base_splice_evidence.clone();
    assert_eq!(
        build_terminal_tail_plan(HlsTerminalTailBuildInput {
            generation: HlsTerminalTailGeneration(1),
            created_at_ms: now_ms,
            base_availability: Arc::from([]),
            base_track_signature: Some(asset.track_signature().clone()),
            base_splice_evidence,
            terminal_splice_evidence,
            base_timing,
            base_key_bindings: Arc::from([]),
            expected_asset: HlsRuntimeCustomTailAssetIdentity::channel_unavailable(
                HlsTerminalAssetIdentity::from_asset(&asset),
            ),
            anchored_bundle: HlsTerminalTailBuildInput::anchored_bundle_for_test(&asset, bundle_target_duration_ms,),
            base_manifest: snapshot,
            asset,
        }),
        Err(HlsTerminalTailCompatibility::TransientPassthroughUnsupported)
    );
}

#[tokio::test]
async fn encrypted_transient_endpoint_stores_client_visible_key_and_typed_terminal_incompatibility() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let now_ms = super::super::current_time_millis();
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), now_ms)
        .await;
    let (proxy_session_id, access_lease_id) = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.clone();
        let access_lease_id = HlsAccessLeaseId("encrypted-access-lease".to_string());
        let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:20\n\
                 #EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/{}/{}/r/key.bin\",IV=0x1,KEYFORMAT=\"identity\",KEYFORMATVERSIONS=\"1\"\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/20.ts\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/21.ts\n\
                 #EXTINF:4.0,\n/hls/shared/live/{}/{}/r/22.ts\n",
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                proxy_session_id.0,
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
            );
        session.transient.replace_manifest_with_semantics(body, now_ms, Some(12_000));
        session.mark_authorized_media_access(now_ms);
        (proxy_session_id, access_lease_id)
    };
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            access_lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "encrypted-client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "encrypted-session".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            60_000,
        ))
        .await;

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("encrypted transient response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms)
        .await
        .expect("lease snapshot");
    let snapshot = lease.last_manifest_snapshot.expect("manifest snapshot");
    let encryption = snapshot.active_encryption.as_ref().expect("active encryption");

    assert!(body.contains(&format!("URI=\"/hls/shared/live/{}/{}/r/key.bin\"", proxy_session_id.0, access_lease_id.0)));
    assert_eq!(snapshot.delivery_mode, HlsManifestDeliveryMode::TransientPassthrough);
    assert_eq!(encryption.method, "AES-128");
    assert_eq!(encryption.iv.as_deref(), Some("0x1"));
    assert_eq!(encryption.key_format.as_deref(), Some("identity"));
    assert_eq!(encryption.key_format_versions.as_deref(), Some("1"));
    assert!(encryption.can_reset_to_clear);

    assert_encrypted_transient_terminal_incompatibility(snapshot, now_ms);
}

#[tokio::test]
async fn transient_key_resource_is_not_cached() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin_with_response(
        "200 OK",
        &[("Content-Type", "application/octet-stream")],
        "key-bytes",
    )
    .await;
    let (proxy_session_id, resource_id) = map_transient_resource_with_kind(
        &app_state,
        &format!("{}/key.bin", origin.base_url),
        "key",
        true,
        TransientResourceKind::Key,
    )
    .await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.key")).await;

    let first = get_response(Arc::clone(&app_state), &uri, None).await;
    let second = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"key-bytes"));
    assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"key-bytes"));
    assert_eq!(origin.requests.lock().await.len(), 2);
}
