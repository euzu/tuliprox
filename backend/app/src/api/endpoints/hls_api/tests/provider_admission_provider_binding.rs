use super::{
    cache_recording_test_item, configure_recording_test_listener, create_active_hls_user_session,
    create_bound_hls_test_session, enable_hls_cache, media_uri_count, overlap_provider_input,
    provider_admission::stats_provider_test_user_session, recording_test_response, recording_test_router,
    recording_test_url, response_body, single_hls_provider_input, spawn_recording_header_origin,
    store_test_sources_with_target, test_app_state, test_app_state_with_inputs, test_fingerprint,
    test_hls_access_context, test_hls_share_target, test_m3u_hls_item, test_m3u_hls_share_target,
    transient_manifest_body, try_test_hls_cached_manifest_response,
};
use crate::{
    api::model::{
        build_proxy_session_id, CacheAccessState, ConnectionKind, HlsAccessLeaseId, HlsAccessLeaseState,
        HlsOriginAccountBinding, HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason, HlsOriginSource,
        HlsOriginSourceKind, HlsSegmentFile, HlsSessionKey, HlsSessionMode, OriginSegmentFetchRef, OriginSegmentKey,
        ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
    },
    model::{ConfigInput, ProxyUserCredentials, StripConfig},
};
use axum::http::{header, HeaderMap, StatusCode};
use http_body_util::BodyExt;
use shared::model::{HlsStripMode, InputType, PlaylistItemType, UserConnectionPermission, XtreamCluster};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn transient_origin_binding_requires_runtime_prepare_for_missing_or_detached_account() {
    let input = single_hls_provider_input("known-account");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let proxy_session_id = ProxySessionId("shared-hls-session".to_string());
    let known_binding =
        HlsOriginAccountBinding::new(Arc::clone(&input.name), Arc::clone(&input.name), &proxy_session_id, 1_000);
    let missing_binding =
        HlsOriginAccountBinding::new(Arc::clone(&input.name), Arc::from("removed-account"), &proxy_session_id, 1_000);
    let mut detached_binding = known_binding.clone();
    detached_binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, 2_000);

    let hls_ctx = app_state.hls_ctx();
    assert!(!super::super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &known_binding));
    assert!(super::super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &missing_binding));
    assert!(super::super::hls_transient_origin_binding_requires_runtime_prepare(&hls_ctx, &detached_binding));
}

#[tokio::test]
async fn hls_account_binding_soft_expiry_retains_session_and_reacquires_on_authorized_manifest_work() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", input.name.as_ref(), 1_000).await;
    {
        let mut session = session.write().await;
        session.target_duration = Some(1);
        session.mark_authorized_media_access(1_000);
        session.activity.last_delivered_media_at_ms = Some(1_000);
    }
    let old_generation = session.read().await.activity.origin_work_generation;

    super::super::detach_unprotected_hls_origin_account_bindings(&app_state, 4_001).await;

    {
        let session = session.read().await;
        let binding = session.origin_account_binding.as_ref().expect("detached binding is retained");
        assert!(matches!(
            binding.binding_mode,
            HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation + 1);
    }

    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let prepared_origin = super::super::prepare_hls_origin_runtime(
        &app_state,
        &session,
        &input,
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        "http://root.example.com/live/root-user/root-pass/12345.m3u8",
        &proxy_session_id,
        &test_fingerprint(),
        ConnectionKind::Normal,
        0,
        super::super::HlsOriginWorkKind::Manifest,
        super::super::HlsOriginWorkClass::ManifestInteractive,
        4_100,
    )
    .await
    .expect("authorized origin work can reacquire a detached binding");

    let binding =
        prepared_origin.origin_account_binding_to_store.as_ref().expect("new binding should be stored by caller");
    assert_eq!(binding.account_name.as_ref(), "account-a");
    assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
    assert_eq!(prepared_origin.fetch_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
    app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_origin_account_handle);
}

#[tokio::test]
async fn hls_account_binding_without_media_activity_expires_after_startup_window() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    {
        let mut session = session.write().await;
        session.target_duration = Some(12);
        session.mark_authorized_manifest_access(1_000);
    }
    let old_generation = session.read().await.activity.origin_work_generation;

    super::super::detach_unprotected_hls_origin_account_bindings(&app_state, 5_000).await;
    assert!(session
        .read()
        .await
        .origin_account_binding
        .as_ref()
        .is_some_and(tuliprox_hls::api::HlsOriginAccountBinding::is_active));
    super::super::detach_unprotected_hls_origin_account_bindings(&app_state, 60_000).await;

    let session = session.read().await;
    let binding = session.origin_account_binding.as_ref().expect("binding remains");
    assert!(matches!(
        binding.binding_mode,
        HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
    ));
    assert_eq!(session.activity.origin_work_generation, old_generation + 1);
    assert_eq!(session.activity.last_authorized_media_at_ms, None);
    assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
}

