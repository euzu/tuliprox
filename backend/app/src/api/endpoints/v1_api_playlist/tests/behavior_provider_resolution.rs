use super::{behavior::spawn_stalker_mock_server, resolve_provider_url_for_request, test_app_config, test_app_state};
use crate::model::{
    AppConfig, Config, ConfigInput, ConfigInputOptions, ConfigInputUpdateQuality, ConfigProvider, ConfigSource,
    ConfigTarget, SourcesConfig,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use shared::{
    foundation::Filter,
    model::{
        provider_saturation::build_group_lookup, ConfigPaths, ConfigProviderDto, InputType, PlaylistRequest,
        ProcessingOrder, XtreamCluster,
    },
    utils::Internable,
};
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

#[test]
fn resolve_provider_url_for_input_request_rewrites_provider_scheme() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input, source);
    let resolved = resolve_provider_url_for_request(
        &app_config,
        &PlaylistRequest::Input("input".to_string()),
        "provider://demo/live/user/pass/1359.ts",
    );

    assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
}

#[test]
fn resolve_provider_url_for_target_request_rewrites_provider_scheme() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let target = Arc::new(ConfigTarget {
        id: 11,
        enabled: true,
        name: "target".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![],
        rename: None,
        mapping_ids: None,
        mapping: Arc::default(),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
        curation: None,
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
    let app_config = test_app_config(input, source);
    let resolved = resolve_provider_url_for_request(
        &app_config,
        &PlaylistRequest::Target(11),
        "provider://demo/live/user/pass/1359.ts",
    );

    assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
}

#[test]
fn resolve_provider_url_passthrough_for_unresolved_provider_input_request() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(input, source);
    let original = "provider://unknown/live/user/pass/1359.ts";
    let resolved =
        resolve_provider_url_for_request(&app_config, &PlaylistRequest::Input("input".to_string()), original);

    assert_eq!(resolved, original);
}

#[test]
fn resolve_provider_url_passthrough_for_unresolved_provider_target_request() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "demo".intern(),
        urls: vec!["http://provider.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "input".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..Default::default()
    });
    let target = Arc::new(ConfigTarget {
        id: 11,
        enabled: true,
        name: "target".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![],
        rename: None,
        mapping_ids: None,
        mapping: Arc::default(),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
        curation: None,
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
    let app_config = test_app_config(input, source);
    let original = "provider://unknown/live/user/pass/1359.ts";
    let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

    assert_eq!(resolved, original);
}

#[test]
fn resolve_provider_url_passthrough_for_ambiguous_target_request() {
    let provider_a = ConfigProvider::from(&ConfigProviderDto {
        name: "shared".intern(),
        urls: vec!["http://provider-a.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let provider_b = ConfigProvider::from(&ConfigProviderDto {
        name: "shared".intern(),
        urls: vec!["http://provider-b.example".intern()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
        dns: None,
    });
    let input_a = Arc::new(ConfigInput {
        id: 7,
        name: "input-a".intern(),
        provider_configs: Some(vec![Arc::new(provider_a)]),
        ..Default::default()
    });
    let input_b = Arc::new(ConfigInput {
        id: 8,
        name: "input-b".intern(),
        provider_configs: Some(vec![Arc::new(provider_b)]),
        ..Default::default()
    });
    let target = Arc::new(ConfigTarget {
        curation: None,
        id: 11,
        enabled: true,
        name: "target".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![],
        rename: None,
        mapping_ids: None,
        mapping: Arc::default(),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });
    let source =
        ConfigSource { inputs: vec![Arc::clone(&input_a.name), Arc::clone(&input_b.name)], targets: vec![target] };
    let inputs = vec![input_a, input_b];
    let sources = SourcesConfig {
        batch_files: vec![],
        provider: vec![],
        group_lookup: build_group_lookup(&inputs),
        inputs,
        sources: vec![source],
        templates: None,
    };

    let app_config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::empty()),
        api_proxy: Arc::new(ArcSwapOption::empty()),
        file_locks: Arc::new(crate::utils::FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
    };

    let original = "provider://shared/live/user/pass/1359.ts";
    let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

    assert_eq!(resolved, original);
}

#[test]
fn build_playlist_webplayer_url_uses_cluster_stream_type() {
    let live =
        super::super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Live);
    let movie =
        super::super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Video);
    let series =
        super::super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Series);

    assert_eq!(live, "http://player.example/api/v1/playlist/webplayer/token123/1/live/42");
    assert_eq!(movie, "http://player.example/api/v1/playlist/webplayer/token123/1/movie/42");
    assert_eq!(series, "http://player.example/api/v1/playlist/webplayer/token123/1/series/42");
}

