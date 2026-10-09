use super::{
    access_lease_id_from_variant_uri, enable_hls_cache, normal_manifest, proxy_session_id_from_variant_uri,
    response_body, single_variant_master_playlist, test_addr_with_port, test_app_state, test_fingerprint,
    test_fingerprint_with_addr, test_m3u_hls_item,
};
use crate::{
    api::model::{
        AppState, ConnectionKind, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsBandwidthPersistenceState,
        HlsPlaybackFamilyKey, HlsSessionHandle, ProxySessionId, SegmentCacheStatus,
    },
    model::{Config, ConfigInput, ProxyUserCredentials, StripConfig},
};
use axum::http::{header, StatusCode};
use shared::model::{HlsStripMode, InputType, StreamProperties, UserConnectionPermission};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
pub(in crate::api::endpoints::hls_api::tests) enum TestLiveBitrateRepositoryState {
    MissingDatabase,
    MissingStreamItem,
    ExistingHigher,
    Update,
    PermanentlyInapplicable,
    RepositoryIoError,
}

impl TestLiveBitrateRepositoryState {
    pub(in crate::api::endpoints::hls_api::tests) const fn input_name(self) -> &'static str {
        match self {
            Self::MissingDatabase => "bandwidth-missing-database",
            Self::MissingStreamItem => "bandwidth-missing-item",
            Self::ExistingHigher => "bandwidth-existing-higher",
            Self::Update => "bandwidth-update",
            Self::PermanentlyInapplicable => "bandwidth-inapplicable",
            Self::RepositoryIoError => "bandwidth-io-error",
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn prepare_test_live_bitrate_repository(
    input: &ConfigInput,
    storage_root: &std::path::Path,
    repository_state: TestLiveBitrateRepositoryState,
) {
    let storage_path =
        crate::repository::build_input_storage_path(&input.name, storage_root.to_string_lossy().as_ref());
    let database_path = crate::repository::get_input_m3u_playlist_file_path(&storage_path, &input.name);
    match repository_state {
        TestLiveBitrateRepositoryState::MissingDatabase | TestLiveBitrateRepositoryState::PermanentlyInapplicable => {}
        TestLiveBitrateRepositoryState::RepositoryIoError => {
            std::fs::create_dir_all(&storage_path).expect("input storage");
            std::fs::write(&database_path, b"invalid btree data").expect("corrupt input tree");
        }
        TestLiveBitrateRepositoryState::MissingStreamItem
        | TestLiveBitrateRepositoryState::ExistingHigher
        | TestLiveBitrateRepositoryState::Update => {
            std::fs::create_dir_all(&storage_path).expect("input storage");
            let (stream_ref, stored_bitrate) = match repository_state {
                TestLiveBitrateRepositoryState::MissingStreamItem => ("different-channel", 0),
                TestLiveBitrateRepositoryState::ExistingHigher => ("channel-a", 3_000_000),
                TestLiveBitrateRepositoryState::Update => ("channel-a", 0),
                TestLiveBitrateRepositoryState::MissingDatabase
                | TestLiveBitrateRepositoryState::PermanentlyInapplicable
                | TestLiveBitrateRepositoryState::RepositoryIoError => unreachable!(),
            };
            let mut item = test_m3u_hls_item(input, 12345, stream_ref, "http://origin.test/live.m3u8");
            item.additional_properties = Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
                bitrate: stored_bitrate,
                ..Default::default()
            })));
            let mut tree = crate::repository::BPlusTree::new();
            tree.insert(Arc::clone(&item.provider_id), item);
            tree.store(&database_path).expect("input tree");
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn prepare_runtime_bandwidth_session(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    manifest_rendered_at_ms: u64,
) -> HlsSessionHandle {
    let origin_source = super::super::build_hls_origin_source(input, "channel-a");
    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            origin_source.session_key(),
            origin_source,
            &app_state.get_encrypt_secret(),
            manifest_rendered_at_ms,
        )
        .await;
    let manifest = normal_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
    );
    {
        let mut session_guard = session.write().await;
        session_guard.apply_origin_manifest(&manifest).expect("runtime learning timeline");
        for entry in session_guard.segments.values_mut() {
            entry.status =
                SegmentCacheStatus::Ready { content_length: 1_000_000, ready_at_ms: manifest_rendered_at_ms };
        }
        session_guard.advance_media_readiness_generation();
        session_guard.render_and_store_manifest(manifest_rendered_at_ms).expect("runtime learning manifest");
        session_guard.mark_authorized_media_access(manifest_rendered_at_ms);
    }
    session
}