#[tokio::test]
async fn hls_ready_cache_hit_does_not_require_origin_reacquire_when_binding_is_detached() {
    let app_state = test_app_state();
    let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    {
        let mut session = session.write().await;
        session
            .origin_account_binding
            .as_mut()
            .expect("binding exists")
            .detach(HlsOriginAccountDetachedReason::Cleanup, 2_000);
        session.segments.insert(
            123,
            SegmentEntry {
                origin_key: OriginSegmentKey {
                    origin_epoch: 0,
                    effective_host_id: 0,
                    host_local_sequence: 123,
                    host_local_index: 123,
                },
                proxy_seq: 123,
                duration_ms: 4_000,
                proxy_file_ext: "ts".to_string(),
                content_type: "video/mp2t".to_string(),
                cache_key: SegmentCacheKey::new(proxy_session_id, 123, "ts"),
                discontinuity_before: false,
                program_date_time: None,
                daterange_tags_before: Vec::new(),
                origin_byte_range: None,
                map_ref: None,
                encryption: None,
                origin_fetch_ref: Some(OriginSegmentFetchRef {
                    resolved_origin_url: "http://origin.example.com/123.ts".to_string(),
                    byte_range: None,
                    valid_until_ms: None,
                }),
                status: SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
                last_rendered_at_ms: None,
                access: Arc::new(CacheAccessState::new()),
            },
        );
    }

    let segment_file = HlsSegmentFile { proxy_seq: 123, extension: "ts".to_string() };
    assert!(!super::super::hls_segment_request_requires_origin_work(&session, &segment_file).await);
    assert!(super::super::hls_origin_binding_needs_reacquire(&session).await);
}

#[tokio::test]
async fn hls_origin_account_rebind_failure_sets_backoff_without_changing_session_identity() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    create_active_hls_user_session(&app_state).await;
    let input = ConfigInput { id: 1, name: Arc::from("stale-input"), ..ConfigInput::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");
    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            session_key.clone(),
            origin_source.clone(),
            &app_state.get_encrypt_secret(),
            1_000,
        )
        .await;
    {
        let mut session_guard = session.write().await;
        session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("removed-account"),
            &proxy_session_id,
            1_000,
        ));
    }

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
    let session_guard = session.read().await;
    assert_eq!(session_guard.key, session_key);
    assert_eq!(session_guard.proxy_session_id, proxy_session_id);
    let binding = session_guard.origin_account_binding.as_ref().expect("stale binding remains");
    assert_eq!(binding.account_name.as_ref(), "removed-account");
    assert_eq!(binding.generation, 0);
    assert_eq!(session_guard.origin_account_rebind.consecutive_rebind_failures, 1);
    assert!(session_guard.origin_account_rebind.next_rebind_allowed_at_ms.is_some());
}