#[tokio::test]
async fn playlist_live_input_route_supports_stalker_inputs() {
    let temp_dir = tempdir().expect("temp dir");
    let (base_url, server_handle) = spawn_stalker_mock_server().await;
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "stalker-preview".intern(),
        input_type: InputType::Stalker,
        url: base_url,
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: crate::model::ConfigInputFlagsSet::new(),
            update_quality: ConfigInputUpdateQuality::default(),
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
            flussonic_hls_catchup: shared::model::FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: shared::model::default_flussonic_hls_catchup_max_duration_secs(),
        }),
        stalker: Some(crate::model::StalkerInputConfig {
            device: None,
            auth_mode: shared::model::StalkerAuthMode::Auto,
            mag_preset: shared::model::StalkerMagPreset::GenericSafe,
            endpoint_preference: shared::model::StalkerEndpointPreference::ServerLoad,
            size_caps: None,
            catalog_max_pages: None,
            ..Default::default()
        }),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(Arc::clone(&input), source);
    let mut sources = app_config.sources.load().as_ref().clone();
    sources.inputs.push(Arc::new(ConfigInput {
        id: 8,
        name: "m3u".intern(),
        input_type: InputType::M3u,
        ..Default::default()
    }));
    app_config.sources.store(Arc::new(sources));
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let app_state = test_app_state(Arc::new(app_config));
    let router =
        super::super::v1_api_playlist_register_protected(super::super::v1_api_playlist_register_public(Router::new()))
            .with_state(Arc::clone(&app_state));
    let request = Request::builder()
        .method("POST")
        .uri("/playlist/live")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"Input":"stalker-preview"}"#))
        .expect("request");

    let response = router.clone().into_service::<Body>().oneshot(request).await.expect("response");

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let body_text = String::from_utf8(body.to_vec()).expect("utf8");
    assert_eq!(status, StatusCode::OK, "{body_text}");
    assert!(body_text.contains("Demo Channel"), "{body_text}");
    assert!(!body_text.contains("ffmpeg http://streams.example/live/101"), "{body_text}");
    let body_json: serde_json::Value = serde_json::from_str(&body_text).unwrap_or_default();
    let playback_url = body_json
        .as_array()
        .and_then(|items| items.first())
        .and_then(serde_json::Value::as_array)
        .and_then(|item| item.get(6))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(!playback_url.is_empty(), "{body_text}");
    assert!(playback_url.starts_with("http") || playback_url.starts_with('/'), "{playback_url}");

    let resource_path = playback_url
        .find("/playlist/resource/")
        .map(|index| &playback_url[index..])
        .expect("Stalker playback resource path");
    let response = router
        .clone()
        .into_service::<Body>()
        .oneshot(Request::get(resource_path).body(Body::empty()).expect("resource request"))
        .await
        .expect("resource response");
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT, "{resource_path}");
    assert_eq!(
        response.headers().get("location").and_then(|value| value.to_str().ok()),
        Some("http://8.8.8.8/live/101")
    );

    let response = router
        .clone()
        .into_service::<Body>()
        .oneshot(Request::get("/playlist/resource/*").body(Body::empty()).expect("malformed request"))
        .await
        .expect("malformed response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let legacy = shared::utils::obfuscate_text(&app_state.get_encrypt_secret(), "http://10.0.0.1/icon.png");
    let response = router
        .clone()
        .into_service::<Body>()
        .oneshot(
            Request::get(format!("/playlist/resource/{legacy}")).body(Body::empty()).expect("legacy token request"),
        )
        .await
        .expect("legacy token response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    for locator in [
        format!("{}99/live/101", crate::api::endpoints::api_playlist_utils::STALKER_RESOURCE_SCHEME),
        format!("{}8/live/101", crate::api::endpoints::api_playlist_utils::STALKER_RESOURCE_SCHEME),
    ] {
        let resource = shared::utils::seal_web_ui_resource_url(&app_state.get_encrypt_secret(), &locator);
        let response = router
            .clone()
            .into_service::<Body>()
            .oneshot(
                Request::get(format!("/playlist/resource/{resource}")).body(Body::empty()).expect("not-found request"),
            )
            .await
            .expect("not-found response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{locator}");
    }

    let resource = shared::utils::seal_web_ui_resource_url(
        &app_state.get_encrypt_secret(),
        &format!("{}7/live/102", crate::api::endpoints::api_playlist_utils::STALKER_RESOURCE_SCHEME),
    );
    let response = router
        .into_service::<Body>()
        .oneshot(
            Request::get(format!("/playlist/resource/{resource}"))
                .body(Body::empty())
                .expect("private destination request"),
        )
        .await
        .expect("private destination response");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    server_handle.abort();
}