pub(in crate::api::endpoints::hls_api::tests) async fn hls_runtime_bandwidth_manifest_case(
    repository_state: TestLiveBitrateRepositoryState,
) -> (HlsBandwidthPersistenceState, Option<u32>) {
    let temp = tempfile::tempdir().expect("temp dir");
    let app_state = test_app_state();
    let current_config = app_state.app_config.config.load();
    app_state.app_config.config.store(Arc::new(Config {
        storage_dir: temp.path().to_string_lossy().into_owned(),
        ..current_config.as_ref().clone()
    }));
    let input = ConfigInput {
        id: 7,
        name: Arc::from(repository_state.input_name()),
        input_type: if matches!(repository_state, TestLiveBitrateRepositoryState::PermanentlyInapplicable) {
            InputType::Library
        } else {
            InputType::M3u
        },
        ..ConfigInput::default()
    };
    prepare_test_live_bitrate_repository(&input, temp.path(), repository_state);
    let manifest_rendered_at_ms = super::super::current_time_millis();
    let session = prepare_runtime_bandwidth_session(&app_state, &input, manifest_rendered_at_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let access_lease_id = HlsAccessLeaseId(format!("{}-lease", repository_state.input_name()));
    let now_ms = super::super::current_time_millis();
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            access_lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id,
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            input.id,
            "channel-a".to_string(),
            12345,
            now_ms,
            60_000,
        ))
        .await;

    let response = super::super::try_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        super::super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
    )
    .await
    .expect("cached media manifest response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response_body(response).await.is_empty());

    let bandwidth_persistence = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = session.read().await.bandwidth_persistence;
            if !matches!(state, HlsBandwidthPersistenceState::Idle | HlsBandwidthPersistenceState::InFlight { .. }) {
                break state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bandwidth persistence completion");
    let stored_bitrate = if matches!(
        repository_state,
        TestLiveBitrateRepositoryState::ExistingHigher | TestLiveBitrateRepositoryState::Update
    ) {
        crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
            .await
            .expect("stored bitrate read")
    } else {
        None
    };
    (bandwidth_persistence, stored_bitrate)
}

#[tokio::test]
async fn hls_runtime_bandwidth_missing_database_retries_without_failing_media_manifest() {
    let (state, _) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::MissingDatabase).await;

    assert!(matches!(state, HlsBandwidthPersistenceState::RetryAfter { .. }));
}

#[tokio::test]
async fn hls_runtime_bandwidth_missing_item_retries_without_failing_media_manifest() {
    let (state, _) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::MissingStreamItem).await;

    assert!(matches!(state, HlsBandwidthPersistenceState::RetryAfter { .. }));
}

#[tokio::test]
async fn hls_runtime_bandwidth_existing_higher_completes_without_failing_media_manifest() {
    let (state, stored_bitrate) =
        hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::ExistingHigher).await;

    assert!(matches!(state, HlsBandwidthPersistenceState::Persisted { bitrate_bps: 2_000_000 }));
    assert_eq!(stored_bitrate, Some(3_000_000));
}

#[tokio::test]
async fn hls_runtime_bandwidth_update_completes_without_failing_media_manifest() {
    let (state, stored_bitrate) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::Update).await;

    assert!(matches!(state, HlsBandwidthPersistenceState::Persisted { bitrate_bps: 2_000_000 }));
    assert_eq!(stored_bitrate, Some(2_000_000));
}