#[tokio::test]
async fn recording_force_redirect_keeps_vod_and_series_on_the_provider_proxy() -> Result<(), Box<dyn std::error::Error>>
{
    for (output, cluster, item_type) in [
        (shared::model::TargetType::M3u, XtreamCluster::Video, PlaylistItemType::Video),
        (shared::model::TargetType::M3u, XtreamCluster::Series, PlaylistItemType::Series),
        (shared::model::TargetType::Xtream, XtreamCluster::Video, PlaylistItemType::Video),
        (shared::model::TargetType::Xtream, XtreamCluster::Series, PlaylistItemType::Series),
    ] {
        // The proxy answers for an origin that cannot be resolved directly.
        let (proxy_url, requests, proxy_task) = spawn_recording_header_origin().await;
        let provider_url = format!(
            "http://recording-origin.invalid/{}/{}12345.mp4",
            cluster.as_stream_type(),
            if output == shared::model::TargetType::Xtream { "user/pass/" } else { "" }
        );
        let input = ConfigInput {
            id: 1,
            name: Arc::from("recording-input"),
            input_type: if output == shared::model::TargetType::Xtream { InputType::Xtream } else { InputType::M3u },
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            url: if output == shared::model::TargetType::Xtream {
                "http://recording-origin.invalid".to_string()
            } else {
                provider_url.clone()
            },
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let mut target = if output == shared::model::TargetType::Xtream {
            test_hls_share_target(false)
        } else {
            test_m3u_hls_share_target()
        };
        target.use_memory_cache = true;
        target.name = "default".to_string();
        if let Some(options) = target.options.as_mut() {
            options.force_redirect = Some(shared::model::ClusterFlags::all());
        }
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        let storage = tempfile::tempdir()?;
        configure_recording_test_listener(&app_state, storage.path());
        let mut config = (*app_state.app_config.config.load_full()).clone();
        config.proxy = Some(crate::model::ProxyConfig { url: proxy_url, username: None, password: None });
        app_state.app_config.config.store(Arc::new(config));
        let client = crate::utils::request::create_client(&app_state.app_config).build()?;
        app_state.http_clients.default.store(Arc::new(client.clone()));
        app_state.http_clients.no_redirect.store(Arc::new(client));
        let mut item = test_m3u_hls_item(&input, 12345, "12345", &provider_url);
        item.item_type = item_type;
        cache_recording_test_item(&app_state, &target, item).await?;
        let router = recording_test_router(&app_state);
        let recording =
            recording_test_response(&router, &recording_test_url(&app_state, &target, &input, cluster)?).await?;
        assert_eq!(
            recording.status(),
            StatusCode::OK,
            "{cluster:?}: the recording endpoint must relay the provider body"
        );
        assert_eq!(recording.into_body().collect().await?.to_bytes().as_ref(), b"segment-bytes");
        let ordinary_url = if output == shared::model::TargetType::M3u {
            format!(
                "/{}/{}/hls-user/hls-pass/12345.mp4",
                crate::repository::storage_const::M3U_STREAM_PATH,
                cluster.as_stream_type()
            )
        } else {
            format!("/{}/hls-user/hls-pass/12345.mp4", cluster.as_stream_type())
        };
        let ordinary = recording_test_response(&router, &ordinary_url).await?;
        assert!(
            ordinary.status().is_redirection(),
            "{output:?}: ordinary playback keeps force_redirect: {}",
            ordinary.status()
        );
        assert_eq!(
            ordinary.headers().get(header::LOCATION).ok_or("redirect location missing")?.to_str()?,
            provider_url
        );
        let requests = requests.lock().map_err(|error| error.to_string())?.clone();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].starts_with(&format!("get {provider_url} ")), "{requests:?}");
        proxy_task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn hls_entry_origin_reservation_requires_real_provider_handle() {
    let app_state = test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = single_hls_provider_input("missing-provider");

    let reservation = super::super::try_reserve_hls_entry_origin_account_for_redirect(
        &app_state,
        &test_fingerprint(),
        &user,
        &input,
        12345,
        "http://origin.example.com/live/source-user/source-pass/12345.m3u8",
        "hls-session-token",
        "hls-cache:test-session",
        crate::model::PlaybackKind::LiveHls,
        super::super::hls_origin_account_reservation_ttl_secs_fallback(),
        UserConnectionPermission::Allowed,
        ConnectionKind::Normal,
        false,
    )
    .await;

    assert!(reservation.is_none(), "provisioning redirect must not use an exhausted/counter-only check");
}

#[tokio::test]
async fn hls_cache_expired_transient_manifest_with_active_binding_is_served_while_manifest_valid() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let now_ms = super::super::current_time_millis();
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.clone();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_id.0),
            now_ms.saturating_sub(1_000),
            Some(60_000),
        );
        session.mark_authorized_media_access(now_ms.saturating_sub(60_000));
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::from("test-input"),
            Arc::from("test-account"),
            &proxy_session_id,
            now_ms,
        ));
        session.origin_refresh.in_flight = true;
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
        super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
    )
    .await
    .expect("valid committed transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
}

#[tokio::test]
async fn hls_cache_expired_transient_manifest_with_active_binding_is_not_served_after_manifest_validity() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let now_ms = super::super::current_time_millis();
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.clone();
        session.transient.replace_manifest_with_semantics(
            transient_manifest_body(&proxy_session_id.0),
            now_ms.saturating_sub(60_000),
            Some(1_000),
        );
        session.mark_authorized_media_access(now_ms.saturating_sub(60_000));
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::from("test-input"),
            Arc::from("test-account"),
            &proxy_session_id,
            now_ms,
        ));
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
    .await;

    assert!(response.is_none());
}

#[test]
fn hls_cache_stats_provider_prefers_active_origin_account_binding() {
    let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
    let proxy_session_id = ProxySessionId("stats-session".to_string());
    let binding = HlsOriginAccountBinding::new(
        Arc::clone(&origin_source.input_name),
        Arc::from("cdn-dev-alias"),
        &proxy_session_id,
        100,
    );
    let user_session = stats_provider_test_user_session("cdn-dev");

    let provider = super::super::hls_cache_stats_provider(&origin_source, Some(&binding), &user_session);

    assert_eq!(provider.as_ref(), "cdn-dev-alias");
}

#[test]
fn hls_cache_stats_provider_falls_back_when_origin_account_binding_is_not_active() {
    let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
    let proxy_session_id = ProxySessionId("stats-session".to_string());
    let mut binding = HlsOriginAccountBinding::new(
        Arc::clone(&origin_source.input_name),
        Arc::from("cdn-dev-alias"),
        &proxy_session_id,
        100,
    );
    binding.detach(HlsOriginAccountDetachedReason::Cleanup, 200);
    let user_session = stats_provider_test_user_session("session-provider");

    let provider = super::super::hls_cache_stats_provider(&origin_source, Some(&binding), &user_session);

    assert_eq!(provider.as_ref(), "session-provider");
}

#[test]
fn hls_cache_stats_provider_falls_back_to_input_name_without_session_provider() {
    let origin_source = HlsOriginSource::new(1, Arc::from("cdn-dev"), "12345", HlsOriginSourceKind::XtreamLive);
    let user_session = stats_provider_test_user_session("");

    let provider = super::super::hls_cache_stats_provider(&origin_source, None, &user_session);

    assert_eq!(provider.as_ref(), "cdn-dev");
}