/// The playback endpoints resolve a Stalker item from the catalog through the item's own
/// provider id. This is the path behind `Failed to resolve initial Stalker playback URL`,
/// so it needs the real id -> item -> `create_link` chain against a portal, not just the
/// Web UI preview route.
#[tokio::test]
async fn initial_stalker_playback_resolution_uses_the_requested_items_own_command() {
    let temp_dir = tempdir().expect("temp dir");
    let (base_url, server_handle) = spawn_stalker_mock_server().await;
    let input = Arc::new(ConfigInput {
        id: 7,
        name: "stalker-playback".intern(),
        input_type: InputType::Stalker,
        url: base_url,
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: crate::model::ConfigInputFlagsSet::new(),
            update_quality: ConfigInputUpdateQuality::default(),
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
            flussonic_hls_catchup: shared::model::FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: shared::model::default_flussonic_hls_catchup_max_duration_secs(),
        }),
        stalker: Some(crate::model::StalkerInputConfig {
            device: None,
            auth_mode: shared::model::StalkerAuthMode::Auto,
            mag_preset: shared::model::StalkerMagPreset::GenericSafe,
            endpoint_preference: shared::model::StalkerEndpointPreference::ServerLoad,
            size_caps: None,
            catalog_max_pages: None,
            ..Default::default()
        }),
        ..Default::default()
    });
    let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
    let app_config = test_app_config(Arc::clone(&input), source);
    app_config
        .config
        .store(Arc::new(Config { storage_dir: temp_dir.path().to_string_lossy().to_string(), ..Default::default() }));
    let app_state = test_app_state(Arc::new(app_config));
    let router =
        super::super::v1_api_playlist_register_protected(super::super::v1_api_playlist_register_public(Router::new()))
            .with_state(Arc::clone(&app_state));

    // The catalog import publishes the generation the playback path reads.
    let response = router
        .clone()
        .into_service::<Body>()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/playlist/live")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"Input":"stalker-playback"}"#))
                .expect("request"),
        )
        .await
        .expect("catalog response");
    assert_eq!(response.status(), StatusCode::OK);

    let live_input = app_state.app_config.get_input_by_name(&"stalker-playback".intern()).expect("stalker input");
    let unresolved: Arc<str> = "".intern();
    let live = shared::model::PlaylistItemType::Live;
    let resolve = |provider_id: u32| {
        let app_state = Arc::clone(&app_state);
        let live_input = Arc::clone(&live_input);
        let unresolved = Arc::clone(&unresolved);
        async move {
            crate::api::api_utils::resolve_initial_stalker_playback_url(
                &app_state,
                &live_input,
                provider_id,
                XtreamCluster::Live,
                live,
                &unresolved,
            )
            .await
        }
    };

    // An id that is not in the catalog must not resolve, and must not invalidate the
    // published generation the next request reads.
    let err = resolve(999_999).await.expect_err("an unknown provider id cannot be resolved");
    assert!(err.to_string().contains("999999"), "{err}");

    // Each item resolves through its own stored cmd: the mock portal answers a different
    // destination per cmd, so a provider id that picked the wrong item would surface here.
    assert_eq!(
        resolve(101).await.expect("item 101 resolves").as_ref(),
        "http://8.8.8.8/live/101",
        "the resolved url must come from item 101's own cmd"
    );
    assert_eq!(
        resolve(102).await.expect("item 102 resolves").as_ref(),
        "http://127.0.0.1/live/102",
        "the resolved url must come from item 102's own cmd"
    );

    // An item that already carries a url is served as is.
    let resolved_url: Arc<str> = "http://stream.example/already-resolved.ts".intern();
    let passthrough = crate::api::api_utils::resolve_initial_stalker_playback_url(
        &app_state,
        &live_input,
        101,
        XtreamCluster::Live,
        live,
        &resolved_url,
    )
    .await
    .expect("a resolved url is passed through");
    assert!(Arc::ptr_eq(&passthrough, &resolved_url));

    server_handle.abort();
}