#[tokio::test]
async fn hls_runtime_bandwidth_inapplicable_and_io_error_do_not_fail_media_manifest() {
    let (inapplicable, _) =
        hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::PermanentlyInapplicable).await;
    let (io_error, _) = hls_runtime_bandwidth_manifest_case(TestLiveBitrateRepositoryState::RepositoryIoError).await;

    assert!(matches!(inapplicable, HlsBandwidthPersistenceState::PermanentlyInapplicable { bitrate_bps: 2_000_000 }));
    assert!(matches!(io_error, HlsBandwidthPersistenceState::RetryAfter { .. }));
}

#[tokio::test]
async fn hls_runtime_bandwidth_persistence_is_entry_gated_and_deduplicated() {
    let temp = tempfile::tempdir().expect("temp dir");
    let app_state = test_app_state();
    let current_config = app_state.app_config.config.load();
    app_state.app_config.config.store(Arc::new(Config {
        storage_dir: temp.path().to_string_lossy().into_owned(),
        ..current_config.as_ref().clone()
    }));
    let input = ConfigInput {
        id: 7,
        name: Arc::from("runtime-bandwidth-input"),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let storage_path = crate::repository::build_input_storage_path(&input.name, temp.path().to_string_lossy().as_ref());
    std::fs::create_dir_all(&storage_path).expect("input storage");
    let item = test_m3u_hls_item(&input, 12345, "channel-a", "http://origin.test/live.m3u8");
    let mut tree = crate::repository::BPlusTree::new();
    tree.insert(Arc::clone(&item.provider_id), item);
    tree.store(&crate::repository::get_input_m3u_playlist_file_path(&storage_path, &input.name)).expect("input tree");

    let origin_source = super::super::build_hls_origin_source(&input, "channel-a");
    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_source_and_outcome(
            origin_source.session_key(),
            origin_source,
            &app_state.get_encrypt_secret(),
            1,
        )
        .await;
    {
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n\
                 #EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n",
        );
        let mut session_guard = session.write().await;
        session_guard.apply_origin_manifest(&manifest).expect("runtime learning timeline");
        for entry in session_guard.segments.values_mut() {
            entry.status = SegmentCacheStatus::Ready { content_length: 1_000_000, ready_at_ms: 2 };
        }
    }

    assert!(super::super::spawn_hls_runtime_bandwidth_persistence(
        &app_state,
        &session,
        super::super::HlsRuntimeBandwidthLearningContext::Disabled,
    )
    .is_none());
    assert_eq!(
        crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
            .await
            .expect("unknown bitrate read"),
        None
    );

    let task = super::super::spawn_hls_runtime_bandwidth_persistence(
        &app_state,
        &session,
        super::super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
    )
    .expect("runtime persistence task");
    task.await.expect("runtime persistence completion");

    assert_eq!(
        crate::repository::load_input_live_bitrate_bps(&app_state.app_config, &input, "channel-a")
            .await
            .expect("persisted bitrate read"),
        Some(2_000_000)
    );
    assert!(super::super::spawn_hls_runtime_bandwidth_persistence(
        &app_state,
        &session,
        super::super::HlsRuntimeBandwidthLearningContext::Eligible(&input),
    )
    .is_none());
}

#[test]
fn hls_master_playlist_response_diagnostic_reports_complete_response_contract_for_every_bandwidth_source() {
    for selection in [
        super::super::HlsMasterBandwidthSelection::resolve(Some(2_500_000), Some(2_000_000)),
        super::super::HlsMasterBandwidthSelection::resolve(None, Some(2_000_000)),
        super::super::HlsMasterBandwidthSelection::resolve(None, None),
    ] {
        let rendered = super::super::hls_entry_master_playlist_response(
            &ProxySessionId("diagnostic-proxy-session".to_string()),
            &HlsAccessLeaseId("diagnostic-lease".to_string()),
            selection.bandwidth(),
            Some("/iptv"),
        );
        let fields = super::super::HlsMasterPlaylistResponseDiagnostic {
            lease: "lease-safe".to_string(),
            session: "session-safe".to_string(),
            proxy_session: "proxy-safe".to_string(),
            user_session: "user-safe".to_string(),
            virtual_id: 12345,
            bandwidth_bps: selection.bandwidth().advertised_bps(),
            bandwidth_source: selection.source().as_log_value(),
            content_length: rendered.content_length,
        }
        .to_string();

        assert!(fields.contains("lease=lease-safe session=session-safe proxy_session=proxy-safe"));
        assert!(fields.contains("user_session=user-safe virtual_id=12345"));
        assert!(fields.contains(&format!(
            "bandwidth_bps={} bandwidth_source={}",
            selection.bandwidth().advertised_bps(),
            selection.source().as_log_value()
        )));
        assert!(fields.contains("status=200"));
        assert!(fields.ends_with(&format!("content_length={}", rendered.content_length)));
        assert_eq!(
            rendered
                .response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok()),
            Some(rendered.content_length)
        );
    }
}

#[test]
fn hls_entry_stream_context_keeps_only_positive_live_bitrate() {
    let input = ConfigInput { name: Arc::from("m3u-input"), ..ConfigInput::default() };
    let mut item = test_m3u_hls_item(&input, 1001, "channel-a", "http://origin.example.com/live.m3u8");
    item.additional_properties = Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
        bitrate: 2_500_000,
        ..shared::model::LiveStreamProperties::default()
    })));

    let context = super::super::HlsEntryStreamContext::from_playlist_item(&item).expect("entry stream context");
    assert_eq!(context.virtual_id(), 1001);
    assert_eq!(context.stream_ref(), "channel-a");
    assert_eq!(context.known_bitrate_bps(), Some(2_500_000));

    if let Some(StreamProperties::Live(properties)) = item.additional_properties.as_mut() {
        properties.bitrate = 0;
    }
    assert_eq!(
        super::super::HlsEntryStreamContext::from_playlist_item(&item)
            .expect("entry stream context without bitrate")
            .known_bitrate_bps(),
        None
    );
}

#[tokio::test]
async fn hls_cache_entry_prefers_item_bitrate_then_loads_db_without_target_rebuild() {
    let temp = tempfile::tempdir().expect("temp dir");
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut config = app_state.app_config.config.load().as_ref().clone();
    config.storage_dir = temp.path().to_string_lossy().into_owned();
    app_state.app_config.config.store(Arc::new(config));
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input =
        ConfigInput { id: 9, name: Arc::from("m3u-input"), input_type: InputType::M3u, ..ConfigInput::default() };
    let mut stored_item = test_m3u_hls_item(&input, 70001, "channel-a", "http://origin.example.com/live.m3u8");
    stored_item.additional_properties = Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
        bitrate: 2_500_000,
        ..shared::model::LiveStreamProperties::default()
    })));
    let input_storage = crate::repository::build_input_storage_path(&input.name, &temp.path().to_string_lossy());
    std::fs::create_dir_all(&input_storage).expect("input storage");
    let db_path = crate::repository::get_input_m3u_playlist_file_path(&input_storage, &input.name);
    let mut tree = crate::repository::BPlusTree::new();
    tree.insert(Arc::<str>::from("channel-a"), stored_item);
    tree.store(&db_path).expect("input M3U metadata");
    let request_url = "http://origin.example.com/live.m3u8";

    let db_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        super::super::build_hls_origin_source(&input, "channel-a"),
        70001,
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
    let (db_bandwidth, db_variant_uri) = single_variant_master_playlist(db_response).await;
    assert_eq!(db_bandwidth, 3_000_000);
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&db_variant_uri).to_string());
    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&db_variant_uri).to_string());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("DB-backed access lease");
    assert_eq!(lease.known_bitrate_bps, Some(2_500_000));

    std::fs::write(&db_path, b"invalid metadata tree").expect("corrupt test metadata");
    let item_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint_with_addr(test_addr_with_port(55127)),
        &user,
        super::super::build_hls_origin_source(&input, "channel-a"),
        70001,
        None,
        Some(3_000_000),
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let (item_bandwidth, _) = single_variant_master_playlist(item_response).await;
    assert_eq!(item_bandwidth, 3_600_000);

    let fallback_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint_with_addr(test_addr_with_port(55128)),
        &user,
        super::super::build_hls_origin_source(&input, "channel-a"),
        70001,
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
    let (fallback_bandwidth, _) = single_variant_master_playlist(fallback_response).await;
    assert_eq!(fallback_bandwidth, 1_000_000);
}
