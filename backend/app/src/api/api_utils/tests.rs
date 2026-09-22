use super::*;
use crate::{
    api::model::{
        empty_resource_client_set, ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager,
        EventManager, MetadataUpdateManager, PlaylistStorageState, ProviderConfig as RuntimeProviderConfig,
        ProviderConfigConnection, ResourceClientSet, SharedStreamManager,
    },
    auth::Fingerprint,
    model::{
        AppConfig, Config, ConfigInput, ConfigInputAlias, ConfigProvider, ConfigTarget, GracePeriodOptions,
        MediaToolCapabilities, NetworkAccess, ProcessTargets, ProxyUserCredentials, SourcesConfig, StreamHistoryConfig,
    },
    repository::GeoIp,
    utils::FileLockManager,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, Response, StatusCode},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures::stream;
use http_body_util::BodyExt;
use shared::{
    defaults::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs},
    foundation::Filter,
    model::{
        provider_saturation::build_group_lookup, AdmissionStrategy, ClusterFlags, ConfigPaths, ConfigProviderDto,
        ConfigTargetOptions, GeoIpUnavailablePolicy, InputFetchMethod, InputType, PlaylistItem, PlaylistItemHeader,
        PlaylistItemType, ProcessingOrder, ProviderUrlSelectionPolicy, ProxyType, ResourcePolicyDto, StreamChannel,
        TargetType, XtreamCluster,
    },
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, RwLock},
};
use tuliprox_core::{
    model::{public_only_policy, ResourceClientKey, ResourcePolicy, ResourcePolicyError, ResourceRedirectMode},
    utils::{resource_cache_key, response_compression::should_compress_response},
};
use tuliprox_session::{
    admission::{evaluate_remaining_strategies_after_grace, get_effective_admission_strategies},
    AdmissionRejectionReason, GraceResolutionContext,
};

#[test]
fn stalker_playback_refreshes_invalid_or_rejected_urls() {
    assert!(should_refresh_stalker_playback(InputType::Stalker, false, None));
    assert!(should_refresh_stalker_playback(InputType::Stalker, true, Some(StatusCode::UNAUTHORIZED)));
    assert!(!should_refresh_stalker_playback(InputType::Stalker, true, Some(StatusCode::OK)));
    assert!(!should_refresh_stalker_playback(InputType::Xtream, false, None));
}

#[test]
fn initial_stalker_playback_resolves_only_empty_urls() {
    assert!(needs_initial_stalker_resolution(InputType::Stalker, ""));
    assert!(!needs_initial_stalker_resolution(InputType::Stalker, "https://stream.example/live.ts"));
    assert!(!needs_initial_stalker_resolution(InputType::Xtream, ""));
    assert_eq!(stalker_stream_kind(XtreamCluster::Live, PlaylistItemType::Catchup), StalkerStreamKind::Archive);
}

#[test]
fn stream_admission_rejection_is_503_without_upstream_headers() {
    use crate::api::model::StreamAdmissionError;

    for error in [
        StreamAdmissionError::CleanupReceiverClosed,
        StreamAdmissionError::CleanupAdmissionTimeout,
        StreamAdmissionError::RegistrationRejected,
    ] {
        let response = stream_admission_rejected_response(error, "test-user");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get("content-length").is_none(), "must not leak upstream content-length");
        assert!(response.headers().get("content-range").is_none(), "must not leak upstream content-range");
    }
}

fn test_runtime_provider(url: &str, username: &str, password: &str) -> Arc<RuntimeProviderConfig> {
    test_runtime_provider_with_type(url, username, password, InputType::Xtream)
}

#[tokio::test]
async fn streamed_json_array_coalesces_small_entries() {
    let response = stream_json_array_stream(stream::iter(0..4_096u32));
    let mut body = response.into_body();
    let mut frames = 0usize;
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return;
        };
        if let Ok(data) = frame.into_data() {
            frames += 1;
            bytes.extend_from_slice(&data);
        }
    }
    assert!(frames <= 2, "small JSON entries should be coalesced, got {frames} frames");
    let decoded = serde_json::from_slice::<Vec<u32>>(&bytes);
    assert!(decoded.is_ok_and(|values| values.len() == 4_096));
}

#[tokio::test]
async fn catchup_payload_probe_detects_fragmented_hls() {
    let source =
        stream::iter([Ok::<_, StreamError>(Bytes::from_static(b"#EX")), Ok(Bytes::from_static(b"TM3U\nsegment.ts\n"))])
            .boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::HlsManifest(_))));
    if let Ok(CatchupPayload::HlsManifest(manifest)) = result {
        assert_eq!(manifest, b"#EXTM3U\nsegment.ts\n".as_slice());
    }
}

#[tokio::test]
async fn catchup_payload_probe_replays_ts_bytes() {
    let expected = Bytes::from_static(b"\x47direct-ts-payload");
    let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
    if let Ok(CatchupPayload::Direct(mut stream)) = result {
        let mut actual = Vec::new();
        while let Some(chunk) = stream.next().await {
            if let Ok(chunk) = chunk {
                actual.extend_from_slice(&chunk);
            }
        }
        assert_eq!(actual, expected.as_ref());
    }
}

#[tokio::test]
async fn catchup_payload_probe_replays_partial_signature_at_eof() {
    let expected = Bytes::from_static(b"#EXT");
    let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
    if let Ok(CatchupPayload::Direct(mut stream)) = result {
        let actual = stream.next().await.and_then(Result::ok);
        assert_eq!(actual.as_ref(), Some(&expected));
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test]
async fn catchup_payload_probe_rejects_oversized_manifest() {
    let oversized = vec![b'x'; MAX_HLS_MANIFEST_BYTES];
    let source =
        stream::iter([Ok::<_, StreamError>(Bytes::from_static(b"#EXTM3U")), Ok(Bytes::from(oversized))]).boxed();

    let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn coalesced_stream_remains_finished_when_polled_again() {
    let stream = coalesce_byte_stream(stream::empty::<Result<Bytes, ()>>());
    futures::pin_mut!(stream);

    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
}

fn test_runtime_provider_with_type(
    url: &str,
    username: &str,
    password: &str,
    input_type: InputType,
) -> Arc<RuntimeProviderConfig> {
    let url = if input_type == InputType::M3u {
        format!("{url}/playlist.m3u8?username={username}&password={password}")
    } else {
        url.to_string()
    };
    let input = ConfigInput {
        name: "provider".intern(),
        url,
        username: Some(username.to_string()),
        password: Some(password.to_string()),
        input_type,
        ..ConfigInput::default()
    };
    Arc::new(RuntimeProviderConfig::new(
        &input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ))
}

fn test_runtime_provider_without_credentials(url: &str, input_type: InputType) -> Arc<RuntimeProviderConfig> {
    let input = ConfigInput { name: "provider".intern(), url: url.to_string(), input_type, ..ConfigInput::default() };
    Arc::new(RuntimeProviderConfig::new(
        &input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ))
}

#[test]
fn test_is_seek_request() {
    let mut headers = HeaderMap::new();

    // No range header
    assert!(!is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=0- (Should be true now to allow session takeover on restart)
    headers.insert("range", "bytes=0-".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=100- (Should be true)
    headers.insert("range", "bytes=100-".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=100-200 (Should be true)
    headers.insert("range", "bytes=100-200".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Live cluster should always return false
    headers.insert("range", "bytes=100-".parse().unwrap());
    assert!(!is_seek_request(XtreamCluster::Live, &headers));
}

#[test]
fn hls_manifests_are_not_forced_as_seek_responses() {
    let mut headers = HeaderMap::new();
    headers.insert("range", HeaderValue::from_static("bytes=0-"));

    assert!(!is_seekable_media_request(XtreamCluster::Video, &headers, Some(HLS_EXT)));
    assert!(is_seekable_media_request(XtreamCluster::Video, &headers, Some(".ts")));
}

#[test]
fn resolve_redirect_location_resolves_provider_scheme_urls() {
    let provider = ConfigProvider::from(&ConfigProviderDto {
        name: "develop".intern(),
        urls: vec!["https://provider.example".intern()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    });
    let input = ConfigInput {
        name: "provider".intern(),
        provider_configs: Some(vec![Arc::new(provider)]),
        ..ConfigInput::default()
    };

    let resolved =
        resolve_redirect_location(Some(&input), "provider://develop/live/provider-user/provider-pass/33486.m3u8")
            .expect("provider url should resolve");

    assert_eq!(resolved, "https://provider.example/live/provider-user/provider-pass/33486.m3u8");
}

#[test]
fn stream_alternative_url_keeps_unmatched_urls_unchanged() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://source.example".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
    let stream_url = "http://other.example/live/source-user/source-pass/123.ts";

    let rewritten = get_stream_alternative_url(stream_url, &input, &alias);

    assert_eq!(rewritten, None);
}

#[test]
fn stream_alternative_url_rewrites_only_query_auth_fields() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://source.example".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
    let stream_url = "http://source.example/player?token=source-user&username=source-user&password=source-pass";

    let rewritten = get_stream_alternative_url(stream_url, &input, &alias);

    assert_eq!(
        rewritten,
        Some("http://alias.example/player?token=source-user&username=alias-user&password=alias-pass".to_string())
    );
}

#[test]
fn stream_url_matches_provider_requires_base_url_and_account_identity() {
    let provider = test_runtime_provider("http://same.example", "selected-user", "selected-pass");

    assert!(stream_url_matches_provider("http://same.example/live/selected-user/selected-pass/123.ts", &provider));
    assert!(stream_url_matches_provider(
        "http://same.example/timeshift/selected-user/selected-pass/30/2026-06-15:20-00/123.ts",
        &provider
    ));
    assert!(stream_url_matches_provider(
        "http://same.example/future-route/selected-user/selected-pass/opaque/123.ts",
        &provider
    ));
    assert!(!stream_url_matches_provider("http://same.example/live/other-user/other-pass/123.ts", &provider));
    assert!(!stream_url_matches_provider(
        "http://same.example/timeshift/other-user/other-pass/30/2026-06-15:20-00/123.ts",
        &provider
    ));
    assert!(!stream_url_matches_provider(
        "http://same.example/future-route/other-user/other-pass/opaque/123.ts",
        &provider
    ));
}

#[test]
fn stream_url_matches_provider_accepts_external_playlist_url_for_m3u_without_account_signature() {
    let provider =
        test_runtime_provider_with_type("http://provider.example", "selected-user", "selected-pass", InputType::M3u);

    assert!(stream_url_matches_provider(
        "https://hlspackager.akamaized.net/live/DB/ALYAUM_TV/HLS/ALYAUM_TV.m3u8",
        &provider
    ));
    assert!(stream_url_matches_provider(
        "https://shd-gcp-live.edgenextcdn.net/live/bitmovin-mbc-1/15cf99af5de54063fdabfefe66adc075/index.m3u8",
        &provider
    ));
}

#[test]
fn stream_url_matches_provider_rejects_external_cdn_url_with_wrong_account_signature() {
    let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

    assert!(!stream_url_matches_provider("http://cdn.example/live/other-user/other-pass/123.ts", &provider));
    assert!(!stream_url_matches_provider(
        "http://cdn.example/segment.ts?username=other-user&password=other-pass",
        &provider
    ));
}

#[test]
fn stream_url_matches_provider_rejects_external_cdn_url_with_wrong_account_signature_for_m3u() {
    let provider =
        test_runtime_provider_with_type("http://provider.example", "selected-user", "selected-pass", InputType::M3u);

    assert!(!stream_url_matches_provider(
        "http://cdn.example/segment.ts?username=other-user&password=other-pass",
        &provider
    ));
}

#[test]
fn stream_url_matches_provider_detects_m3u_path_credentials_against_alias_account() {
    // Regression: a cross-host M3U URL whose path embeds the alias's
    // account credentials must be detected as an account signature and
    // validated, not silently allowed as an open URL.
    let provider =
        test_runtime_provider_with_type("http://provider.example", "selected-user", "selected-pass", InputType::M3u);

    // Matching path credentials -> allowed (account matches).
    assert!(stream_url_matches_provider("http://cdn.example/live/selected-user/selected-pass/123.ts", &provider));
}

#[test]
fn stream_url_matches_provider_rejects_open_external_cdn_url_without_account_signature_for_xtream() {
    let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

    assert!(!stream_url_matches_provider("http://cdn.example/open/playlist.m3u8", &provider));
    assert!(!stream_url_matches_provider("http://cdn.example/open/segment.ts?key=signedopaque", &provider));
}

#[test]
fn stream_url_matches_provider_rejects_external_cdn_url_for_xtream_even_with_valid_account_signature() {
    let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

    assert!(!stream_url_matches_provider("http://cdn.example/live/selected-user/selected-pass/123.ts", &provider));
    assert!(!stream_url_matches_provider(
        "http://cdn.example/segment.ts?username=selected-user&password=selected-pass",
        &provider
    ));
}

#[test]
fn find_input_account_by_signature_matches_main_input_and_alias_accounts() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example".to_string(),
        username: Some("main-user".to_string()),
        password: Some("main-pass".to_string()),
        input_type: InputType::Xtream,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "alias".intern(),
            url: "http://alias.example".to_string(),
            username: Some("alias-user".to_string()),
            password: Some("alias-pass".to_string()),
            max_connections: 1,
            priority: 0,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    };

    let main = find_input_account_by_signature("http://cdn.example/live/main-user/main-pass/1.ts", &input);
    assert_eq!(
        main,
        Some(("http://provider.example".to_string(), Some("main-user".to_string()), Some("main-pass".to_string()),))
    );

    let alias = find_input_account_by_signature("http://cdn.example/live/alias-user/alias-pass/1.ts", &input);
    assert_eq!(
        alias,
        Some(("http://alias.example".to_string(), Some("alias-user".to_string()), Some("alias-pass".to_string()),))
    );

    assert_eq!(find_input_account_by_signature("http://cdn.example/live/other/other/1.ts", &input), None);
    assert_eq!(find_input_account_by_signature("http://cdn.example/open/playlist.m3u8", &input), None);
}

#[test]
fn get_stream_alternative_url_rewrites_external_cdn_url_with_valid_account_signature_for_alias_account() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
    let stream_url = "http://cdn.example/live/source-user/source-pass/123.ts";

    let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
    assert_eq!(rewritten, Some("http://cdn.example/live/alias-user/alias-pass/123.ts".to_string()));
}

#[test]
fn get_stream_alternative_url_rewrites_opaque_m3u_token_for_alias_account() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        input_type: InputType::M3u,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "alias".intern(),
            url: "http://playlist.example/b.m3u?token=provider-b-token".to_string(),
            username: None,
            password: None,
            max_connections: 0,
            priority: 0,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_without_credentials(
        "http://playlist.example/b.m3u?token=provider-b-token",
        InputType::M3u,
    );

    let rewritten =
        get_stream_alternative_url("http://stream.example/channel/segment.ts?token=provider-a-token", &input, &alias);

    assert_eq!(rewritten, Some("http://stream.example/channel/segment.ts?token=provider-b-token".to_string()));
}

#[test]
fn get_stream_alternative_url_rewrites_single_opaque_m3u_credential_with_different_key() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_without_credentials(
        "http://playlist.example/b.m3u?api_key=provider-b-key",
        InputType::M3u,
    );

    let rewritten = get_stream_alternative_url(
        "http://stream.example/segment.ts?token=provider-a-token&quality=hd",
        &input,
        &alias,
    );

    assert_eq!(rewritten, Some("http://stream.example/segment.ts?api_key=provider-b-key&quality=hd".to_string()));
}

#[test]
fn get_stream_alternative_url_rejects_ambiguous_cross_key_credential_mapping() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_without_credentials(
        "http://playlist.example/b.m3u?api_key=provider-b-key&auth=provider-b-auth",
        InputType::M3u,
    );

    assert_eq!(
        get_stream_alternative_url("http://stream.example/segment.ts?token=provider-a-token", &input, &alias),
        None
    );
}

#[test]
fn get_stream_alternative_url_rejects_partial_source_credential_mapping() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://playlist.example/a.m3u?token=provider-a-token&api_key=provider-a-key".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_without_credentials(
        "http://playlist.example/b.m3u?token=provider-b-token",
        InputType::M3u,
    );

    assert_eq!(
        get_stream_alternative_url(
            "http://stream.example/segment.ts?token=provider-a-token&api_key=provider-a-key&quality=hd",
            &input,
            &alias,
        ),
        None
    );
}

#[tokio::test]
async fn select_provider_stream_url_rewrites_opaque_m3u_token_for_allocated_alias() {
    let input = ConfigInput {
        name: "provider-a".intern(),
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        input_type: InputType::M3u,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider-b".intern(),
            url: "http://playlist.example/b.m3u?token=provider-b-token".to_string(),
            username: None,
            password: None,
            max_connections: 0,
            priority: 0,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_without_credentials(
        "http://playlist.example/b.m3u?token=provider-b-token",
        InputType::M3u,
    );

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let app_config = create_test_dual_provider_app_config();
    let mut config = (*app_config.config.load_full()).clone();
    config.storage_dir = temp.path().to_string_lossy().into_owned();
    app_config.config.store(Arc::new(config));
    let app_config = Arc::new(app_config);
    let selected = select_provider_stream_url(
        "http://stream.example/channel/segment.ts?token=provider-a-token",
        &input,
        &alias,
        false,
        &app_config,
    )
    .await;

    assert_eq!(
        selected,
        Some(("provider".intern(), "http://stream.example/channel/segment.ts?token=provider-b-token".to_string(),))
    );
}

#[tokio::test]
async fn select_provider_stream_url_uses_persisted_alias_url_for_independent_stream_token() {
    use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader};
    use tuliprox_repository::{get_input_m3u_playlist_file_path, get_input_storage_path, persist_input_m3u_playlist};

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let app_config = create_test_dual_provider_app_config();
    let mut config = (*app_config.config.load_full()).clone();
    config.storage_dir = temp.path().to_string_lossy().into_owned();
    app_config.config.store(Arc::new(config));
    let app_config = Arc::new(app_config);

    let input = ConfigInput {
        name: "primary-account".intern(),
        url: "http://playlist.example/list.m3u?access_key=primary-playlist-key".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias_input = ConfigInput {
        name: "backup-account".intern(),
        url: "http://playlist.example/list.m3u?access_key=backup-playlist-key".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = Arc::new(RuntimeProviderConfig::new(
        &alias_input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ));

    let storage_path = get_input_storage_path(&alias.name, &app_config.config.load().storage_dir)
        .await
        .expect("alias storage should be created");
    let playlist_path = get_input_m3u_playlist_file_path(&storage_path, &alias.name);
    let playlist = vec![PlaylistGroup {
        id: 1,
        title: "Live".intern(),
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                id: "channel-323".intern(),
                input_stream_id: "channel-323".intern(),
                url: "http://stream.example:4000/323/mono.m3u8?token=backup-stream-token".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        }],
        xtream_cluster: XtreamCluster::Live,
    }];
    persist_input_m3u_playlist(&app_config, &playlist_path, &playlist).await.expect("alias playlist should persist");

    let selected = select_provider_stream_url(
        "http://stream.example:4000/323/mono.m3u8?token=primary-stream-token",
        &input,
        &alias,
        false,
        &app_config,
    )
    .await;

    assert_eq!(
        selected,
        Some((
            "backup-account".intern(),
            "http://stream.example:4000/323/mono.m3u8?token=backup-stream-token".to_string(),
        ))
    );
}

#[test]
fn get_stream_alternative_url_rewrites_timeshift_path_credentials_for_alias_account() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
    let stream_url = "http://provider.example/timeshift/source-user/source-pass/30/2026-06-15:20-00/123.ts";

    let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
    assert_eq!(
        rewritten,
        Some("http://alias.example/timeshift/alias-user/alias-pass/30/2026-06-15:20-00/123.ts".to_string())
    );
}

#[test]
fn get_stream_alternative_url_rewrites_future_route_path_credentials_for_alias_account() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
    let stream_url = "http://provider.example/future-route/source-user/source-pass/opaque/123.ts";

    let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
    assert_eq!(rewritten, Some("http://alias.example/future-route/alias-user/alias-pass/opaque/123.ts".to_string()));
}

#[test]
fn get_stream_alternative_url_keeps_open_external_playlist_url_for_m3u() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
    let stream_url = "https://cnbc-live.akamaized.net/cnbc/master.m3u8";

    assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), Some(stream_url.to_string()));
}

#[test]
fn get_stream_alternative_url_keeps_open_external_multisegment_playlist_url_for_m3u() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
    let stream_url = "https://hnpsechtsc.turknet.ercdn.net/xpnvudnlsv/cnbc-e/cnbc-e.m3u8";

    assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), Some(stream_url.to_string()));
}

#[test]
fn get_stream_alternative_url_keeps_open_external_m3u_url_for_provider_without_credentials() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let provider = test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);
    let stream_url = "http://s.only4.tv/17113/video.m3u8?token=abc";

    assert_eq!(get_stream_alternative_url(stream_url, &input, &provider), Some(stream_url.to_string()));
}

#[test]
fn get_stream_alternative_url_rejects_query_credentials_for_provider_without_credentials() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let provider = test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);

    assert_eq!(
        get_stream_alternative_url(
            "http://cdn.example/segment.ts?username=other-user&password=other-pass",
            &input,
            &provider
        ),
        None
    );
}

#[test]
fn get_stream_alternative_url_rejects_basic_auth_credentials_for_provider_without_credentials() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8".to_string(),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let provider = test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);

    assert_eq!(get_stream_alternative_url("http://user:pass@cdn.example/segment.ts", &input, &provider), None);
}

#[test]
fn get_stream_alternative_url_rejects_external_m3u_url_with_unmatched_account_signature() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::M3u,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);

    assert_eq!(
        get_stream_alternative_url(
            "http://cdn.example/segment.ts?username=other-user&password=other-pass",
            &input,
            &alias,
        ),
        None
    );
}

#[test]
fn get_stream_alternative_url_does_not_passthrough_arbitrary_open_external_url_for_xtream() {
    let input = ConfigInput {
        name: "source".intern(),
        url: "http://provider.example".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
    let stream_url = "http://cdn.example/open/playlist.m3u8";

    assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), None);
}

#[test]
fn media_server_proxy_response_header_filter_drops_hop_by_hop_headers() {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(is_hop_by_hop_response_header(&HeaderName::from_static(name)));
    }
    assert!(!is_hop_by_hop_response_header(&header::CONTENT_TYPE));
}

#[test]
fn media_server_image_error_status_classifies_client_and_upstream_failures() {
    let parse_error = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
        .detail("media server image URL is missing required path parts");
    assert_eq!(media_server_image_error_status(&parse_error), StatusCode::BAD_REQUEST);

    let not_found = MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
        .detail("plex media-server image URL is missing image_path");
    assert_eq!(media_server_image_error_status(&not_found), StatusCode::NOT_FOUND);

    let upstream = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
        .detail("media-server image request failed");
    assert_eq!(media_server_image_error_status(&upstream), StatusCode::BAD_GATEWAY);
}

#[test]
fn media_server_playback_urls_are_proxy_only_redirect_guard_candidates() {
    let plex_input = ConfigInput { input_type: InputType::Plex, ..ConfigInput::default() };
    let emby_input = ConfigInput { input_type: InputType::Emby, ..ConfigInput::default() };
    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

    assert!(is_media_server_playback_url(
        &plex_input,
        "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
    ));
    assert!(is_media_server_playback_url(
        &m3u_input,
        "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
    ));
    assert!(is_media_server_playback_url(&plex_input, "https://plex.example/stream.mkv"));
    assert!(!is_media_server_playback_url(&emby_input, "https://emby.example/stream.mkv"));
    assert!(!is_media_server_playback_url(&m3u_input, "https://provider.example/stream.mkv"));
    assert!(!is_media_server_stream_ref_url("https://provider.example/stream.mkv"));
    assert!(is_media_server_stream_ref_url("media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"));
    assert_eq!(
        resolve_request_url_for_logging(
            &plex_input,
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
        )
        .as_ref(),
        "media-server://<redacted>"
    );
}

#[test]
fn test_resolve_request_url_for_logging_respects_sanitization_setting() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    struct SanitizeSettingGuard(bool);
    impl Drop for SanitizeSettingGuard {
        fn drop(&mut self) { shared::utils::set_sanitize_sensitive_info(self.0); }
    }

    let prev = shared::utils::is_sanitize_sensitive_info_enabled();
    let _restore = SanitizeSettingGuard(prev);

    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };
    let media_ref = "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fsecret_key";
    let standard_url = "http://provider.example/live/stream.m3u8";

    // 1. When sanitization is enabled:
    shared::utils::set_sanitize_sensitive_info(true);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, media_ref).as_ref(), "media-server://<redacted>");
    assert_eq!(resolve_request_url_for_logging(&m3u_input, standard_url).as_ref(), standard_url);

    // 2. When sanitization is disabled:
    shared::utils::set_sanitize_sensitive_info(false);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, media_ref).as_ref(), media_ref);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, standard_url).as_ref(), standard_url);
}

#[test]
fn test_streaming_response_extension_disables_compression() {
    let mut response = Response::new(());
    mark_response_as_uncompressed(&mut response);

    assert!(!should_compress_response(&response));
}

async fn spawn_legacy_hls_test_origin(
    response_head: String,
    response_body: Vec<u8>,
) -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("test origin accepts request");
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("test origin reads request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        socket.write_all(response_head.as_bytes()).await.expect("test origin writes response headers");
        socket.write_all(&response_body).await.expect("test origin writes response body");
        String::from_utf8_lossy(&request).into_owned()
    });
    (origin_addr, origin_task)
}

async fn spawn_range_aware_test_origin(
    body: &'static [u8],
    connections: usize,
) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..connections {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("test origin reads request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let request_lower = String::from_utf8_lossy(&request).to_ascii_lowercase();
            requests.push(request_lower.clone());

            let range = request_lower
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .and_then(|spec| spec.split_once('-'))
                .and_then(|(start, end)| {
                    let start: usize = start.trim().parse().ok()?;
                    let end: usize =
                        if end.trim().is_empty() { body.len().saturating_sub(1) } else { end.trim().parse().ok()? };
                    Some((start, end.min(body.len().saturating_sub(1))))
                })
                .unwrap_or((0, body.len().saturating_sub(1)));

            let slice = &body[range.0..=range.1];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp2t\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                range.0,
                range.1,
                body.len(),
                slice.len()
            );
            socket.write_all(head.as_bytes()).await.expect("test origin writes response headers");
            socket.write_all(slice).await.expect("test origin writes response body");
        }
        requests
    });
    (origin_addr, origin_task)
}

#[allow(dead_code)]
#[derive(Clone)]
enum FakeOriginMode {
    EmptyBody,
    BlockFirstChunk(Arc<tokio::sync::Notify>),
    ErrorBeforeFirstChunk,
    ErrorAfterFirstChunk(Vec<u8>),
    FixedBytes(Vec<u8>),
}

async fn spawn_controlled_fake_origin(
    mode: FakeOriginMode,
    max_requests: usize,
) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..max_requests {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            if request.is_empty() {
                continue;
            }
            let request_lower = String::from_utf8_lossy(&request).to_ascii_lowercase();
            requests.push(request_lower);

            match &mode {
                FakeOriginMode::EmptyBody => {
                    let head =
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::BlockFirstChunk(notify) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let mut probe = [0_u8; 1];
                    tokio::select! {
                        _ = notify.notified() => {},
                        _ = socket.read(&mut probe) => {},
                    }
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorBeforeFirstChunk => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorAfterFirstChunk(chunk) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let chunk_hdr = format!("{:x}\r\n", chunk.len());
                    let _ = socket.write_all(chunk_hdr.as_bytes()).await;
                    let _ = socket.write_all(chunk).await;
                    let _ = socket.write_all(b"\r\n").await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::FixedBytes(bytes) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(bytes).await;
                    let _ = socket.shutdown().await;
                }
            }
        }
        requests
    });
    (origin_addr, origin_task)
}

async fn forced_legacy_hls_test_response(
    origin_addr: SocketAddr,
    request_headers: &HeaderMap,
    client_port: u16,
) -> axum::response::Response {
    let origin_url = format!("http://{origin_addr}/segment.ts");
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        ..ConfigInput::default()
    });
    let app_config = create_test_provider_app_config();
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let client_addr = SocketAddr::from(([127, 0, 0, 1], client_port));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    let session = UserSession {
        token: format!("legacy-hls-marker-{client_port}"),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: origin_url.as_str().intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };
    let mut stream_channel = create_test_local_channel(&origin_url);
    stream_channel.provider_id = u32::from(input.id);
    stream_channel.input_name = Arc::clone(&input.name);
    stream_channel.item_type = PlaylistItemType::Catchup;
    stream_channel.cluster = XtreamCluster::Live;
    stream_channel.url = origin_url.as_str().intern();

    force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        ForceStreamRequestContext {
            req_headers: request_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response()
}

#[tokio::test]
async fn forced_hls_provider_response_disables_compression_and_streams_identity_bytes() {
    const IDENTITY_BODY: &[u8] = b"legacy hls identity segment";

    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    encoder.write_all(IDENTITY_BODY).await.expect("gzip test body encodes");
    encoder.shutdown().await.expect("gzip test encoder finishes");
    let encoded_body = encoder.into_inner();

    let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            encoded_body.len()
        );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, encoded_body).await;
    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_310).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!should_compress_response(&response));
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let body = response.into_body().collect().await.expect("legacy HLS response body").to_bytes();
    assert_eq!(body.as_ref(), IDENTITY_BODY);

    let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
}

#[tokio::test]
async fn forced_hls_unencoded_partial_response_preserves_range_and_disables_compression() {
    const PARTIAL_BODY: &[u8] = b"cdef";
    let response_head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp2t\r\nContent-Range: bytes 2-5/10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            PARTIAL_BODY.len()
        );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, PARTIAL_BODY.to_vec()).await;
    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-"));

    let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_311).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!should_compress_response(&response));
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    let body = response.into_body().collect().await.expect("legacy HLS partial body").to_bytes();
    assert_eq!(body.as_ref(), PARTIAL_BODY);

    let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
    assert!(request.contains("\r\nrange: bytes=2-\r\n"));
}

#[tokio::test]
async fn forced_reopen_stays_on_pinned_provider_account() {
    const ACCOUNT_A_BODY: &[u8] = b"account-a-marker";
    const ACCOUNT_B_BODY: &[u8] = b"account-b-marker";

    let head_a = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ACCOUNT_A_BODY.len()
    );
    let head_b = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ACCOUNT_B_BODY.len()
    );
    let (origin_a, task_a) = spawn_legacy_hls_test_origin(head_a, ACCOUNT_A_BODY.to_vec()).await;
    let (origin_b, task_b) = spawn_legacy_hls_test_origin(head_b, ACCOUNT_B_BODY.to_vec()).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a}"),
        username: Some("user-a".to_string()),
        password: Some("pass-a".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: format!("http://{origin_b}"),
            username: Some("user-b".to_string()),
            password: Some("pass-b".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let app_config = Arc::new(create_test_provider_app_config());
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(app_config);

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_400));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    let session = UserSession {
        token: "stickiness-token".to_string(),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_a}/live/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };
    let mut stream_channel = create_test_live_channel(&format!("http://{origin_a}/live/1.ts"));
    stream_channel.provider_id = 1;
    stream_channel.input_name = Arc::clone(&input.name);
    stream_channel.item_type = PlaylistItemType::Catchup;
    stream_channel.cluster = XtreamCluster::Live;
    stream_channel.url = session.stream_url.clone();

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.expect("stream body").to_bytes();
    assert_eq!(body.as_ref(), ACCOUNT_A_BODY, "forced reopen must stay on the pinned provider account A");

    let request_a = task_a.await.expect("origin A task completes").to_ascii_lowercase();
    assert!(!request_a.is_empty(), "origin A must have received the seek request");

    let request_b = tokio::time::timeout(std::time::Duration::from_millis(250), task_b).await;
    assert!(request_b.is_err(), "origin B must not receive a seek from an A-affine session");
}

#[tokio::test]
async fn overlapping_vod_range_requests_return_correct_account_bytes() {
    const VOD_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(VOD_BODY, 2).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_500));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/movie/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/movie/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Video;
        channel.cluster = XtreamCluster::Video;
        channel.url = format!("http://{origin_addr}/movie/1.ts").intern();
        channel
    };

    let build_request = |range: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static(range));
        let session = make_session(match range {
            "bytes=0-9" => "vod-range-1",
            _ => "vod-range-2",
        });
        (headers, session)
    };

    let (first_headers, first_session) = build_request("bytes=0-9");
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &first_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(first.headers().get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok()), Some("bytes 0-9/20"));
    let first_body = first.into_body().collect().await.expect("first range body").to_bytes();
    assert_eq!(first_body.as_ref(), &VOD_BODY[0..10]);

    let (second_headers, second_session) = build_request("bytes=5-14");
    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &second_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        second.headers().get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok()),
        Some("bytes 5-14/20")
    );
    let second_body = second.into_body().collect().await.expect("second range body").to_bytes();
    assert_eq!(second_body.as_ref(), &VOD_BODY[5..15]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 2, "both range requests must reach the same provider account");
    assert!(requests[0].contains("range: bytes=0-9"));
    assert!(requests[1].contains("range: bytes=5-14"));
}

#[tokio::test]
async fn parallel_series_range_requests_keep_both_claims_active() {
    const SERIES_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(SERIES_BODY, 2).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_501));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 43,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/series/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/series/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Series;
        channel.cluster = XtreamCluster::Series;
        channel.url = format!("http://{origin_addr}/series/1.ts").intern();
        channel
    };

    let first_headers = {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
        headers
    };
    let second_headers = {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=5-14"));
        headers
    };

    // Open both range requests before consuming either body, so both claims are
    // active at the same time. This exercises concurrent claims of one playback
    // rather than a sequential request/consume cycle.
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &make_session("series-range-1"),
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &make_session("series-range-2"),
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 2, "both range claims must be active concurrently");

    let first_body = first.into_body().collect().await.expect("first series range body").to_bytes();
    assert_eq!(first_body.as_ref(), &SERIES_BODY[0..10]);

    let second_body = second.into_body().collect().await.expect("second series range body").to_bytes();
    assert_eq!(second_body.as_ref(), &SERIES_BODY[5..15]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 2, "both series range requests must reach the same provider account");
    assert!(requests[0].contains("range: bytes=0-9"));
    assert!(requests[1].contains("range: bytes=5-14"));
}

async fn spawn_load_test_origin(body: &'static [u8]) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("load origin binds");
    let addr = listener.local_addr().expect("load origin address");
    let app = axum::Router::new().fallback(move || async move {
        ([(axum::http::header::CONTENT_TYPE, "video/mp2t")], Bytes::from_static(body))
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn load_test_latency_percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn validate_load_response(status: StatusCode, payload: &[u8], expected: &[u8]) -> Result<(), String> {
    if status != StatusCode::OK {
        return Err(format!("unexpected HTTP status {status}"));
    }
    if payload != expected {
        return Err(format!("payload mismatch: expected {} bytes, received {}", expected.len(), payload.len()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct SoakResourceSample {
    elapsed_secs: f64,
    rss_kib: Option<u64>,
    provider_connections: usize,
    lease_starting: usize,
    lease_active: usize,
    lease_idle: usize,
    user_requests: usize,
    user_sessions: usize,
    shared_origins: usize,
    shared_subscribers: usize,
    shared_meters: usize,
    running_tasks: usize,
    completed: u64,
    planned_aborted: u64,
    unexpected_failed: u64,
}

fn median_u64(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

#[cfg(target_os = "linux")]
fn load_test_rss_kib() -> Option<u64> {
    let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
        return None;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn load_test_rss_kib() -> Option<u64> { None }

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "HTTP load test: cargo +stable test -p tuliprox --release --bin tuliprox -- --ignored --nocapture http_provider_lease_load_test"]
async fn http_provider_lease_load_test() {
    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    // Warm-up so allocator and connection pools are exercised before measurement.
    for index in 0..32u16 {
        let addr = SocketAddr::from(([127, 0, 0, 1], 45_000 + index));
        let fingerprint = create_test_fingerprint(addr);
        let session = load_test_session(&input, origin_addr, &format!("warmup-{index}"), addr);
        let channel = load_test_channel(&input, origin_addr);
        let response = force_provider_stream_response(
            &fingerprint,
            &app,
            &session,
            channel,
            ForceStreamRequestContext {
                req_headers: &HeaderMap::new(),
                input: &input,
                user: &load_test_user(&format!("warmup-user-{index}")),
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response();
        let status = response.status();
        let body = response.into_body().collect().await.expect("warm-up body collects").to_bytes();
        validate_load_response(status, &body, &TS_PACKET).expect("warm-up response must match the fixture");
    }

    let baseline_rss_kib = load_test_rss_kib();
    eprintln!("HTTP provider lease load test (unlimited provider, real HTTP body, 5 repetitions per concurrency)");
    for concurrency in [1usize, 5, 50, 200] {
        let rounds_per_task = 20;
        let repetitions = 5;
        let mut all_samples = Vec::<(u64, u64)>::new();
        let mut total_ops = 0u64;
        let mut total_elapsed_secs = 0f64;
        for rep in 0..repetitions {
            let samples = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
            let started = std::time::Instant::now();
            let mut tasks = tokio::task::JoinSet::new();
            for task_index in 0..concurrency {
                let app = Arc::clone(&app);
                let input = Arc::clone(&input);
                let samples = Arc::clone(&samples);
                let username = format!("load-user-{rep}-{task_index}");
                tasks.spawn(async move {
                    for round in 0..rounds_per_task {
                        let addr = SocketAddr::from(([127, 0, 0, 1], 46_000 + (task_index as u16)));
                        let fingerprint = create_test_fingerprint(addr);
                        let token = format!("load-{rep}-{task_index}-{round}");
                        let session = load_test_session(&input, origin_addr, &token, addr);
                        let channel = load_test_channel(&input, origin_addr);
                        let user = load_test_user(&username);

                        let acquire_at = std::time::Instant::now();
                        let response = force_provider_stream_response(
                            &fingerprint,
                            &app,
                            &session,
                            channel,
                            ForceStreamRequestContext {
                                req_headers: &HeaderMap::new(),
                                input: &input,
                                user: &user,
                                session_reservation_ttl_secs: 0,
                                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                            },
                            None,
                        )
                        .await
                        .into_response();
                        let acquire_us = acquire_at.elapsed().as_micros() as u64;

                        let status = response.status();
                        let body = response.into_body();
                        let body_at = std::time::Instant::now();
                        let collected = body.collect().await.expect("load body collects").to_bytes();
                        validate_load_response(status, &collected, &TS_PACKET)
                            .expect("load response must match the TS packet");
                        let body_us = body_at.elapsed().as_micros() as u64;

                        samples.lock().unwrap().push((acquire_us, body_us));
                    }
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.expect("load task must not fail");
            }

            total_elapsed_secs += started.elapsed().as_secs_f64();
            let rep_samples = Arc::try_unwrap(samples).expect("samples unique").into_inner().unwrap();
            total_ops += rep_samples.len() as u64;
            all_samples.extend(rep_samples);
        }

        let mut acquire = Vec::with_capacity(all_samples.len());
        let mut body = Vec::with_capacity(all_samples.len());
        for (a, b) in all_samples {
            acquire.push(a);
            body.push(b);
        }
        acquire.sort_unstable();
        body.sort_unstable();
        let throughput = total_ops as f64 / total_elapsed_secs.max(f64::EPSILON);
        let byte_throughput = total_ops as f64 * TS_PACKET.len() as f64 / total_elapsed_secs.max(f64::EPSILON);
        let rss_delta_kib = load_test_rss_kib()
            .zip(baseline_rss_kib)
            .map(|(rss, baseline)| rss.saturating_sub(baseline).to_string())
            .unwrap_or_else(|| "unsupported".to_string());
        eprintln!(
            "concurrency={concurrency:>3} ops={total_ops:>5} throughput={throughput:>9.1} ops/s {byte_throughput:>11.1} bytes/s | acquire p50/p95/p99={}/{}/{}us | body p50/p95/p99={}/{}/{}us | rss_delta_kib={rss_delta_kib}",
            load_test_latency_percentile(&acquire, 0.50),
            load_test_latency_percentile(&acquire, 0.95),
            load_test_latency_percentile(&acquire, 0.99),
            load_test_latency_percentile(&body, 0.50),
            load_test_latency_percentile(&body, 0.95),
            load_test_latency_percentile(&body, 0.99),
        );
        let (user_requests, user_sessions) = app.active_users.playback_resource_counts().await;
        let (shared_origins, shared_subscribers) = app.shared_stream_manager.resource_counts().await;
        let shared_meters = app.shared_stream_manager.meter_count();
        let leases = app.active_provider.provider_lease_usage(&input.name);
        eprintln!(
            "[load resource] concurrency={concurrency} conns={} starting={} active={} idle={} user_requests={user_requests} user_sessions={user_sessions} shared_origins={shared_origins} shared_subscribers={shared_subscribers} shared_meters={shared_meters}",
            app.active_provider.get_provider_connections_count(),
            leases.starting,
            leases.active,
            leases.idle,
        );
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let leases = app.active_provider.provider_lease_usage(&input.name);
            let (user_requests, user_sessions) = app.active_users.playback_resource_counts().await;
            let (shared_origins, shared_subscribers) = app.shared_stream_manager.resource_counts().await;
            let shared_meters = app.shared_stream_manager.meter_count();
            if app.active_provider.get_provider_connections_count() == 0
                && leases.total() == 0
                && user_requests == 0
                && user_sessions == 0
                && shared_origins == 0
                && shared_subscribers == 0
                && shared_meters == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("load resources must drain to baseline");
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "provider slots must return to baseline");
    let usage = app.active_provider.provider_lease_usage(&input.name);
    assert_eq!(usage.total(), 0, "lease table must return to baseline after HTTP churn");
}

fn load_test_user(username: &str) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user
}

fn load_test_session(input: &ConfigInput, origin_addr: SocketAddr, token: &str, addr: SocketAddr) -> UserSession {
    UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr,
        socket_bound: false,
        active_addrs: vec![addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    }
}

fn load_test_channel(input: &ConfigInput, origin_addr: SocketAddr) -> StreamChannel {
    let mut channel = create_test_live_channel(&format!("http://{origin_addr}/live/42.ts"));
    channel.provider_id = u32::from(input.id);
    channel.input_name = Arc::clone(&input.name);
    channel.url = format!("http://{origin_addr}/live/42.ts").intern();
    channel
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "30-minute soak: TULIPROX_SOAK_SECS=60 cargo +stable test -p tuliprox --release --bin tuliprox -- --ignored --nocapture http_provider_lease_soak_test"]
async fn http_provider_lease_soak_test() {
    let duration_secs: u64 = std::env::var("TULIPROX_SOAK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800);
    const SOAK_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&SOAK_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let baseline_rss_kib = load_test_rss_kib();
    let completed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let aborted = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let unexpected_failed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let peak_connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let running_tasks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resource_samples = Arc::new(std::sync::Mutex::new(Vec::<SoakResourceSample>::new()));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(duration_secs);
    let sampler_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler_stop_task = Arc::clone(&sampler_stop);
    let app_sampler = Arc::clone(&app);
    let sampler_input_name = Arc::clone(&input.name);
    let sampler_completed = Arc::clone(&completed);
    let sampler_aborted = Arc::clone(&aborted);
    let sampler_failed = Arc::clone(&unexpected_failed);
    let sampler_running = Arc::clone(&running_tasks);
    let sampler_samples = Arc::clone(&resource_samples);
    let sampler_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let start_time = std::time::Instant::now();
        while !sampler_stop_task.load(std::sync::atomic::Ordering::Relaxed) {
            interval.tick().await;
            let rss = load_test_rss_kib().map_or_else(|| "unsupported".to_string(), |rss| rss.to_string());
            let conns = app_sampler.active_provider.get_provider_connections_count();
            let leases = app_sampler.active_provider.provider_lease_usage(&sampler_input_name);
            let (user_requests, user_sessions) = app_sampler.active_users.playback_resource_counts().await;
            let (shared_origins, shared_subscribers) = app_sampler.shared_stream_manager.resource_counts().await;
            let shared_meters = app_sampler.shared_stream_manager.meter_count();
            let sample = SoakResourceSample {
                elapsed_secs: start_time.elapsed().as_secs_f64(),
                rss_kib: load_test_rss_kib(),
                provider_connections: conns,
                lease_starting: leases.starting,
                lease_active: leases.active,
                lease_idle: leases.idle,
                user_requests,
                user_sessions,
                shared_origins,
                shared_subscribers,
                shared_meters,
                running_tasks: sampler_running.load(std::sync::atomic::Ordering::Relaxed),
                completed: sampler_completed.load(std::sync::atomic::Ordering::Relaxed),
                planned_aborted: sampler_aborted.load(std::sync::atomic::Ordering::Relaxed),
                unexpected_failed: sampler_failed.load(std::sync::atomic::Ordering::Relaxed),
            };
            eprintln!(
                "[soak time_series] t={:4.1}s rss_kib={} conns={} starting={} active={} idle={} user_requests={} user_sessions={} shared_origins={} shared_subscribers={} shared_meters={} running_tasks={} completed={} planned_aborted={} unexpected_failed={}",
                sample.elapsed_secs,
                rss,
                sample.provider_connections,
                sample.lease_starting,
                sample.lease_active,
                sample.lease_idle,
                sample.user_requests,
                sample.user_sessions,
                sample.shared_origins,
                sample.shared_subscribers,
                sample.shared_meters,
                sample.running_tasks,
                sample.completed,
                sample.planned_aborted,
                sample.unexpected_failed,
            );
            sampler_samples.lock().unwrap().push(sample);
        }
    });

    let mut tasks = tokio::task::JoinSet::new();
    for task_index in 0..16u16 {
        let app = Arc::clone(&app);
        let input = Arc::clone(&input);
        let completed = Arc::clone(&completed);
        let aborted = Arc::clone(&aborted);
        let unexpected_failed = Arc::clone(&unexpected_failed);
        let peak_connections = Arc::clone(&peak_connections);
        let running_tasks = Arc::clone(&running_tasks);
        tasks.spawn(async move {
            running_tasks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let result = async {
                let addr = SocketAddr::from(([127, 0, 0, 1], 47_000 + task_index));
                let fingerprint = create_test_fingerprint(addr);
                let username = format!("soak-user-{task_index}");
                let user = load_test_user(&username);
                let token = format!("soak-reconnect-family-{task_index}");
                let mut round = 0u64;
                while std::time::Instant::now() < deadline {
                    round += 1;
                    let session = load_test_session(&input, origin_addr, &token, addr);
                    let channel = load_test_channel(&input, origin_addr);
                    let response = force_provider_stream_response(
                        &fingerprint,
                        &app,
                        &session,
                        channel,
                        ForceStreamRequestContext {
                            req_headers: &HeaderMap::new(),
                            input: &input,
                            user: &user,
                            session_reservation_ttl_secs: 2,
                            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                        },
                        None,
                    )
                    .await
                    .into_response();

                    let status = response.status();
                    if status != StatusCode::OK {
                        unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return Err(format!("soak response status was {status}"));
                    }
                    // Alternate full playback (collect) and abort (drop without consuming).
                    if round % 2 == 0 {
                        let collected = match response.into_body().collect().await {
                            Ok(body) => body.to_bytes(),
                            Err(err) => {
                                unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                return Err(format!("soak body collection failed: {err}"));
                            }
                        };
                        if let Err(err) = validate_load_response(status, &collected, &SOAK_PACKET) {
                            unexpected_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return Err(err);
                        }
                        completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        drop(response);
                        aborted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    let current = app.active_provider.get_provider_connections_count();
                    peak_connections.fetch_max(current, std::sync::atomic::Ordering::Relaxed);
                }
                Ok::<(), String>(())
            }
            .await;
            running_tasks.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            result
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("soak task must join").unwrap_or_else(|err| panic!("soak task failed: {err}"));
    }

    sampler_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler_task.await;

    let completed = completed.load(std::sync::atomic::Ordering::Relaxed);
    let aborted = aborted.load(std::sync::atomic::Ordering::Relaxed);
    let peak = peak_connections.load(std::sync::atomic::Ordering::Relaxed);
    let unexpected_failed = unexpected_failed.load(std::sync::atomic::Ordering::Relaxed);
    let rss_delta_kib = load_test_rss_kib()
        .zip(baseline_rss_kib)
        .map(|(rss, baseline)| rss.saturating_sub(baseline).to_string())
        .unwrap_or_else(|| "unsupported".to_string());
    eprintln!(
        "soak duration={duration_secs}s completed={completed} planned_aborted={aborted} unexpected_failed={unexpected_failed} peak_connections={peak} rss_delta_kib={rss_delta_kib}"
    );

    let samples = Arc::try_unwrap(resource_samples).expect("resource samples unique").into_inner().unwrap();
    assert!(!samples.is_empty(), "soak must record at least one resource sample");
    let third = (samples.len() / 3).max(1);
    let middle_start = third.min(samples.len());
    let middle_end = (third.saturating_mul(2)).min(samples.len());
    let last_start = middle_end.min(samples.len());
    let mut middle_rss =
        samples[middle_start..middle_end].iter().filter_map(|sample| sample.rss_kib).collect::<Vec<_>>();
    let mut last_rss = samples[last_start..].iter().filter_map(|sample| sample.rss_kib).collect::<Vec<_>>();
    match (median_u64(&mut middle_rss), median_u64(&mut last_rss)) {
        (Some(middle), Some(last)) => {
            let delta = i128::from(last) - i128::from(middle);
            eprintln!("soak rss rolling-median middle_third={middle}KiB last_third={last}KiB delta={delta}KiB");
        }
        _ => eprintln!("soak rss rolling-median unsupported on this platform"),
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let leases = app.active_provider.provider_lease_usage(&input.name);
            let (user_requests, user_sessions) = app.active_users.playback_resource_counts().await;
            let (shared_origins, shared_subscribers) = app.shared_stream_manager.resource_counts().await;
            let shared_meters = app.shared_stream_manager.meter_count();
            if app.active_provider.get_provider_connections_count() == 0
                && leases.total() == 0
                && user_requests == 0
                && user_sessions == 0
                && shared_origins == 0
                && shared_subscribers == 0
                && shared_meters == 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("soak resources must drain to baseline");

    assert_eq!(
        app.active_provider.get_provider_connections_count(),
        0,
        "provider slots must return to baseline after soak"
    );
    let usage = app.active_provider.provider_lease_usage(&input.name);
    assert_eq!(usage.total(), 0, "lease table must return to baseline after soak");
    assert_eq!(unexpected_failed, 0, "soak must not hide failed operations");
    assert!(peak <= 16, "active connections must stay bounded by the churn workers, observed {peak}");
}

#[tokio::test]
async fn direct_ts_eof_before_first_byte_releases_slot_without_idle_lease() {
    let (origin_addr, origin_task) = spawn_controlled_fake_origin(FakeOriginMode::EmptyBody, 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_601));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_ts_eof".to_string();

    let session = UserSession {
        token: "session-eof".to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let channel = load_test_channel(&input, origin_addr);

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = response.into_body().collect().await.expect("empty body collects").to_bytes();
    assert!(body_bytes.is_empty(), "expected 0 payload bytes on immediate EOF");

    let requests = origin_task.await.expect("origin finishes");
    assert_eq!(requests.len(), 1);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "physical slot must be released");
    let lease_usage = app_state.active_provider.provider_lease_usage(&input.name);
    assert_eq!(lease_usage.starting, 0, "no starting leases remaining");
    assert_eq!(lease_usage.active, 0, "no active leases remaining");
    assert_eq!(lease_usage.idle, 0, "unconfirmed stream must NOT transition to an idle lease");
    assert_eq!(lease_usage.total(), 0, "total leases must be 0");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "active user stream must be cleaned up");
}

#[tokio::test]
async fn direct_ts_abort_while_waiting_for_first_byte_releases_exact_request() {
    let notify = Arc::new(tokio::sync::Notify::new());
    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::BlockFirstChunk(Arc::clone(&notify)), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_602));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_ts_abort".to_string();

    let session = UserSession {
        token: "session-abort-before-first-byte".to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let channel = load_test_channel(&input, origin_addr);

    let response = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    drop(response);
    notify.notify_waiters();

    let _ = origin_task.await;

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        0,
        "physical slot must be released on abort"
    );
    let lease_usage = app_state.active_provider.provider_lease_usage(&input.name);
    assert_eq!(lease_usage.total(), 0, "no residual lease claim after unconfirmed abort");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "active user stream must be cleaned up");
}

#[tokio::test]
async fn provider_error_after_first_byte_finishes_only_matching_request() {
    const TS_CHUNK_188: &[u8] = &[0x47; 188];
    const SIBLING_CHUNK: &[u8] = &[0x47; 376];

    let (origin_fail_addr, origin_fail_task) =
        spawn_controlled_fake_origin(FakeOriginMode::ErrorAfterFirstChunk(TS_CHUNK_188.to_vec()), 1).await;
    let (origin_ok_addr, origin_ok_task) =
        spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(SIBLING_CHUNK.to_vec()), 1).await;

    let input_fail = Arc::new(ConfigInput {
        id: 1,
        name: "provider_fail".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_fail_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let input_ok = Arc::new(ConfigInput {
        id: 2,
        name: "provider_ok".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_ok_addr}"),
        enabled: true,
        priority: 1,
        max_connections: 2,
        ..ConfigInput::default()
    });

    let mut config = create_test_provider_app_config();
    config.sources = Arc::new(ArcSwap::from_pointee(SourcesConfig {
        inputs: vec![Arc::clone(&input_fail), Arc::clone(&input_ok)],
        ..SourcesConfig::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let addr_fail = SocketAddr::from(([127, 0, 0, 1], 55_603));
    let addr_ok = SocketAddr::from(([127, 0, 0, 1], 55_604));

    let user = load_test_user("viewer_multistream");
    let session_fail = load_test_session(&input_fail, origin_fail_addr, "session-fail", addr_fail);
    let session_ok = load_test_session(&input_ok, origin_ok_addr, "session-ok", addr_ok);

    let channel_fail = load_test_channel(&input_fail, origin_fail_addr);
    let channel_ok = load_test_channel(&input_ok, origin_ok_addr);

    let resp_fail = force_provider_stream_response(
        &create_test_fingerprint(addr_fail),
        &app_state,
        &session_fail,
        channel_fail,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input_fail,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let resp_ok = force_provider_stream_response(
        &create_test_fingerprint(addr_ok),
        &app_state,
        &session_ok,
        channel_ok,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input_ok,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_fail.status(), StatusCode::OK);
    assert_eq!(resp_ok.status(), StatusCode::OK);
    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        2,
        "both providers have an active connection"
    );

    let mut fail_body = resp_fail.into_body();
    let first_frame = fail_body.frame().await.expect("first frame exists").expect("first frame is valid");
    let first_chunk = first_frame.into_data().expect("data chunk exists");
    assert_eq!(first_chunk.as_ref(), TS_CHUNK_188, "first byte was confirmed with exact chunk bytes");

    let second_frame_res = fail_body.frame().await;
    assert!(
        second_frame_res.is_none() || second_frame_res.as_ref().is_some_and(Result::is_err),
        "stream must terminate or error after first chunk"
    );
    drop(fail_body);

    for _ in 0..50 {
        if app_state.active_provider.provider_lease_usage(&input_fail.name).total() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(
        app_state.active_provider.provider_lease_usage(&input_fail.name).total(),
        0,
        "failed provider leases cleaned up"
    );
    assert_eq!(
        app_state.active_provider.provider_lease_usage(&input_ok.name).total(),
        1,
        "sibling provider lease remains intact"
    );
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1, "sibling connection still active");

    let ok_bytes = resp_ok.into_body().collect().await.expect("sibling body collects").to_bytes();
    assert_eq!(ok_bytes.as_ref(), SIBLING_CHUNK);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "all slots returned to baseline");
    let _ = origin_fail_task.await;
    let _ = origin_ok_task.await;
}

#[tokio::test]
async fn no_first_byte_failure_allows_next_request_to_prefer_provider_a() {
    const TS_CHUNK: &[u8] = &[0x47; 188];
    let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("origin A binds");
    let origin_a_addr = listener_a.local_addr().expect("origin A address");
    let task_a = tokio::spawn(async move {
        let mut requests = Vec::new();
        for round in 0..2 {
            let Ok((mut socket, _)) = listener_a.accept().await else { break };
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut chunk = [0u8; 512];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                req.extend_from_slice(&chunk[..read]);
            }
            requests.push(String::from_utf8_lossy(&req).to_ascii_lowercase());
            if round == 0 {
                let head =
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.shutdown().await;
            } else {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    TS_CHUNK.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(TS_CHUNK).await;
                let _ = socket.shutdown().await;
            }
        }
        requests
    });

    let (origin_b_addr, _task_b) = spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(TS_CHUNK.to_vec()), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_A".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_B".intern(),
            url: format!("http://{origin_b_addr}"),
            username: None,
            password: None,
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });

    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_605));
    let fingerprint = create_test_fingerprint(client_addr);
    let user = load_test_user("viewer_prefer_a");

    let session_1 = load_test_session(&input, origin_a_addr, "session-1", client_addr);
    let channel_1 = load_test_channel(&input, origin_a_addr);

    let resp_1 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_1,
        channel_1,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_1.status(), StatusCode::OK);
    let b1 = resp_1.into_body().collect().await.expect("body 1 collects").to_bytes();
    assert!(b1.is_empty(), "request 1 had early EOF");

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0
            && app_state.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "slot on A must be free");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0, "no phantom lease on A");

    let session_2 = load_test_session(&input, origin_a_addr, "session-2", client_addr);
    let channel_2 = load_test_channel(&input, origin_a_addr);

    let resp_2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_2,
        channel_2,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp_2.status(), StatusCode::OK);
    let b2 = resp_2.into_body().collect().await.expect("body 2 collects").to_bytes();
    assert_eq!(b2.as_ref(), TS_CHUNK, "request 2 successfully used Provider A and read payload");

    let reqs_a = task_a.await.expect("task A completed");
    assert_eq!(reqs_a.len(), 2, "both requests 1 and 2 were routed to Provider A!");
}

#[tokio::test]
async fn parallel_vod_abort_preserves_sibling_claim_and_provider_stickiness() {
    const VOD_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(VOD_BODY, 3).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_701));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_vod_parallel".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/movie/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/movie/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Video;
        channel.cluster = XtreamCluster::Video;
        channel.url = format!("http://{origin_addr}/movie/1.ts").intern();
        channel
    };

    let build_request = |range: &'static str, token: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static(range));
        let session = make_session(token);
        (headers, session)
    };

    let (first_headers, first_session) = build_request("bytes=0-9", "vod-playback");
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &first_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);

    let (second_headers, second_session) = build_request("bytes=5-14", "vod-playback");
    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &second_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "affine claims share one playback row");
    assert_eq!(app_state.active_users.playback_resource_counts().await.0, 2, "both request claims active");

    drop(first);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_users.playback_resource_counts().await.0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted VOD body must release only its request claim");
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "sibling playback row remains active");

    let second_body = second.into_body().collect().await.expect("second body collects").to_bytes();
    assert_eq!(second_body.as_ref(), &VOD_BODY[5..15]);

    let (third_headers, third_session) = build_request("bytes=10-19", "vod-playback");
    let third = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &third_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &third_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(third.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        third.headers().get(header::CONTENT_RANGE).and_then(|value| value.to_str().ok()),
        Some("bytes 10-19/20")
    );
    let third_body = third.into_body().collect().await.expect("third body collects").to_bytes();
    assert_eq!(third_body.as_ref(), &VOD_BODY[10..20]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 3, "all 3 range requests must reach the same provider account");
    assert!(requests[0].contains("range: bytes=0-9"));
    assert!(requests[1].contains("range: bytes=5-14"));
    assert!(requests[2].contains("range: bytes=10-19"));

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("VOD provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("vod-playback");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
}

#[tokio::test]
async fn parallel_series_abort_then_seek_reuses_account_without_stale_claim() {
    const SERIES_BODY: &[u8] = b"0123456789abcdefghij";

    let (origin_addr, origin_task) = spawn_range_aware_test_origin(SERIES_BODY, 3).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_702));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer_series_parallel".to_string();

    let make_session = |token: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 43,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/series/1.ts").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let make_channel = || {
        let mut channel = create_test_local_channel(&format!("http://{origin_addr}/series/1.ts"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Series;
        channel.cluster = XtreamCluster::Series;
        channel.url = format!("http://{origin_addr}/series/1.ts").intern();
        channel
    };

    let build_request = |range: &'static str, token: &'static str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static(range));
        let session = make_session(token);
        (headers, session)
    };

    let (first_headers, first_session) = build_request("bytes=0-9", "series-playback");
    let first = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &first_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &first_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);

    let (second_headers, second_session) = build_request("bytes=5-14", "series-playback");
    let second = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &second_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &second_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "affine claims share one playback row");
    assert_eq!(app_state.active_users.playback_resource_counts().await.0, 2, "both request claims active");

    drop(first);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_users.playback_resource_counts().await.0 != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted Series body must release only its request claim");
    assert_eq!(app_state.active_users.active_streams().await.len(), 1, "sibling playback row remains active");

    let second_body = second.into_body().collect().await.expect("second body collects").to_bytes();
    assert_eq!(second_body.as_ref(), &SERIES_BODY[5..15]);

    let (third_headers, third_session) = build_request("bytes=12-19", "series-playback");
    let third = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &third_session,
        make_channel(),
        ForceStreamRequestContext {
            req_headers: &third_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 15,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(third.status(), StatusCode::PARTIAL_CONTENT);
    let third_body = third.into_body().collect().await.expect("third body collects").to_bytes();
    assert_eq!(third_body.as_ref(), &SERIES_BODY[12..20]);

    let requests = origin_task.await.expect("origin task completes");
    assert_eq!(requests.len(), 3, "all series requests reach same provider");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("series provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("series-playback");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
}

#[tokio::test]
async fn stale_direct_cleanup_after_rebind_preserves_successor_request() {
    let app_state = create_test_provider_app_state();
    let provider_name: Arc<str> = Arc::from("provider_1");
    let owner = "test-rebind-owner";
    let addr_x = SocketAddr::from(([127, 0, 0, 1], 55_631));
    let addr_y = SocketAddr::from(([127, 0, 0, 1], 55_632));

    let req_x = tuliprox_core::model::PlaybackRequestId::from_raw(101);
    let lease_ref_x =
        tuliprox_session::PlaybackLeaseRef { owner, kind: tuliprox_core::model::PlaybackKind::Vod, request_id: req_x };
    let handle_x = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_x,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_x),
        )
        .expect("request X acquires provider");
    let stale_tag = handle_x.binding_tag.expect("request X binding tag");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_x, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_x);
    app_state.active_provider.release_handle(&handle_x);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );
    assert_eq!(app_state.active_provider.provider_lease_usage(&provider_name).idle, 1);

    let req_y = tuliprox_core::model::PlaybackRequestId::from_raw(102);
    let lease_ref_y =
        tuliprox_session::PlaybackLeaseRef { owner, kind: tuliprox_core::model::PlaybackKind::Vod, request_id: req_y };
    let handle_y = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_y,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_y),
        )
        .expect("request Y reacquires provider");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_y, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_y);

    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let usage_after_stale_finish = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage_after_stale_finish.active, 1, "successor request Y must keep the active lease alive");

    app_state.active_provider.clear_identified_provider_reservation(owner, &provider_name, Some(stale_tag));

    let usage_after_stale_tag = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage_after_stale_tag.active, 1, "stale clear tag must not erase active successor lease");

    app_state.active_provider.release_handle(&handle_y);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_y,
        tuliprox_core::model::PlaybackRequestOutcome::ServerShutdown,
    );

    let final_usage = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(final_usage.total(), 0, "request Y has finished");
}

#[tokio::test]
async fn stale_hls_cleanup_after_rebind_preserves_current_origin_and_segments() {
    let app_state = create_test_provider_app_state();
    let provider_name: Arc<str> = Arc::from("provider_1");
    let owner = "test-hls-rebind-owner";
    let addr_x = SocketAddr::from(([127, 0, 0, 1], 55_633));
    let addr_y = SocketAddr::from(([127, 0, 0, 1], 55_634));

    let req_x = tuliprox_core::model::PlaybackRequestId::from_raw(201);
    let lease_ref_x = tuliprox_session::PlaybackLeaseRef {
        owner,
        kind: tuliprox_core::model::PlaybackKind::LiveHls,
        request_id: req_x,
    };
    let handle_x = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_x,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_x),
        )
        .expect("request X acquires HLS provider");
    let stale_tag = handle_x.binding_tag.expect("request X binding tag");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_x, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_x);
    app_state.active_provider.release_handle(&handle_x);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let req_y = tuliprox_core::model::PlaybackRequestId::from_raw(202);
    let lease_ref_y = tuliprox_session::PlaybackLeaseRef {
        owner,
        kind: tuliprox_core::model::PlaybackKind::LiveHls,
        request_id: req_y,
    };
    let handle_y = app_state
        .active_provider
        .acquire_exact_connection_with_lease_for_session(
            &provider_name,
            &addr_y,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
            Some(lease_ref_y),
        )
        .expect("request Y reacquires HLS provider");
    app_state.active_provider.refresh_playback_lease(&provider_name, &lease_ref_y, 30);
    app_state.active_provider.confirm_identified_playback_activity(owner, req_y);

    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_x,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );

    let usage = app_state.active_provider.provider_lease_usage(&provider_name);
    assert!(usage.active >= 1 || usage.idle >= 1, "active/reconnect lease remains for successor");

    app_state.active_provider.clear_identified_provider_reservation(owner, &provider_name, Some(stale_tag));

    let usage2 = app_state.active_provider.provider_lease_usage(&provider_name);
    assert_eq!(usage2.total(), usage.total(), "stale clear tag must not drop the HLS lease");

    app_state.active_provider.release_handle(&handle_y);
    app_state.active_provider.finish_identified_playback_request(
        owner,
        req_y,
        tuliprox_core::model::PlaybackRequestOutcome::ServerShutdown,
    );
    assert_eq!(app_state.active_provider.provider_lease_usage(&provider_name).total(), 0);
}

#[tokio::test]
async fn shared_proxy_clients_keep_independent_bodies_and_cleanup() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("shared origin binds");
    let origin_addr = listener.local_addr().expect("shared origin addr");
    let app = axum::Router::new().fallback(|| async {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        tokio::spawn(async move {
            for i in 0..10u8 {
                let _ = tx.send(Ok(Bytes::from(vec![i; 188]))).await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "video/mp2t")
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap()
    });
    let origin_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_p".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 5,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let proxy_addr = SocketAddr::from(([127, 0, 0, 1], 55_190));
    let fp1 = Fingerprint::new("client1|ua1".to_string(), "127.0.0.1".to_string(), proxy_addr);
    let fp2 = Fingerprint::new("client2|ua2".to_string(), "127.0.0.1".to_string(), proxy_addr);

    let user1 = load_test_user("user_shared_1");
    let user2 = load_test_user("user_shared_2");
    let stream_url = format!("http://{origin_addr}/live/user/pass/shared.ts");
    let mut channel1 = load_test_channel(&input, origin_addr);
    channel1.url = stream_url.as_str().intern();
    let mut channel2 = load_test_channel(&input, origin_addr);
    channel2.url = stream_url.as_str().intern();

    let resp1 = stream_response(
        &fp1,
        &app_state,
        "shared-session-1",
        None,
        channel1,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user1,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();
    assert_eq!(resp1.status(), StatusCode::OK);

    let resp2 = stream_response(
        &fp2,
        &app_state,
        "shared-session-2",
        None,
        channel2,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user2,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();
    assert_eq!(resp2.status(), StatusCode::OK);

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1, "only 1 shared origin connection");

    drop(resp1);

    let body2 = resp2.into_body().collect().await.expect("client 2 body collects").to_bytes();
    assert!(!body2.is_empty(), "client 2 receives stream bytes after client 1 dropped");
    drop(body2);

    for _ in 0..50 {
        if app_state.active_provider.get_provider_connections_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    origin_task.abort();
}

#[tokio::test]
async fn shared_admission_rejection_does_not_leak_meter_registration() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let origin_addr = spawn_load_test_origin(b"adm-test-ts-chunk").await;
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_adm".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 5,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    config.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig { metrics_enabled: true, ..crate::model::StreamConfig::default() }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let cleanup_tx = app_state.connection_manager.cleanup_tx();
    let mut permits = Vec::new();
    while let Ok(permit) = cleanup_tx.clone().try_reserve_owned() {
        permits.push(permit);
    }

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_195));
    let fp = create_test_fingerprint(client_addr);
    let user = load_test_user("user_adm_fail");
    let stream_url = format!("http://{origin_addr}/live/user/pass/adm.ts");
    let mut channel = load_test_channel(&input, origin_addr);
    channel.url = stream_url.as_str().intern();

    let resp = stream_response(
        &fp,
        &app_state,
        "shared-session-adm",
        None,
        channel,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "shared admission failure must yield 503 Service Unavailable"
    );
    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        0,
        "admission failure must NOT fall through to opening a provider connection"
    );
    assert_eq!(app_state.shared_stream_manager.meter_count(), 0, "rejected admission must not retain a meter ID");

    drop(permits);
}

#[tokio::test]
async fn shared_start_abort_does_not_leak_meter_registration() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let notify = Arc::new(tokio::sync::Notify::new());
    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::BlockFirstChunk(Arc::clone(&notify)), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_abort".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    config.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig { metrics_enabled: true, ..crate::model::StreamConfig::default() }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_196));
    let fp = create_test_fingerprint(client_addr);
    let user = load_test_user("user_shared_abort");
    let stream_url = format!("http://{origin_addr}/live/user/pass/shared_abort.ts");
    let mut channel = load_test_channel(&input, origin_addr);
    channel.url = stream_url.as_str().intern();

    let resp = stream_response(
        &fp,
        &app_state,
        "shared-session-abort",
        None,
        channel,
        &stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    assert_eq!(resp.status(), StatusCode::OK);
    drop(resp);
    notify.notify_waiters();
    let _ = origin_task.await;

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0
            || app_state.active_provider.provider_lease_usage(&input.name).total() != 0
            || !app_state.active_users.active_streams().await.is_empty()
            || app_state.shared_stream_manager.meter_count() != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shared abort resources must return to baseline");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0, "provider slot released");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0, "provider request released");
    assert_eq!(app_state.active_users.active_streams().await.len(), 0, "user claim released");
    assert_eq!(app_state.shared_stream_manager.meter_count(), 0, "aborted origin meter registration released");
}

#[tokio::test]
async fn catchup_abort_seek_and_window_change_preserve_correct_affinity() {
    const CATCHUP_BODY_A: &[u8] = b"catchup-origin-a-archive-payload";
    const CATCHUP_BODY_B: &[u8] = b"catchup-origin-b-archive-payload";

    let (origin_a, task_a) = spawn_range_aware_test_origin(CATCHUP_BODY_A, 2).await;
    let (origin_b, task_b) = spawn_range_aware_test_origin(CATCHUP_BODY_B, 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_a}"),
        enabled: true,
        priority: 0,
        max_connections: 2,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: format!("http://{origin_b}"),
            username: None,
            password: None,
            priority: 1,
            max_connections: 2,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_620));
    let fingerprint = create_test_fingerprint(client_addr);
    let user = load_test_user("viewer_catchup");

    let make_catchup_session = |token: &str, window: &str| UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_a}/timeshift/1.ts?window={window}").intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    let make_catchup_channel = |window: &str| {
        let mut channel = create_test_local_channel(&format!("http://{origin_a}/timeshift/1.ts?window={window}"));
        channel.provider_id = 1;
        channel.input_name = Arc::clone(&input.name);
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Live;
        channel.url = format!("http://{origin_a}/timeshift/1.ts?window={window}").intern();
        channel
    };

    let session_w1_1 = make_catchup_session("catchup-w1", "w1");
    let session_w1_2 = make_catchup_session("catchup-w1", "w1");

    let mut h1 = HeaderMap::new();
    h1.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
    let mut h2 = HeaderMap::new();
    h2.insert(header::RANGE, HeaderValue::from_static("bytes=5-14"));

    let resp1 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w1_1,
        make_catchup_channel("w1"),
        ForceStreamRequestContext {
            req_headers: &h1,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let resp2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w1_2,
        make_catchup_channel("w1"),
        ForceStreamRequestContext {
            req_headers: &h2,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(resp1.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp2.status(), StatusCode::PARTIAL_CONTENT);

    let mut session_w2 = make_catchup_session("catchup-w2", "w2");
    session_w2.provider = "provider_2".intern();
    session_w2.stream_url = format!("http://{origin_b}/timeshift/1.ts?window=w2").intern();
    let mut channel_w2 = make_catchup_channel("w2");
    channel_w2.url = Arc::clone(&session_w2.stream_url);
    let mut h_w2 = HeaderMap::new();
    h_w2.insert(header::RANGE, HeaderValue::from_static("bytes=0-9"));
    let resp_w2 = force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session_w2,
        channel_w2,
        ForceStreamRequestContext {
            req_headers: &h_w2,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 10,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();
    assert_eq!(resp_w2.status(), StatusCode::PARTIAL_CONTENT);
    let bytes_w2 = resp_w2.into_body().collect().await.expect("window w2 body").to_bytes();
    assert_eq!(bytes_w2.as_ref(), &CATCHUP_BODY_B[..10], "independent window uses available provider B");

    drop(resp1);
    let bytes2 = resp2.into_body().collect().await.expect("resp2 body").to_bytes();
    assert_eq!(bytes2.as_ref(), &CATCHUP_BODY_A[5..15]);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parallel catchup requests must release their physical connections");

    let reqs_a = task_a.await.expect("task A completes");
    assert_eq!(reqs_a.len(), 2, "both requests in window w1 stayed on provider A");
    let reqs_b = task_b.await.expect("task B completes");
    assert_eq!(reqs_b.len(), 1, "the independent window used provider B while A was full");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app_state.active_provider.get_provider_connections_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("catchup provider connections must drain");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    app_state.active_provider.clear_provider_reservation("catchup-w1");
    app_state.active_provider.clear_provider_reservation("catchup-w2");
    assert_eq!(app_state.active_provider.provider_lease_usage(&input.name).total(), 0);
    assert_eq!(app_state.active_provider.provider_lease_usage(&"provider_2".intern()).total(), 0);
}

#[tokio::test]
async fn dash_stream_request_remains_redirect_and_preserves_provider_affinity() {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_dash".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-dash.example".to_string(),
        username: Some("dash-user".to_string()),
        password: Some("dash-pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 2,
        ..ConfigInput::default()
    });
    let target = Arc::new(ConfigTarget {
        id: 1,
        name: "target_dash".to_string(),
        enabled: true,
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });

    let mut app_cfg = create_test_provider_app_config();
    app_cfg.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: None,
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));

    let pli = PlaylistItem {
        header: PlaylistItemHeader {
            id: "1".intern(),
            name: "DASH Live".intern(),
            group: "Live".intern(),
            title: "DASH Live".intern(),
            url: "http://provider-dash.example/live/dash-user/dash-pass/1.mpd".intern(),
            item_type: PlaylistItemType::LiveDash,
            xtream_cluster: XtreamCluster::Live,
            virtual_id: shared::model::VirtualId(1),
            ..PlaylistItemHeader::default()
        },
    };

    let mut user = load_test_user("dash_viewer");
    user.proxy = ProxyType::Reverse(None);
    for target_type in [TargetType::M3u, TargetType::Xtream] {
        let redirect_params = RedirectParams {
            item: &pli,
            provider_id: pli.get_provider_id(),
            cluster: XtreamCluster::Live,
            target_type,
            target: &target,
            input: &input,
            user: &user,
            stream_ext: Some(shared::defaults::DASH_EXT),
            req_context: crate::api::endpoints::xtream_api::ApiStreamContext::Live,
            action_path: "",
        };

        let resp = redirect_response(&app_state, &redirect_params)
            .await
            .expect("DASH must return a redirect response")
            .into_response();
        assert_eq!(resp.status(), StatusCode::FOUND, "DASH request must be HTTP 302 Found redirect");
        let location = resp.headers().get(axum::http::header::LOCATION).expect("location header present");
        let location_str = location.to_str().expect("valid location string");
        assert!(
            location_str.contains("provider-dash.example"),
            "redirect location must point to provider URL, got {location_str}"
        );
    }

    let dash_ttl = get_session_reservation_ttl_secs(&app_state, PlaylistItemType::LiveDash);
    assert_eq!(dash_ttl, default_hls_session_ttl_secs(), "DASH preserves provider affinity TTL");
}

#[tokio::test]
async fn http_provider_lease_load_test_negative_control_detects_payload_corruption() {
    const TS_PACKET_VALID: [u8; 188] = [0x47; 188];
    const TS_PACKET_CORRUPT: [u8; 188] = [0; 188];

    let (origin_addr, origin_task) =
        spawn_controlled_fake_origin(FakeOriginMode::FixedBytes(TS_PACKET_CORRUPT.to_vec()), 1).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_neg_ctrl".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let addr = SocketAddr::from(([127, 0, 0, 1], 55_650));
    let fingerprint = create_test_fingerprint(addr);
    let session = load_test_session(&input, origin_addr, "neg-session", addr);
    let channel = load_test_channel(&input, origin_addr);
    let user = load_test_user("neg_ctrl_user");

    let response = force_provider_stream_response(
        &fingerprint,
        &app,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    let status = response.status();
    let body = response.into_body().collect().await.expect("body collects").to_bytes();
    assert!(
        validate_load_response(status, &body, &TS_PACKET_VALID).is_err(),
        "negative control must be rejected by the same validator as the load harness"
    );

    let _ = origin_task.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_provider_lease_smoke_test() {
    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_smoke".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    for concurrency in [1usize, 2] {
        let rounds = 5;
        let mut tasks = tokio::task::JoinSet::new();
        for task_idx in 0..concurrency {
            let app = Arc::clone(&app);
            let input = Arc::clone(&input);
            let user = load_test_user(&format!("smoke-user-{task_idx}"));
            tasks.spawn(async move {
                for r in 0..rounds {
                    let addr = SocketAddr::from(([127, 0, 0, 1], 48_000 + (task_idx as u16)));
                    let fingerprint = create_test_fingerprint(addr);
                    let token = format!("smoke-{task_idx}-{r}");
                    let session = load_test_session(&input, origin_addr, &token, addr);
                    let channel = load_test_channel(&input, origin_addr);

                    let response = force_provider_stream_response(
                        &fingerprint,
                        &app,
                        &session,
                        channel,
                        ForceStreamRequestContext {
                            req_headers: &HeaderMap::new(),
                            input: &input,
                            user: &user,
                            session_reservation_ttl_secs: 0,
                            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                        },
                        None,
                    )
                    .await
                    .into_response();

                    let status = response.status();
                    let body = response.into_body().collect().await.expect("smoke body collects").to_bytes();
                    validate_load_response(status, &body, &TS_PACKET).expect("smoke response must match the TS packet");
                }
            });
        }
        while let Some(res) = tasks.join_next().await {
            res.expect("smoke task must succeed");
        }
    }

    for _ in 0..50 {
        if app.active_provider.get_provider_connections_count() == 0
            && app.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "smoke slots baseline");
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 0, "smoke lease baseline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_provider_lease_downstream_smoke_test() {
    const TS_PACKET: [u8; 188] = [0x47; 188];
    let origin_addr = spawn_load_test_origin(&TS_PACKET).await;

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_downstream".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 0,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    #[derive(Clone)]
    struct DownstreamState {
        app: Arc<AppState>,
        input: Arc<ConfigInput>,
        origin_addr: SocketAddr,
    }

    async fn stream_handler(
        State(state): State<DownstreamState>,
        ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ) -> axum::response::Response {
        let fingerprint = create_test_fingerprint(addr);
        let user = load_test_user("downstream-user");
        let session = load_test_session(&state.input, state.origin_addr, "downstream-token", addr);
        let channel = load_test_channel(&state.input, state.origin_addr);
        force_provider_stream_response(
            &fingerprint,
            &state.app,
            &session,
            channel,
            ForceStreamRequestContext {
                req_headers: &HeaderMap::new(),
                input: &state.input,
                user: &user,
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response()
    }

    let router = Router::new().route("/stream", get(stream_handler)).with_state(DownstreamState {
        app: Arc::clone(&app),
        input: Arc::clone(&input),
        origin_addr,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("downstream listener binds");
    let downstream_addr = listener.local_addr().expect("downstream address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await;
    });

    let client = reqwest::Client::new();
    for concurrency in [1usize, 2] {
        let mut tasks = tokio::task::JoinSet::new();
        for task_idx in 0..concurrency {
            let client = client.clone();
            let url = format!("http://{downstream_addr}/stream");
            tasks.spawn(async move {
                for round in 0..3 {
                    let response = client.get(&url).send().await.expect("downstream response");
                    let status = response.status();
                    let body = response.bytes().await.expect("downstream body");
                    validate_load_response(status, &body, &TS_PACKET)
                        .unwrap_or_else(|err| panic!("downstream task {task_idx} round {round}: {err}"));
                }
            });
        }
        while let Some(res) = tasks.join_next().await {
            res.expect("downstream task must succeed");
        }
    }

    for _ in 0..50 {
        if app.active_provider.get_provider_connections_count() == 0
            && app.active_provider.provider_lease_usage(&input.name).total() == 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(app.active_provider.get_provider_connections_count(), 0, "downstream slots baseline");
    assert_eq!(app.active_provider.provider_lease_usage(&input.name).total(), 0, "downstream lease baseline");

    server.abort();
}

#[test]
fn test_regular_response_keeps_compression_enabled() {
    let response = Response::new(());

    assert!(should_compress_response(&response));
}

#[test]
fn test_get_stream_config_u64_uses_default_when_stream_config_missing() {
    assert_eq!(
        resolve_stream_config_u64(None, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs()),
        default_hls_session_ttl_secs()
    );
    assert_eq!(
        resolve_stream_config_u64(None, |stream| stream.catchup_session_ttl_secs, default_catchup_session_ttl_secs()),
        default_catchup_session_ttl_secs()
    );
}

#[tokio::test]
async fn test_get_session_reservation_ttl_secs_uses_hls_ttl_for_live_dash() {
    let app_state = create_test_app_state();
    assert_eq!(
        get_session_reservation_ttl_secs(&app_state, PlaylistItemType::LiveDash),
        default_hls_session_ttl_secs()
    );
}

#[test]
fn provider_affinity_policy_matches_stream_types() {
    assert!(!PlaylistItemType::Live.requires_provider_affinity());
    assert!(!PlaylistItemType::LiveUnknown.requires_provider_affinity());
    assert!(PlaylistItemType::LiveHls.requires_provider_affinity());
    assert!(PlaylistItemType::LiveDash.requires_provider_affinity());
    assert!(PlaylistItemType::Video.requires_provider_affinity());
    assert!(PlaylistItemType::Series.requires_provider_affinity());
    assert!(PlaylistItemType::Catchup.requires_provider_affinity());
}

#[tokio::test]
async fn resolve_streaming_strategy_honors_forced_provider_fallback_policy() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55301".parse().unwrap_or_else(|_| unreachable!());
    let strict_addr: SocketAddr = "127.0.0.1:55302".parse().unwrap_or_else(|_| unreachable!());
    let fallback_addr: SocketAddr = "127.0.0.1:55303".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let strict = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(strict_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("vod-session"),
            playback_kind: crate::model::PlaybackKind::Vod,
            accept_requested_stream_url: false,
        },
    )
    .await;
    assert!(strict.provider_handle.is_none(), "strict provider affinity should not allocate a different provider");
    assert!(
        matches!(
            strict.provider_stream_state,
            ProviderStreamState::Custom { reason: ProviderStreamCustomReason::ProviderExhausted, .. }
        ),
        "strict provider affinity should fail closed when the pinned provider is unavailable"
    );

    let fallback = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(fallback_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: true,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;
    let (ProviderStreamState::Available(Some(fallback_provider), _)
    | ProviderStreamState::GracePeriod(Some(fallback_provider), _)) = fallback.provider_stream_state
    else {
        panic!("fallback-enabled request should allocate a provider")
    };
    assert_eq!(fallback_provider.as_ref(), "provider_2");

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&strict_addr);
    app_state.active_provider.release_connection(&fallback_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rewrites_url_on_fallback_even_when_accept_requested_stream_url_is_true() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());
    let fallback_addr: SocketAddr = "127.0.0.1:55305".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let fallback = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(fallback_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: true,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("vod-session"),
            playback_kind: crate::model::PlaybackKind::Vod,
            accept_requested_stream_url: true,
        },
    )
    .await;

    let (ProviderStreamState::Available(Some(fallback_provider), url)
    | ProviderStreamState::GracePeriod(Some(fallback_provider), url)) = fallback.provider_stream_state
    else {
        panic!("fallback-enabled request should allocate fallback provider")
    };
    assert_eq!(fallback_provider.as_ref(), "provider_2");
    assert_eq!(url.as_ref(), "http://provider-2.example/movie/user2/pass2/1.mkv");

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&fallback_addr);
}

#[tokio::test]
async fn create_stream_response_details_preserves_stored_headers_when_fallback_open_fails() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let reacquire_addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let user = load_test_user("test-fallback-headers-user");
    let session_token = "sess-fallback-headers-1";
    let initial_headers = HashMap::from([("cookie".to_string(), "old_prov_sess=1".to_string())]);
    let created = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 1,
            provider: &pinned_provider,
            stream_url,
            addr: &reacquire_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(!created.is_empty());
    app_state.active_users.update_session_provider_headers(&user.username, session_token, &initial_headers).await;

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let channel = create_test_live_channel(stream_url);
    let details = create_stream_response_details(
        &app_state,
        &get_stream_options(&app_state.app_config),
        stream_url,
        &user.username,
        &create_test_fingerprint(reacquire_addr),
        &HeaderMap::new(),
        &input,
        &channel,
        PlaylistItemType::Video,
        crate::api::model::ProviderContentRepresentationMode::Identity,
        false,
        UserConnectionPermission::Allowed,
        Some(&pinned_provider),
        true,
        false,
        VirtualId::new(channel.virtual_id),
        0,
        crate::api::model::ConnectionKind::Normal,
        true,
        Some(session_token),
        Some(&initial_headers),
        true,
        None,
        None,
    )
    .await
    .unwrap_or_else(|err| panic!("create_stream_response_details should succeed: {err}"));

    assert_eq!(details.provider_name.as_deref(), Some("provider_2"));
    assert!(details.session_headers.is_none(), "fallback request should not pass pinned provider session headers");
    assert!(details.stream.is_none(), "test setup should fail to open the fallback provider stream");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");
    assert_eq!(
        session.provider_session_headers, initial_headers,
        "stored session headers should be retained when the fallback provider cannot be opened"
    );

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&reacquire_addr);
}

#[tokio::test]
async fn force_provider_stream_response_clears_stored_headers_after_fallback_open_succeeds() {
    const FALLBACK_BODY: &[u8] = b"fallback-provider";
    let response_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        FALLBACK_BODY.len()
    );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, FALLBACK_BODY.to_vec()).await;
    let app_config = create_test_dual_provider_app_config();
    let Some(configured_input) = app_config.sources.load().inputs.first().cloned() else { unreachable!() };
    let mut fallback_input = (*configured_input).clone();
    let Some(aliases) = fallback_input.aliases.as_mut() else { unreachable!() };
    let Some(fallback_alias) = aliases.first_mut() else { unreachable!() };
    fallback_alias.url = format!("http://{origin_addr}");
    app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(fallback_input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55308".parse().unwrap_or_else(|_| unreachable!());
    let reacquire_addr: SocketAddr = "127.0.0.1:55309".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

    let user = load_test_user("test-fallback-force-user");
    let session_token = "sess-fallback-force-1";
    let initial_headers = HashMap::from([("cookie".to_string(), "old_pinned_token=abc".to_string())]);
    let created = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 1,
            provider: &pinned_provider,
            stream_url,
            addr: &reacquire_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(!created.is_empty());
    app_state.active_users.update_session_provider_headers(&user.username, session_token, &initial_headers).await;

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");

    let channel = create_test_live_channel(stream_url);
    let response = force_provider_stream_response(
        &create_test_fingerprint(reacquire_addr),
        &app_state,
        &session,
        channel,
        ForceStreamRequestContext {
            req_headers: &HeaderMap::new(),
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.expect("fallback stream body").to_bytes();
    assert_eq!(body.as_ref(), FALLBACK_BODY);
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), origin_task)
        .await
        .expect("fallback request was not sent within 5 seconds")
        .expect("fallback origin task completes");
    assert!(!request.is_empty(), "fallback provider must receive the stream request");

    let updated_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, session_token)
        .await
        .expect("session should exist");
    assert!(
        updated_session.provider_session_headers.is_empty(),
        "stored session headers must be cleared after the fallback provider stream opens"
    );

    app_state.active_provider.release_connection(&busy_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rewrites_stale_alias_url_to_selected_main_provider() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://provider-2.example/live/user2/pass2/100.ts",
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("request should allocate the main provider")
    };
    assert_eq!(provider.as_ref(), "provider_1");
    assert_eq!(url.as_ref(), "http://provider-1.example/live/user1/pass1/100.ts");

    app_state.active_provider.release_connection(&addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rewrites_opaque_m3u_token_after_alias_allocation() {
    let app_config = create_test_dual_provider_app_config();
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider-a".intern(),
        input_type: InputType::M3u,
        url: "http://playlist.example/a.m3u?token=provider-a-token".to_string(),
        enabled: true,
        max_connections: 1,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider-b".intern(),
            url: "http://playlist.example/b.m3u?token=provider-b-token".to_string(),
            username: None,
            password: None,
            max_connections: 0,
            priority: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let provider_a = "provider-a".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let alias_addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &provider_a,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy provider A");

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://stream.example/channel/segment.ts?token=provider-a-token",
        &create_test_fingerprint(alias_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("request should allocate provider B")
    };
    assert_eq!(provider.as_ref(), "provider-b");
    assert_eq!(url.as_ref(), "http://stream.example/channel/segment.ts?token=provider-b-token");

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&alias_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rejects_unmapped_provider_url() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55305".parse().unwrap_or_else(|_| unreachable!());

    let strategy = resolve_streaming_strategy(
        &app_state,
        "http://unmapped.example/live/user1/pass1/100.ts",
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;

    assert!(strategy.provider_handle.is_none());
    assert!(matches!(
        strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::UnmappedProviderUrl, .. }
    ));

    app_state.active_provider.release_connection(&addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_accepts_stalker_portal_url() {
    let app_config = create_test_provider_app_config();
    let Some(configured_input) = app_config.sources.load().inputs.first().cloned() else { unreachable!() };
    let mut stalker_input = (*configured_input).clone();
    stalker_input.input_type = InputType::Stalker;
    stalker_input.username = None;
    stalker_input.password = None;
    app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(stalker_input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://line.example/play/live.php?mac=00:11:22:33:44:55&stream=347&extension=ts&play_token=abc";

    let strategy = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else { unreachable!() };
    assert_eq!(provider.as_ref(), "provider_1");
    assert_eq!(url.as_ref(), stream_url);

    app_state.active_provider.release_connection(&addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_rejects_stalker_url_after_forced_provider_fallback() {
    let app_config = create_test_dual_provider_app_config();
    let Some(configured_input) = app_config.sources.load().inputs.first().cloned() else { unreachable!() };
    let mut stalker_input = (*configured_input).clone();
    stalker_input.input_type = InputType::Stalker;
    app_config
        .sources
        .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(stalker_input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let pinned_provider = "provider_1".intern();
    let busy_addr: SocketAddr = "127.0.0.1:55308".parse().unwrap_or_else(|_| unreachable!());
    let fallback_addr: SocketAddr = "127.0.0.1:55309".parse().unwrap_or_else(|_| unreachable!());
    let stream_url = "http://line.example/play/live.php?mac=00:11:22:33:44:55&stream=347&extension=ts&play_token=abc";

    let busy = app_state.active_provider.acquire_exact_connection_with_grace(
        &pinned_provider,
        &busy_addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(busy.is_some(), "setup should occupy the pinned provider");

    let strategy = resolve_streaming_strategy(
        &app_state,
        stream_url,
        &create_test_fingerprint(fallback_addr),
        &input,
        StreamingAcquireOptions {
            force_provider: Some(&pinned_provider),
            allow_forced_provider_fallback: true,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: false,
        },
    )
    .await;

    assert!(matches!(
        strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::UnmappedProviderUrl, .. }
    ));

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&fallback_addr);
}

#[tokio::test]
async fn resolve_streaming_strategy_accepts_session_requested_stream_url() {
    let app_state = create_test_dual_provider_app_state();
    let input_name = "provider_1".intern();
    let input =
        app_state.app_config.sources.load().get_input_by_name(&input_name).cloned().unwrap_or_else(|| unreachable!());
    let addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
    let trusted_url = "http://unmapped.example/live/user1/pass1/100.ts";
    let strategy = resolve_streaming_strategy(
        &app_state,
        trusted_url,
        &create_test_fingerprint(addr),
        &input,
        StreamingAcquireOptions {
            force_provider: None,
            allow_forced_provider_fallback: false,
            allow_provider_grace: false,
            user_priority: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            session_owner: Some("live-session"),
            playback_kind: crate::model::PlaybackKind::LiveTs,
            accept_requested_stream_url: true,
        },
    )
    .await;

    let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
        panic!("session-requested URL should be accepted for the pinned provider")
    };
    assert_eq!(provider.as_ref(), "provider_1");
    assert_eq!(url.as_ref(), trusted_url);

    app_state.active_provider.release_connection(&addr);
}

#[test]
fn test_should_allow_exhausted_shared_reconnect_only_for_matching_shared_session() {
    let session = UserSession {
        transition_version: 1,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        token: "tok".to_string(),
        virtual_id: 282,
        provider: Arc::<str>::from("provider"),
        stream_url: Arc::<str>::from("http://provider/live/449924.ts"),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!()),
        socket_bound: false,
        active_addrs: vec!["127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!())],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    assert!(should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(false, Some(&session), 282, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 999, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/other.ts"));
}

fn create_test_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "local_media".intern(),
        input_type: InputType::Library,
        headers: HashMap::default(),
        url: "file:///tmp".to_string(),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: None,
        ..ConfigInput::default()
    });
    let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
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
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

/// App config whose only input carries `policy` and one enabled alias.
fn create_policy_app_config(policy: ResourcePolicyDto) -> AppConfig {
    let config = create_test_app_config();
    let policy = Arc::new(ResourcePolicy::from_dto(&policy).expect("test policy"));
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "main-input".intern(),
        input_type: InputType::M3u,
        url: "https://provider.example/playlist.m3u".to_string(),
        enabled: true,
        resource_policy: Some(policy),
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "aliased-input".intern(),
            url: "https://provider.example/alias.m3u".to_string(),
            username: None,
            password: None,
            priority: 0,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let inputs = vec![Arc::clone(&input)];
    let sources = SourcesConfig { inputs, group_lookup: build_group_lookup(&[input]), ..SourcesConfig::default() };
    config.sources.store(Arc::new(sources));
    config
}

#[test]
fn resource_authorization_uses_the_main_input_policy_for_an_alias() {
    let app_config = create_policy_app_config(ResourcePolicyDto {
        allowed_hosts: vec!["media.home.arpa".to_string()],
        allowed_networks: vec!["192.168.50.20/32".to_string()],
    });
    let input_name = Arc::from("aliased-input");
    let authorization = resolve_resource_authorization(&app_config, Some(&input_name)).expect("authorized");

    assert_eq!(authorization.input_name.as_deref(), Some("main-input"));
    assert!(authorization.policy.allows_host("media.home.arpa"));
    assert_ne!(authorization.policy_digest, public_only_policy().digest());
}

#[test]
fn resource_authorization_rejects_an_unknown_or_disabled_origin() {
    let app_config = create_policy_app_config(ResourcePolicyDto::default());
    let input_name = Arc::from("removed-input");
    let error = resolve_resource_authorization(&app_config, Some(&input_name)).expect_err("rejected");

    assert_eq!(error, ResourcePolicyError::UnknownOrigin("removed-input".to_string()));
    assert_eq!(rejection_status(&error), StatusCode::BAD_REQUEST);
}

#[test]
fn resource_authorization_follows_a_config_change_without_regenerating_links() {
    let app_config = create_policy_app_config(ResourcePolicyDto {
        allowed_hosts: vec!["first.home.arpa".to_string()],
        allowed_networks: vec!["192.168.50.20/32".to_string()],
    });
    let input_name = Arc::from("main-input");
    let before = resolve_resource_authorization(&app_config, Some(&input_name)).expect("authorized");

    let reloaded = create_policy_app_config(ResourcePolicyDto {
        allowed_hosts: vec!["second.home.arpa".to_string()],
        allowed_networks: vec!["10.0.0.0/8".to_string()],
    });
    app_config.sources.store(Arc::clone(&reloaded.sources.load()));

    let after = resolve_resource_authorization(&app_config, Some(&input_name)).expect("authorized");

    assert_ne!(before.policy_digest, after.policy_digest);
    assert!(after.policy.allows_host("second.home.arpa"));
    assert!(!after.policy.allows_host("first.home.arpa"));
}

fn create_test_provider_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-1.example".to_string(),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: None,
        ..ConfigInput::default()
    });
    let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
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
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

fn create_test_dual_provider_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-1.example".to_string(),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: "http://provider-2.example".to_string(),
            username: Some("user2".to_string()),
            password: Some("pass2".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
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
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

fn create_test_app_state() -> Arc<AppState> { create_test_app_state_for_config(Arc::new(create_test_app_config())) }

#[tokio::test]
async fn create_api_proxy_user_defaults_output_clusters_to_all() {
    let app_state = create_test_app_state();
    let user = create_api_proxy_user(&app_state);
    assert_eq!(user.output_clusters, ClusterFlags::all());
}

fn create_test_provider_app_state() -> Arc<AppState> {
    create_test_app_state_for_config(Arc::new(create_test_provider_app_config()))
}

fn create_test_dual_provider_app_state() -> Arc<AppState> {
    create_test_app_state_for_config(Arc::new(create_test_dual_provider_app_config()))
}

fn create_test_app_state_for_config(app_cfg: Arc<AppConfig>) -> Arc<AppState> {
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    let history_config = Some(StreamHistoryConfig::default());
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager = Arc::new(ConnectionManager::new(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        history_config.as_ref(),
    ));

    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
    let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

    Arc::new(AppState {
        forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        })),
        app_config: app_cfg,
        http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
        resource_clients: empty_resource_client_set(),
        downloads: Arc::new(crate::api::model::DownloadQueue::new()),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
        hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
        stalker_resolve_coordinator: Arc::default(),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        update_guard: crate::api::model::UpdateGuard::new(),
        metadata_manager,
        identity_registry: Arc::new(tuliprox_repository::identity_registry::IdentityRegistry::empty(
            std::path::PathBuf::new(),
        )),
        login_throttle: Arc::new(crate::auth::LoginThrottle::new()),
        token_revocations: Arc::new(tuliprox_repository::token_revocations::TokenRevocations::empty(
            std::path::PathBuf::new(),
        )),
        manual_update_sender,
    })
}

fn create_test_fingerprint(addr: std::net::SocketAddr) -> Fingerprint {
    Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr)
}

#[tokio::test]
async fn resource_cache_is_scoped_to_the_authorizing_policy() {
    const CACHED_BODY: &[u8] = b"cached image";
    const UPSTREAM_BODY: &[u8] = b"upstream image";

    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache_dir = temp_dir.path().to_string_lossy();
    let mut cache = LRUResourceCache::new(1024, cache_dir.as_ref());
    let resource_url = "http://1.1.1.1/icon.png";

    // An entry stored under the public-only scope: it must never answer a request authorized by a
    // policy that is allowed to reach private destinations.
    let public_only_scope = resource_cache_key(public_only_policy().digest().as_str(), resource_url);
    let cached_path = cache.store_path(&public_only_scope, Some("image/png"));
    tokio::fs::write(&cached_path, CACHED_BODY).await.expect("write cached image");
    cache.add_content(&public_only_scope, Some("image/png".to_string()), CACHED_BODY.len()).expect("cache entry");
    app_state.cache.store(Some(Arc::new(RwLock::new(cache))));

    let response_head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        UPSTREAM_BODY.len()
    );
    let (upstream_addr, upstream_task) = spawn_legacy_hls_test_origin(response_head, UPSTREAM_BODY.to_vec()).await;
    let proxy = reqwest::Proxy::http(format!("http://{upstream_addr}")).expect("mock proxy URL");
    let mock_client = reqwest::Client::builder().proxy(proxy).build().expect("mock upstream client");

    let public_only = ResolvedResourceAuthorization::public_only();
    let private_policy = Arc::new(
        ResourcePolicy::from_dto(&shared::model::ResourcePolicyDto {
            allowed_hosts: vec!["media.home.arpa".to_string()],
            allowed_networks: vec!["192.168.50.20/32".to_string()],
        })
        .expect("policy"),
    );
    let scoped = ResolvedResourceAuthorization::for_input("private".into(), Some(&private_policy));
    assert_ne!(public_only.policy_digest, scoped.policy_digest);

    let mut clients = HashMap::new();
    for key in [
        ResourceClientKey::new(public_only.policy_digest.clone(), ResourceRedirectMode::Bounded),
        ResourceClientKey::new(scoped.policy_digest.clone(), ResourceRedirectMode::Bounded),
    ] {
        clients.insert(key, mock_client.clone());
    }
    app_state.resource_clients.store(Arc::new(ResourceClientSet::from_clients(clients)));

    let cached_response = resource_response(
        &app_state,
        ResourceFetchOptions::cached(public_only.clone()),
        resource_url,
        &HeaderMap::new(),
        None,
    )
    .await
    .into_response();
    assert_eq!(cached_response.status(), StatusCode::OK);
    let cached_body = cached_response.into_body().collect().await.expect("read cached image").to_bytes();
    assert_eq!(cached_body, Bytes::from_static(CACHED_BODY));

    let fresh_response =
        resource_response(&app_state, ResourceFetchOptions::cached(scoped), resource_url, &HeaderMap::new(), None)
            .await
            .into_response();
    assert_eq!(fresh_response.status(), StatusCode::OK);
    let fresh_body = fresh_response.into_body().collect().await.expect("read upstream image").to_bytes();
    assert_eq!(fresh_body, Bytes::from_static(UPSTREAM_BODY));
    assert_ne!(fresh_body, Bytes::from_static(CACHED_BODY));

    let upstream_request = upstream_task.await.expect("mock upstream task completes");
    assert!(upstream_request.starts_with("GET http://1.1.1.1/icon.png HTTP/1.1\r\n"));
}

#[tokio::test]
async fn resource_response_rejects_blocked_ip_literal_without_a_client() {
    let app_state = create_test_app_state();
    let response = resource_response(
        &app_state,
        ResourceFetchOptions::cached(ResolvedResourceAuthorization::public_only()),
        "http://127.0.0.1/icon.png",
        &HeaderMap::new(),
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn resource_response_rejects_a_policy_without_a_current_client() {
    let app_state = create_test_app_state();
    let response = resource_response(
        &app_state,
        ResourceFetchOptions::cached(ResolvedResourceAuthorization::public_only()),
        "http://1.1.1.1/icon.png",
        &HeaderMap::new(),
        None,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

fn create_test_fingerprint_with_user_agent(addr: std::net::SocketAddr, user_agent: &str) -> Fingerprint {
    Fingerprint::new(format!("{}|{user_agent}", addr.ip()), addr.ip().to_string(), addr)
}

fn create_test_app_state_with_stream_config(stream: crate::model::StreamConfig) -> Arc<AppState> {
    let config = Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        user_access_control: true,
        ..Config::default()
    };

    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    create_test_app_state_for_config(Arc::new(app_cfg))
}

fn create_test_local_channel(url: &str) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id: 41,
        provider_id: 0,
        input_name: "library".intern(),
        item_type: PlaylistItemType::LocalVideo,
        cluster: XtreamCluster::Video,
        group: "Local Movies".intern(),
        title: "Local Test".intern(),
        url: url.into(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn create_test_live_channel(url: &str) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id: 42,
        provider_id: 1,
        input_name: "provider_1".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "Live".intern(),
        title: "Shared Live".intern(),
        url: url.into(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn create_test_session(
    token: &str,
    item_type: PlaylistItemType,
    lifecycle: crate::api::model::PlaybackLifecycle,
) -> UserSession {
    UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::<str>::from("provider-a"),
        stream_url: Arc::<str>::from(match item_type {
            PlaylistItemType::LiveHls => "http://provider-1.example/live/42.m3u8",
            _ => "http://provider-1.example/live/42.ts",
        }),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: "127.0.0.1:55555".parse().unwrap_or_else(|_| unreachable!()),
        socket_bound: item_type.uses_socket_bound_session(),
        active_addrs: Vec::new(),
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle,
    }
}

#[test]
fn classify_playback_request_marks_adaptive_playlist_request_as_prepare() {
    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: None,
        prepare_only: true,
        terminate: false,
    });

    assert_eq!(request_class, PlaybackRequestClass::Prepare);
}

#[test]
fn classify_playback_request_marks_preserved_session_as_activate() {
    let session = create_test_session(
        "tok-preserved",
        PlaylistItemType::LiveHls,
        crate::api::model::PlaybackLifecycle::Preserved,
    );

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(request_class, PlaybackRequestClass::Activate);
}

#[test]
fn classify_playback_request_marks_counted_session_as_follow_up() {
    let session =
        create_test_session("tok-active", PlaylistItemType::LiveHls, crate::api::model::PlaybackLifecycle::Active);

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(request_class, PlaybackRequestClass::FollowUp);
}

/// `PendingProvider` must NOT be classified as `FollowUp`.
/// `PendingProvider` has no counted lease yet — the session is still waiting
/// for a provider slot. A new request on a `PendingProvider` session should
/// be `Activate` so that full admission evaluation happens, not a cheap
/// `FollowUp` skip.
#[test]
fn classify_playback_request_marks_pending_provider_as_activate_not_follow_up() {
    let session = create_test_session(
        "tok-pending",
        PlaylistItemType::LiveHls,
        crate::api::model::PlaybackLifecycle::PendingProvider {
            data: crate::api::model::PendingProviderState {
                reason_code: crate::api::model::PendingProviderReason::GraceHold,
                created_at: 1,
                deadline: 30,
                version: 1,
                wake_source: None,
            },
        },
    );

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(
        request_class,
        PlaybackRequestClass::Activate,
        "PendingProvider should not be FollowUp - it has no counted lease yet"
    );
}

/// `Active` without a counted lease must NOT be classified as `FollowUp`.
/// `FollowUp` should only be returned when the session actually owns a
/// counted admission lease. A session with `Active` lifecycle but no counted
/// lease should go through `Activate` so that the counted lease is reacquired.
#[test]
fn classify_playback_request_marks_active_without_counted_as_activate_not_follow_up() {
    let mut session = create_test_session(
        "tok-active-uncounted",
        PlaylistItemType::LiveHls,
        crate::api::model::PlaybackLifecycle::Active, // counted=false via is_counted()
    );
    // Manually force counted=false by setting to Prepared lifecycle, then restoring
    // Note: is_counted() returns false for Prepared, true for Active
    // For this test we need a session that is Active lifecycle but not counted
    // The new model derives counted from lifecycle, so we must use a different lifecycle
    // to represent "not counted". Use Prepared instead.
    session.lifecycle = crate::api::model::PlaybackLifecycle::Prepared;

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(
        request_class,
        PlaybackRequestClass::Activate,
        "Active session with counted=false should not be FollowUp"
    );
}

/// Prepared sessions must be classified as Activate.
#[test]
fn classify_playback_request_marks_prepared_session_as_activate() {
    let session =
        create_test_session("tok-prepared", PlaylistItemType::LiveHls, crate::api::model::PlaybackLifecycle::Prepared);

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(request_class, PlaybackRequestClass::Activate);
}

/// `GraceActive` without counted lease must NOT be classified as `FollowUp`.
#[test]
fn classify_playback_request_marks_grace_active_without_counted_as_activate() {
    let mut session = create_test_session(
        "tok-grace-uncounted",
        PlaylistItemType::LiveHls,
        crate::api::model::PlaybackLifecycle::Active, // is_counted() = true for GraceActive
    );
    // Test scenario: session has GraceActive lifecycle but we need it NOT counted
    // This represents the edge case before grace task resolves. Use Prepared lifecycle
    // to model "not counted" since is_counted() returns false for Prepared.
    session.lifecycle = crate::api::model::PlaybackLifecycle::Prepared;

    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: Some(&session),
        prepare_only: false,
        terminate: false,
    });

    assert_eq!(
        request_class,
        PlaybackRequestClass::Activate,
        "GraceActive session with counted=false should not be FollowUp"
    );
}

#[tokio::test]
async fn activate_session_before_stream_open_skips_placeholder_for_follow_up_session() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "follow-up-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55220.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55220;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-follow-up",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &fingerprint,
            provider: input.name.clone(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-follow-up"),
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-follow-up",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_none(),
        "follow-up activation must not create a placeholder session"
    );
}

#[tokio::test]
async fn activate_session_before_stream_open_revalidates_precomputed_follow_up_request_class() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "precomputed-follow-up-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55221.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55221;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-precomputed-follow-up",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-precomputed-follow-up",
            request_class: Some(PlaybackRequestClass::FollowUp),
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_some(),
        "precomputed FollowUp must be revalidated against the current uncounted lifecycle"
    );
}

// stale FollowUp revalidation
#[tokio::test]
async fn activate_session_before_stream_open_stale_follow_up_reclassified_on_counted_lease_release() {
    // Scenario: pre-computed FollowUp, but session's counted lease was released before
    // the guard was acquired. Must reclassify to Activate so admission runs.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "stale-followup-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55230.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55230;

    // Session created in Active (counted) state.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-stale-followup",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    // Simulate the counted lease being released before activation:
    // expire the session so it no longer has a counted lease.
    app_state.active_users.terminate_session(&user.username, "tok-stale-followup").await;

    // Call activate with stale FollowUp. Must NOT skip admission — reclassification
    // to Activate must run so the placeholder is created.
    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-stale-followup",
            request_class: Some(PlaybackRequestClass::FollowUp),
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: true,
        },
    )
    .await;

    // Must NOT skip — placeholder must be created since session is expired.
    assert!(
        activation.placeholder_transition_version.is_some(),
        "stale FollowUp with expired session must run admission and create placeholder"
    );
}

// pre-resolved Grace materialization
#[tokio::test]
async fn activate_session_before_stream_open_pre_resolved_grace_period_materializes_pending_provider() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });
    let addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "pre-resolved-grace-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55231.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55231;

    // Session in Prepared state (no grace lifecycle yet).
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-pre-resolved-grace",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    // Call activation with pre-resolved GracePeriod permission.
    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-pre-resolved-grace",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::GracePeriod);
    assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

    let session = app_state.active_users.get_and_update_user_session(&user.username, "tok-pre-resolved-grace").await;
    assert!(
        session.is_some_and(|s| matches!(s.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. })),
        "pre-resolved GracePeriod must materialize as PendingProvider lifecycle"
    );
}

/// `activate_session_before_stream_open` skips placeholder for Prepare class.
#[tokio::test]
async fn activate_session_before_stream_open_skips_placeholder_for_prepare() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55222".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "prepare-user".to_string();
    user.max_connections = 1;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-prepare",
            // Explicitly pass Prepare class — placeholder and admission should be skipped.
            request_class: Some(PlaybackRequestClass::Prepare),
            virtual_id: VirtualId::new(55222),
            item_type: PlaylistItemType::LiveHls,
            stream_url: "http://provider.example/live/test.ts",
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: true,
        },
    )
    .await;

    // Prepare returns Allowed without running admission strategies.
    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_none(),
        "Prepare activation must not create a placeholder session"
    );
}

/// `resolve_playback_request_admission` with `prepare_only = true` returns `Prepare` class.
#[tokio::test]
async fn resolve_playback_request_admission_prepare_only_returns_prepare_class() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });
    let addr: SocketAddr = "127.0.0.1:55223".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "prepare-only-user".to_string();
    user.max_connections = 1;

    let (admission, grace_mode, request_class) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &fingerprint,
        None,
        "tok-prepare-only",
        false,
        EvictionReentryGuard::Session("tok-prepare-only"),
        true,  // prepare_only
        false, // terminate
    )
    .await;

    assert_eq!(request_class, PlaybackRequestClass::Prepare);
    // Prepare returns Allowed without running strategies.
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(grace_mode, None);
}

/// `resolve_playback_request_admission` with `terminate = true` returns `Terminate` class
/// and calls `terminate_session` on the existing session.
#[tokio::test]
async fn resolve_playback_request_admission_terminate_returns_terminate_class() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });
    let addr: SocketAddr = "127.0.0.1:55224".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "terminate-user".to_string();
    user.max_connections = 2;

    // First create a session.
    let session_token = "tok-terminate";
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 55224,
            provider: "test-provider",
            stream_url: "http://provider.example/test.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Verify session exists.
    let before = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
    assert!(before.is_some(), "session should exist before terminate");

    let (admission, grace_mode, request_class) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &fingerprint,
        before.as_ref(),
        session_token,
        false,
        EvictionReentryGuard::Session(session_token),
        false, // prepare_only
        true,  // terminate
    )
    .await;

    assert_eq!(request_class, PlaybackRequestClass::Terminate);
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(grace_mode, None);

    // Session should be expired after terminate.
    let after = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
    assert!(after.is_none(), "session should be removed after terminate");
}

/// `classify_playback_request` returns `Terminate` when `terminate = true`.
#[test]
fn classify_playback_request_returns_terminate_when_flag_set() {
    let request_class = classify_playback_request(PlaybackRequestFacts {
        existing_session: None,
        prepare_only: false,
        terminate: true,
    });
    assert_eq!(request_class, PlaybackRequestClass::Terminate);
}

#[tokio::test]
async fn activate_session_before_stream_open_marks_pending_provider_for_grace_hold() {
    let stream_cfg = crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    };
    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        user_access_control: true,
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream_cfg),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let first_addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "grace-hold-user".to_string();
    user.max_connections = 1;
    let first_channel = create_test_live_channel("http://provider-1.example/live/1.ts");
    let mut second_channel = create_test_live_channel("http://provider-1.example/live/2.m3u8");
    second_channel.item_type = PlaylistItemType::LiveHls;
    second_channel.virtual_id = 55231;

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &first_fingerprint,
            provider: input.name.clone(),
            stream_channel: &first_channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &second_fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-grace-hold",
            request_class: None,
            virtual_id: VirtualId::new(second_channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: second_channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: false,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::GracePeriod);
    assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-grace-hold")
        .await
        .expect("placeholder session should exist");
    let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
        panic!("grace hold should mark pending provider state")
    };
    assert!(matches!(pending.reason_code, crate::api::model::PendingProviderReason::GraceHold));
    assert!(pending.deadline >= pending.created_at);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert!(
        !session.lifecycle.is_counted(),
        "pending provider placeholder must not consume an active user lease before commit"
    );
}

#[tokio::test]
async fn activate_session_before_stream_open_does_not_commit_user_lease_before_provider_success() {
    let stream_cfg = crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: None,
    };
    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        user_access_control: true,
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream_cfg),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let addr: SocketAddr = "127.0.0.1:55232".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "atomic-commit-user".to_string();
    user.max_connections = 1;
    let channel = create_test_live_channel("http://provider-1.example/live/3.ts");

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-atomic-commit",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: channel.item_type,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            socket_bound: false,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.grace_mode, None);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-atomic-commit")
        .await
        .expect("placeholder session should exist");
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "allowed activation should stay provisional until provider acquisition and stream commit succeed"
    );
    assert!(
        !session.lifecycle.is_counted(),
        "placeholder session must stay uncounted until the provider side has been committed"
    );
    assert!(!matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }));
}

fn create_test_shared_target() -> ConfigTarget {
    ConfigTarget {
        id: 1,
        enabled: true,
        name: "shared".to_string(),
        options: Some(ConfigTargetOptions {
            share_live_streams: shared::model::ConfigTargetShareLiveStreams { mpeg_ts: true, ..Default::default() },
            ..ConfigTargetOptions::default()
        }),
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    }
}

#[test]
fn admission_failure_reason_maps_to_custom_video_type() {
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::UserAccountExpired),
        Some(CustomVideoStreamType::UserAccountExpired)
    ));
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::UserConnectionsExhausted),
        Some(CustomVideoStreamType::UserConnectionsExhausted)
    ));
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::ProviderConnectionsExhausted),
        Some(CustomVideoStreamType::ProviderConnectionsExhausted)
    ));
    assert!(admission_failure_video_type(ConnectFailureReason::ProviderError).is_none());
}

#[tokio::test]
async fn effective_admission_strategies_use_legacy_grace_when_field_missing() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: None,
    });

    assert_eq!(
        get_effective_admission_strategies(&app_state.admission_ctx()).as_ref(),
        &[shared::model::AdmissionStrategy::GraceHoldStream][..]
    );
}

#[tokio::test]
async fn effective_admission_strategies_respect_explicit_empty_list() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![]),
    });

    assert!(get_effective_admission_strategies(&app_state.admission_ctx()).is_empty());
}

#[tokio::test]
async fn grace_context_is_populated_when_grace_strategy_is_actually_granted() {
    // Use a DIFFERENT session token than the pre-existing counted session.
    // Otherwise session-admission may treat it as a valid reopen and skip the exhausted path.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserOldest,
        ]),
    });

    let addr1: SocketAddr = "127.0.0.1:55401".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55402".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);
    // addr1 and addr2 have DIFFERENT IPs.
    // EvictUserSameIpOldest will NOT match (different IP), so GraceHoldStream is evaluated.
    let mut user = ProxyUserCredentials::default();
    user.username = "user-grace-ctx".to_string();
    user.max_connections = 1;

    // Register the connection first so update_connection succeeds
    app_state.connection_manager.add_connection(&addr1).await;

    // Create the session — lifecycle starts as Prepared (uncounted)
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-existing-counted",
            virtual_id: 55401,
            provider: "provider_1",
            stream_url: "http://provider-1.example/live/55401.m3u8",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    // update_connection promotes the session to Active (counted) and creates a stream.
    // This exhausts the user's single slot (max_connections = 1).
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55401,
            meter_uid: 55401,
            username: "user-grace-ctx",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider_1".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/55401.m3u8"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-existing-counted"),
        })
        .await
        .expect("stream should be created");

    // Now the new request finds the slot exhausted and the grace strategy kicks in.
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new-request"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new-request"),
        },
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::GracePeriod, "grace should be granted");
    assert!(matches!(result.grace_mode, Some(crate::api::model::GraceMode::Hold)));
    let ctx = result.grace_context.expect("grace_context must be present when grace is granted");
    assert_eq!(ctx.strategy_index, 1, "GraceHoldStream is at index 1");
    assert_eq!(ctx.strategies.len(), 3);
    assert!(matches!(ctx.strategies[ctx.strategy_index], AdmissionStrategy::GraceHoldStream));
}

#[tokio::test]
async fn evaluate_remaining_strategies_evicts_after_used_grace() {
    // Strategies: [GraceHoldStream, EvictUserOldest]
    // Grace was used at index 0, so only EvictUserOldest (index 1) is evaluated.
    // Eviction frees the slot -> Allowed.
    let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
    });

    let addr1: SocketAddr = "127.0.0.1:55701".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55702".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-evict".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 55701,
            provider: "provider-evict",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55701,
            meter_uid: 55701,
            username: "remaining-evict",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-evict".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "remaining-evict",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Normal),
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "EvictUserOldest should free the slot"
    );
    assert!(result.grace_context.is_none(), "no grace context on eviction success");
}

#[tokio::test]
async fn evaluate_remaining_strategies_skips_no_match_and_uses_later_eviction() {
    // Strategies: [GraceHoldStream, EvictUserSameIpOldest, EvictUserOldest]
    // Grace was at index 0, remaining are EvictUserSameIpOldest (index 1) and EvictUserOldest (index 2).
    // The existing counted session is at a DIFFERENT IP, so EvictUserSameIpOldest -> NoMatch.
    // EvictUserOldest succeeds -> Allowed.
    let strategies = vec![
        AdmissionStrategy::GraceHoldStream,
        AdmissionStrategy::EvictUserSameIpOldest,
        AdmissionStrategy::EvictUserOldest,
    ];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserOldest,
        ]),
    });

    let addr1: SocketAddr = "127.0.0.1:55801".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55802".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-skip-no-match".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 55801,
            provider: "provider-skip",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55801,
            meter_uid: 55801,
            username: "remaining-skip-no-match",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-skip".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "remaining-skip-no-match",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Normal),
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "EvictUserSameIpOldest should NoMatch, EvictUserOldest should succeed"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_empty_slice_denies() {
    // Strategies: [GraceHoldStream]
    // Grace was at index 0, remaining slice is empty -> exhausted.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext { strategy_index: 0, strategies: strategies.into(), kind: None };

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.5:55901".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "no-remaining-strategies",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        None,
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted, "empty remaining slice should deny");
}

#[tokio::test]
async fn evaluate_remaining_strategies_preserves_soft_kind_on_exhausted() {
    // Strategies: [GraceHoldStream]
    // Grace was at index 0, remaining slice is empty -> exhausted.
    // grace_context.kind is Soft — must be preserved in the exhausted result.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Soft),
    };

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.6:55902".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "soft-kind-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-soft"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-soft"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Soft),
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted, "empty remaining slice should deny");
    assert_eq!(
        result.admission.kind(),
        Some(crate::api::model::ConnectionKind::Soft),
        "exhausted result must preserve the original Soft connection kind"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_does_not_retry_used_prefix() {
    // Strategies: [GraceHoldStream, GraceInstantStream, EvictUserOldest]
    // Grace was at index 1 (GraceInstantStream).
    // Remaining slice: [EvictUserOldest] (index 2).
    // GraceHoldStream (index 0) must NOT be re-evaluated.
    let strategies = vec![
        AdmissionStrategy::GraceHoldStream,
        AdmissionStrategy::GraceInstantStream,
        AdmissionStrategy::EvictUserOldest,
    ];
    let strategies_for_config = strategies.clone();
    let grace_context = GraceResolutionContext { strategy_index: 1, strategies: strategies.into(), kind: None };

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(strategies_for_config),
    });

    let addr1: SocketAddr = "127.0.0.1:56001".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:56002".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "remaining-no-retry".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted",
            virtual_id: 56001,
            provider: "provider-no-retry",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 56001,
            meter_uid: 56001,
            username: "remaining-no-retry",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-no-retry".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "remaining-no-retry",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new"),
        },
        &grace_context,
        Some(crate::api::model::ConnectionKind::Normal),
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::Allowed,
        "only EvictUserOldest should be evaluated, not GraceHoldStream"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_empty_slice_uses_original_kind_not_context_kind() {
    // grace_context.kind = Normal, original_kind = Soft
    // remaining slice is empty -> exhausted result must use original_kind.
    // This proves the empty-slice branch uses original_kind, not grace_context.kind.
    let strategies = vec![AdmissionStrategy::GraceHoldStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Normal),
    };
    let original_kind = Some(crate::api::model::ConnectionKind::Soft);

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let addr: SocketAddr = "10.0.0.7:55903".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "kind-mismatch-empty",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-empty"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-empty"),
        },
        &grace_context,
        original_kind,
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(
        result.admission.kind(),
        original_kind,
        "exhausted result must use original_kind (Soft), not grace_context.kind (Normal)"
    );
}

#[tokio::test]
async fn evaluate_remaining_strategies_later_grace_uses_original_kind_not_context_kind() {
    // grace_context.kind = Normal, original_kind = Soft
    // Strategies: [GraceHoldStream, GraceInstantStream]
    // Grace was used at index 0 (GraceHoldStream).
    // Remaining slice contains GraceInstantStream (index 1).
    // When the helper returns Grace for the remaining strategy, the new
    // GraceResolutionContext.kind must be original_kind (Soft), not grace_context.kind (Normal).
    // This proves build_grace_ctx uses original_kind as source of truth.
    let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream];
    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: strategies.into(),
        kind: Some(crate::api::model::ConnectionKind::Normal),
    };
    let original_kind = Some(crate::api::model::ConnectionKind::Soft);

    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream]),
    });

    let addr1: SocketAddr = "127.0.0.1:55710".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.8:55711".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);

    app_state.connection_manager.add_connection(&addr1).await;
    app_state.connection_manager.add_connection(&addr2).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "kind-mismatch-grace".to_string();
    user.max_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-counted-grace",
            virtual_id: 55710,
            provider: "provider-grace-kind",
            stream_url: "http://provider.example/live/1.ts",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55710,
            meter_uid: 55710,
            username: "kind-mismatch-grace",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider-grace-kind".intern(),
            stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-counted-grace"),
        })
        .await
        .expect("stream should be created");

    let result = evaluate_remaining_strategies_after_grace(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "kind-mismatch-grace",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new-grace"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new-grace"),
        },
        &grace_context,
        original_kind,
    )
    .await;

    assert_eq!(
        result.admission.permission(),
        UserConnectionPermission::GracePeriod,
        "remaining GraceInstantStream should grant GracePeriod"
    );
    assert!(result.grace_context.is_some(), "grace_context must be present when grace is granted");
    assert_eq!(
        result.grace_context.as_ref().unwrap().kind,
        original_kind,
        "GraceResolutionContext.kind in the result must be original_kind (Soft), not grace_context.kind (Normal)"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_falls_through_after_failed_grace_grant() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
    });

    let first_addr: std::net::SocketAddr = "127.0.0.1:55151".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55152".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "fallthrough".to_string();
    session_user.max_connections = 1;
    session_user.soft_connections = 1;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "tok-first",
            virtual_id: 1,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/1.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;

    assert!(app_state.active_users.grant_grace("fallthrough").await);

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "tok-second",
            virtual_id: 2,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/2.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: crate::api::model::ConnectionKind::Soft,
            priority: 0,
            soft_priority: 10,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/2.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-second"),
        })
        .await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallthrough",
            max_connections: 1,
            soft_connections: 1,
            client_ip: "127.0.0.1",
            request_addr: &"127.0.0.1:55153".parse().unwrap_or_else(|_| unreachable!()),
            use_session_admission: true,
            session_token: Some("tok-third"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-third"),
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(grace_mode, None);
}

#[tokio::test]
async fn resolve_admission_with_strategies_allows_existing_session_even_when_user_is_at_limit() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55154".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "session-admission".to_string();
    user.max_connections = 1;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "vod-session",
            virtual_id: 1,
            provider: "provider-a",
            stream_url: "http://provider-1.example/movie/1.mkv",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/movie/1.mkv"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("vod-session"),
        })
        .await;

    let session_based = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("vod-session"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("vod-session"),
        },
    )
    .await;
    assert_eq!(session_based.admission.permission(), UserConnectionPermission::Allowed);

    let connection_based = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: false,
            session_token: Some("vod-session"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("vod-session"),
        },
    )
    .await;
    assert_eq!(connection_based.admission.permission(), UserConnectionPermission::Exhausted);
}

#[tokio::test]
async fn resolve_admission_with_strategies_prevents_recently_evicted_playback_ping_pong() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55181".parse().unwrap_or_else(|_| unreachable!());
    let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55182".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55183".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9001.ts");
    victim_channel.virtual_id = 9001;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9002.ts");
    winner_channel.virtual_id = 9002;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9001.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9002,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9002.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &reconnect_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("socket-reconnect"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9001) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(admission.rejection_reason(), Some(AdmissionRejectionReason::RecentEvictionReentry));
    assert!(admission.is_reentry_suppressed());
    assert_eq!(grace_mode, None);
    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].channel.virtual_id, 9002);

    // A player may retry the winning plain-TS stream on another socket before the
    // previous HTTP body has drained. That is a replacement of the same playback,
    // not the evicted channel trying to reclaim its slot.
    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&winner_addr, reconnect_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&winner_addr).await;
    app_state.connection_manager.add_connection(&reconnect_addr).await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 3,
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &reconnect_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: None,
        })
        .await;

    let retry_addr: std::net::SocketAddr = "127.0.0.1:55184".parse().unwrap_or_else(|_| unreachable!());
    let retry = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &retry_addr,
            use_session_admission: true,
            session_token: Some("socket-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9002) },
        },
    )
    .await;

    assert_eq!(retry.admission.permission(), UserConnectionPermission::Allowed);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_allows_other_channel_after_recent_eviction() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55184".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55185".parse().unwrap_or_else(|_| unreachable!());
    let new_addr: std::net::SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9101.ts");
    victim_channel.virtual_id = 9101;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9102.ts");
    winner_channel.virtual_id = 9102;
    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user-2".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    // Create sessions before update_connection so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9101,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9101.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9102,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9102.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-2",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-2",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-2",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &new_fingerprint.client_ip,
            request_addr: &new_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-new"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9103) },
        },
    )
    .await;
    let admission = result.admission;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_does_not_suppress_different_session_on_same_channel() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55190".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
    let new_addr: std::net::SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "player/1.0");
    let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
    let mut channel = create_test_live_channel("http://provider-1.example/live/9301.m3u8");
    channel.virtual_id = 9301;
    channel.item_type = PlaylistItemType::LiveHls;
    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user-4".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    // Create sessions before update_connection so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9301,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9301.m3u8",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9301,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9301.m3u8",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-4",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-4",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-4",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &new_fingerprint.client_ip,
            request_addr: &new_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-other"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("session-other"),
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(grace_mode, None);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn resolve_admission_with_strategies_allows_recently_evicted_playback_when_soft_slot_is_free() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
    let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55188".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55189".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9201.ts");
    victim_channel.virtual_id = 9201;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9202.ts");
    winner_channel.virtual_id = 9202;

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-3",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-3",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-3",
            max_connections: 1,
            soft_connections: 1,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &reconnect_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("socket-reconnect"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9201) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
    assert_eq!(grace_mode, None);

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].channel.virtual_id, 9202);
}

#[tokio::test]
async fn local_stream_response_registers_active_local_stream() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let addr = "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };

    let _response = local_stream_response(
        &fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        None,
        None,
        false,
    )
    .await
    .into_response();

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1, "local file streaming should register an active stream");
    assert_eq!(active_streams[0].channel.item_type, PlaylistItemType::LocalVideo);
}

#[tokio::test]
async fn local_stream_response_rechecks_limits_before_registering_socket_bound_streams() {
    let mut app_cfg = create_test_app_config();
    let config = Config { user_access_control: true, ..Config::default() };
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-race-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let first_addr = "127.0.0.1:55131".parse().unwrap_or_else(|_| unreachable!());
    let second_addr = "127.0.0.1:55132".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "local-limit-user".to_string();
    user.max_connections = 1;
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, channel.virtual_id, true);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, channel.virtual_id, true);

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(&first_token),
        None,
        false,
    )
    .await
    .into_response();

    let _second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(&second_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert_eq!(app_state.active_users.active_streams().await.len(), 1);
    assert_eq!(
        app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                &second_token
            )
            .await
            .permission(),
        UserConnectionPermission::Exhausted,
        "failed second open must not leave a placeholder session that bypasses admission"
    );
}

#[tokio::test]
async fn stream_response_preserves_soft_kind_for_shared_reuse() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr = "127.0.0.1:55140".parse().unwrap_or_else(|_| unreachable!());
    let owner_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &owner_addr, 0, crate::api::model::ConnectionKind::Normal)
        .expect("owner allocation should exist");
    let subscriber_id =
        tuliprox_core::model::SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, owner_addr)
            .await
            .expect("shared cleanup admission should succeed");
    let shared_stream = stream::pending::<Result<Bytes, std::io::Error>>();
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &owner_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), owner_handle)),
        pending_cleanup,
        0,
        crate::api::model::ConnectionKind::Normal,
    )
    .await;
    assert!(registered.is_some(), "shared stream should register");

    let mut user = ProxyUserCredentials::default();
    user.username = "soft-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    user.priority = 0;
    user.soft_priority = 9;

    let normal_addr = "127.0.0.1:55141".parse().unwrap_or_else(|_| unreachable!());
    let normal_fingerprint = create_test_fingerprint(normal_addr);
    let normal_channel = create_test_live_channel("http://provider-1.example/live/normal.ts");
    app_state.active_users.add_connection(&normal_addr).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &normal_fingerprint,
            provider: input.name.clone(),
            stream_channel: &normal_channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("normal-session"),
        })
        .await
        .expect("normal stream should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Soft));

    let soft_addr = "127.0.0.1:55142".parse().unwrap_or_else(|_| unreachable!());
    let soft_fingerprint = create_test_fingerprint(soft_addr);
    let response = stream_response(
        &soft_fingerprint,
        &app_state,
        "soft-session",
        None,
        create_test_live_channel(stream_url),
        stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let session_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, "soft-session")
        .await;
    assert_eq!(session_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
}

#[tokio::test]
async fn failed_provider_open_preserves_other_allocation_on_same_socket() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let upstream = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await
    });
    let mut config = create_test_provider_app_config();
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        enabled: true,
        input_type: InputType::Xtream,
        url: format!("http://{upstream}"),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        max_connections: 2,
        ..ConfigInput::default()
    });
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));
    let addr = "127.0.0.1:55144".parse()?;
    let live = app
        .active_provider
        .acquire_connection(&input.name, &addr, 0, crate::api::model::ConnectionKind::Normal)
        .ok_or("live allocation missing")?;
    let url = format!("http://{upstream}/live/user1/pass1/100.ts");
    let channel = create_test_live_channel(&url);
    let details = create_stream_response_details(
        &app,
        &get_stream_options(&app.app_config),
        &url,
        "failing-user",
        &create_test_fingerprint(addr),
        &HeaderMap::new(),
        &input,
        &channel,
        PlaylistItemType::Live,
        crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        false,
        UserConnectionPermission::Allowed,
        None,
        true,
        false,
        VirtualId::new(channel.virtual_id),
        0,
        crate::api::model::ConnectionKind::Normal,
        false,
        Some("failed-session"),
        None,
        false,
        None,
        None,
    )
    .await?;
    assert!(details.provider_handle.is_none());
    assert_eq!(app.active_provider.get_provider_connections_count(), 1);
    assert!(!live.cancel_token.as_ref().is_some_and(tokio_util::sync::CancellationToken::is_cancelled));
    app.active_provider.release_handle(&live);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn ts_direct_capacity_spreads_across_priority_groups_then_rejects_sixth() {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_a".intern(),
        enabled: true,
        input_type: InputType::Xtream,
        url: "http://127.0.0.1:1".to_string(),
        username: Some("user-a".to_string()),
        password: Some("pass-a".to_string()),
        priority: 0,
        max_connections: 2,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_b".intern(),
            url: "http://127.0.0.1:2".to_string(),
            username: Some("user-b".to_string()),
            password: Some("pass-b".to_string()),
            priority: 1,
            max_connections: 3,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));

    let addr: SocketAddr = "127.0.0.1:55120".parse().unwrap_or_else(|_| unreachable!());
    let mut handles = Vec::new();
    for _ in 0..5 {
        let handle = app.active_provider.acquire_connection_with_grace(
            &input.name,
            &addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        );
        assert!(handle.is_some(), "five allocations must succeed against A(2)+B(3)");
        handles.push(handle.expect("allocation present"));
    }

    let sixth = app.active_provider.acquire_connection_with_grace(
        &input.name,
        &addr,
        false,
        0,
        crate::api::model::ConnectionKind::Normal,
    );
    assert!(sixth.is_none(), "sixth start must be rejected once A(2)+B(3) are exhausted");

    let active = app.active_provider.active_connections().expect("active connection map present");
    assert_eq!(active.get("provider_a").copied(), Some(5), "multi lineup must hold five allocations in total");

    let capacities = app.active_provider.provider_capacities_for_input(&input.name);
    let current_of = |name: &str| capacities.iter().find(|(n, _, _)| n.as_ref() == name).map(|(_, cur, _)| *cur);
    assert_eq!(current_of("provider_a"), Some(2), "priority group A must hold two allocations");
    assert_eq!(current_of("provider_b"), Some(3), "priority group B must hold three allocations");

    for handle in &handles {
        app.active_provider.release_handle(handle);
    }
    assert_eq!(app.active_provider.get_provider_connections_count(), 0);
}

#[tokio::test]
async fn stream_response_rolls_back_provisional_user_activation_when_provider_open_fails() {
    let mut app_cfg = create_test_provider_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        user_access_control: true,
        custom_stream_response_enabled: true,
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let addr = "127.0.0.1:55143".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });
    let mut user = ProxyUserCredentials::default();
    user.username = "rollback-user".to_string();
    user.max_connections = 1;
    let stream_url = "provider://bad-url";
    let channel = create_test_live_channel(stream_url);

    let response = stream_response(
        &fingerprint,
        &app_state,
        "rollback-session",
        None,
        channel,
        stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    // Custom-video stream is enabled (`custom_stream_response_enabled: true`
    // in this fixture), so a missing resource must return 400 — the
    // Nginx `proxy_intercept_errors on;` contract requires 4xx so the
    // socket is severed instead of looping on a 200 OK fallback body.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "failed provider open must rollback provisional user activation"
    );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, "rollback-session").await.is_none(),
        "failed provider open must remove the provisional placeholder session"
    );
}

/// Regression test for: when a catchup request fails upstream (e.g. provider returns
/// 4xx/5xx) the connection-slot is released, but the provider account was being
/// pinned via `refresh_provider_reservation` for `catchup_session_ttl_secs`. This
/// blocked other sessions of the same family from acquiring the same provider even
/// though the slot was already free. The fix delegates the pinning decision to
/// `should_pin_provider_for_session` and skips the reservation when the response
/// is a non-Provisioning custom video (failure fallback). Provisioning custom videos
/// must keep their reservation since they represent a successful provider handoff.
#[tokio::test]
async fn should_pin_provider_for_session_skips_reservation_on_failure_custom_video() {
    let app_state = create_test_app_state();
    let no_video_details = StreamDetails {
        shared_subscriber_id: None,
        stream: None,
        stream_info: Some((Vec::new(), StatusCode::OK, None, None)),
        provider_name: Some("provider_1".intern()),
        request_url: None,
        session_headers: None,
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions::default(),
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
    };
    assert!(
        should_pin_provider_for_session(&no_video_details, &app_state, PlaylistItemType::Catchup),
        "a real provider stream (no CustomVideoStreamType) must pin the provider"
    );

    let provisioning_details = StreamDetails {
        shared_subscriber_id: None,
        stream: None,
        stream_info: Some((Vec::new(), StatusCode::OK, None, Some(CustomVideoStreamType::Provisioning))),
        provider_name: Some("provider_1".intern()),
        request_url: None,
        session_headers: None,
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions::default(),
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
    };
    assert!(
        should_pin_provider_for_session(&provisioning_details, &app_state, PlaylistItemType::Catchup),
        "a Provisioning custom video represents a successful provider handoff and must pin"
    );

    for failure_type in [
        CustomVideoStreamType::ChannelUnavailable,
        CustomVideoStreamType::ProviderConnectionsExhausted,
        CustomVideoStreamType::UserConnectionsExhausted,
        CustomVideoStreamType::UserAccountExpired,
        CustomVideoStreamType::LowPriorityPreempted,
    ] {
        let failure_details = StreamDetails {
            shared_subscriber_id: None,
            stream: None,
            stream_info: Some((Vec::new(), StatusCode::BAD_REQUEST, None, Some(failure_type))),
            provider_name: Some("provider_1".intern()),
            request_url: None,
            session_headers: None,
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            grace_period: GracePeriodOptions::default(),
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
            custom_reason: None,
        };
        assert!(
            !should_pin_provider_for_session(&failure_details, &app_state, PlaylistItemType::Catchup),
            "{failure_type:?} is a failure fallback — must NOT pin the provider"
        );
    }
}

#[tokio::test]
async fn local_stream_response_disables_response_compression() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let addr = "127.0.0.1:55124".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };

    let response = local_stream_response(
        &fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        None,
        None,
        false,
    )
    .await
    .into_response();

    assert!(!should_compress_response(&response));
}

#[tokio::test]
async fn local_stream_response_reuses_stable_playback_session_token_across_reopens() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let user = ProxyUserCredentials::default();
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55125".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55126".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let _second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1, "stable playback token should reuse the tracked local connection");
    assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
    assert_eq!(active_streams[0].addr, second_fingerprint.addr);
}

#[tokio::test]
async fn local_stream_response_allows_exhausted_reopen_for_same_playback_session_token() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "user1".to_string();
    user.max_connections = 1;
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55127".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55128".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Exhausted,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(second_response.status(), StatusCode::OK);

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
    assert_eq!(active_streams[0].addr, second_fingerprint.addr);
}

#[tokio::test]
async fn local_stream_response_preserves_soft_kind_across_reopens() {
    let app_state = create_test_app_state();
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file_path = temp_dir.path().join("local-soft-test.mkv");
    tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

    let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
    let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
    let mut user = ProxyUserCredentials::default();
    user.username = "soft-local-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    let target = ConfigTarget {
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    };
    let playback_session_token = "local-soft-playback-token";

    let first_fingerprint = create_test_fingerprint("127.0.0.1:55129".parse().unwrap_or_else(|_| unreachable!()));
    let second_fingerprint = create_test_fingerprint("127.0.0.1:55130".parse().unwrap_or_else(|_| unreachable!()));

    let _first_response = local_stream_response(
        &first_fingerprint,
        &app_state,
        channel.clone(),
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Soft,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    let second_response = local_stream_response(
        &second_fingerprint,
        &app_state,
        channel,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Exhausted,
        crate::api::model::ConnectionKind::Normal,
        Some(playback_session_token),
        None,
        false,
    )
    .await
    .into_response();

    assert_eq!(second_response.status(), StatusCode::OK);

    let session_admission = app_state
        .active_users
        .connection_admission_for_session(
            &user.username,
            user.max_connections,
            user.soft_connections,
            playback_session_token,
        )
        .await;
    assert_eq!(session_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
}

#[tokio::test]
async fn activated_session_admission_keeps_hls_placeholders_uncounted_via_api_utils() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.max_connections = 1;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55178".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-first",
            virtual_id: 7101,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/7101.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-second",
            virtual_id: 7102,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/7102.m3u8",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let first_admission = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &first_fingerprint.client_ip,
            request_addr: &first_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-hls-first"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-hls-first"),
        },
    )
    .await;
    let second_admission = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &second_fingerprint.client_ip,
            request_addr: &second_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-hls-second"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-hls-second"),
        },
    )
    .await;

    assert_eq!(first_admission.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(second_admission.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
}

#[tokio::test]
async fn socket_bound_playback_sessions_enforce_hard_limits_per_socket() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "user1".to_string();
    user.max_connections = 1;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55171".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55172".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 5001, true);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 5001, true);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &first_token,
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/5001.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 5001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/5001.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&first_token),
        })
        .await;

    let admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &second_token)
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_evicts_preserved_hls_session_for_same_user_ts_request() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserSameIpLatest,
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserOldest,
            AdmissionStrategy::EvictUserLatest,
        ]),
    });

    let hls_addr: std::net::SocketAddr = "127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!());
    let ts_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
    let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "player/1.0");
    let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "player/1.0");
    let mut user = ProxyUserCredentials::default();
    user.username = "same-user".to_string();
    user.max_connections = 1;

    app_state.connection_manager.add_connection(&hls_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-preserved",
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/5001.m3u8",
            addr: &hls_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &hls_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel {
                item_type: PlaylistItemType::LiveHls,
                virtual_id: 5001,
                ..create_test_live_channel("http://provider-1.example/live/5001.m3u8")
            },
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-preserved"),
        })
        .await;

    app_state.connection_manager.release_connection(&hls_addr).await;
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
    assert!(app_state.active_users.active_streams().await.is_empty());

    let mut close_rx = app_state.connection_manager.get_close_connection_channel();
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &ts_fingerprint.client_ip,
            request_addr: &ts_fingerprint.addr,
            use_session_admission: false,
            session_token: None,
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(5001) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(grace_mode, None);
    assert!(app_state.active_users.active_streams().await.is_empty());
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(100), close_rx.recv()).await.ok().and_then(Result::ok),
        Some(crate::api::model::CloseConnectionSignal::WithReason(
            hls_addr,
            shared::model::DisconnectReason::ClientKicked,
        ))
    );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, "tok-hls-preserved").await.is_none(),
        "preserved session should be removed once the TS request evicts it"
    );
}

#[tokio::test]
async fn socket_bound_playback_sessions_still_allow_soft_slots() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "soft-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    user.priority = 0;
    user.soft_priority = 9;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55173".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55174".parse().unwrap_or_else(|_| unreachable!());
    let third_addr: std::net::SocketAddr = "127.0.0.1:55175".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 6001, true);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 6001, true);
    let third_fingerprint = create_test_fingerprint(third_addr);
    let third_token = create_session_fingerprint(&third_fingerprint, &user.username, 6001, true);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &first_token,
            virtual_id: 6001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/6001.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 6001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/6001.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&first_token),
        })
        .await;

    let second_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &second_token)
        .await;
    assert_eq!(second_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(second_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &second_token,
            virtual_id: 6001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/6001.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: true,
        })
        .await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 6002,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Soft,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/6002.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&second_token),
        })
        .await;

    let third_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &third_token)
        .await;
    assert_eq!(third_admission.permission(), UserConnectionPermission::Exhausted);
}

#[test]
fn session_based_playback_matches_adaptive_types_and_extensions() {
    assert!(is_session_based_playback(PlaylistItemType::LiveHls, None));
    assert!(is_session_based_playback(PlaylistItemType::LiveDash, None));
    assert!(is_session_based_playback(PlaylistItemType::Live, Some(HLS_EXT)));
    assert!(is_session_based_playback(PlaylistItemType::Live, Some(DASH_EXT)));
    assert!(!is_session_based_playback(PlaylistItemType::Video, None));
}

#[test]
fn create_session_fingerprint_switches_between_logical_and_socket_bound_keys() {
    let fingerprint = create_test_fingerprint("127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!()));
    let logical = create_session_fingerprint(&fingerprint, "user1", 7001, false);
    let socket_bound = create_session_fingerprint(&fingerprint, "user1", 7001, true);

    assert_ne!(logical, socket_bound);
    assert!(logical.contains(&fingerprint.key));
    assert!(socket_bound.contains(&fingerprint.addr.to_string()));
}

#[test]
fn adaptive_playback_session_fingerprint_is_logical_across_initial_sockets() {
    let Some(first_addr) = "127.0.0.1:55177".parse().ok() else {
        return;
    };
    let Some(second_addr) = "127.0.0.1:55178".parse().ok() else {
        return;
    };
    let first = Fingerprint::new("10.0.0.6|player".to_string(), "10.0.0.6".to_string(), first_addr);
    let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

    let first_token = create_playback_session_fingerprint(&first, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));
    let second_token =
        create_playback_session_fingerprint(&second, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));

    assert_eq!(first_token, second_token);
    assert!(first_token.contains(&first.key));
    assert!(!first_token.contains(&first.addr.to_string()));
    assert!(!second_token.contains(&second.addr.to_string()));
}

#[test]
fn playback_session_fingerprint_keeps_ts_socket_bound_but_vod_logical() {
    let first_addr: SocketAddr = "127.0.0.1:55179".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: SocketAddr = "127.0.0.1:55180".parse().unwrap_or_else(|_| unreachable!());
    let first = Fingerprint::new("10.0.0.7|player".to_string(), "10.0.0.7".to_string(), first_addr);
    let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

    let first_ts = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Live, None);
    let second_ts = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Live, None);
    let first_vod = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Video, None);
    let second_vod = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Video, None);

    assert_ne!(first_ts, second_ts, "plain TS live remains socket-bound");
    assert_eq!(first_vod, second_vod, "VOD remains logical across reopen/seek sockets");
}

#[test]
fn catchup_session_key_is_sticky_only_within_the_same_archive_window() {
    let addr: SocketAddr = "127.0.0.1:55181".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = Fingerprint::new("10.0.0.8|player".to_string(), "10.0.0.8".to_string(), addr);

    let first = create_catchup_session_key(&fingerprint, "user1", 7004, "/timeshift/3600/1700000000/");
    let same = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/3600/1700000000");
    let seeked = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/3600/1700000300");
    let resized = create_catchup_session_key(&fingerprint, "user1", 7004, "timeshift/1800/1700000000");

    assert_eq!(first, same, "cosmetic path separators must not break provider stickiness");
    assert_ne!(first, seeked, "seeking must identify the new archive window");
    assert_ne!(first, resized, "changing duration must identify the new archive window");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn xtream_hls_then_ts_uses_distinct_tokens_and_evicts_old_hls_session() {
    let mut app_cfg = create_test_app_config();
    let config = Config {
        user_access_control: true,
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig {
                retry: true,
                metrics_enabled: true,
                buffer: None,
                grace_period_millis: 2_000,
                grace_period_timeout_secs: 8,
                grace_period_hold_stream: true,
                hls_session_ttl_secs: 10,
                catchup_session_ttl_secs: 10,
                throttle_str: None,
                throttle_kbps: 0,
                shared_burst_buffer_mb: 1,
                shared_subscriber_idle_timeout_secs: 300,
                cleanup_queue_capacity: 4096,
                recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
                admission_strategies: Some(vec![
                    AdmissionStrategy::EvictUserSameIpOldest,
                    AdmissionStrategy::EvictUserSameIpLatest,
                    AdmissionStrategy::GraceHoldStream,
                    AdmissionStrategy::EvictUserOldest,
                    AdmissionStrategy::EvictUserLatest,
                ]),
            }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    };
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let hls_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
    let ts_addr: SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
    let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "libmpv");
    let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "libmpv");
    let mut user = ProxyUserCredentials::default();
    user.username = "xtream-hls-ts".to_string();
    user.max_connections = 1;

    let virtual_id = 7811;
    let hls_token = create_session_fingerprint(&hls_fingerprint, &user.username, virtual_id, false);
    let ts_token = create_session_fingerprint(&ts_fingerprint, &user.username, virtual_id, true);
    assert_ne!(hls_token, ts_token, "Xtream .m3u8 and .ts must not share the same playback token");

    let mut hls_channel = create_test_live_channel("http://provider-1.example/live/7811.m3u8");
    hls_channel.virtual_id = virtual_id;
    hls_channel.item_type = PlaylistItemType::LiveHls;
    let mut ts_channel = create_test_live_channel("http://provider-1.example/live/7811.ts");
    ts_channel.virtual_id = virtual_id;

    app_state.connection_manager.add_connection(&hls_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &hls_token,
            virtual_id,
            provider: "provider_1",
            stream_url: hls_channel.url.as_ref(),
            addr: &hls_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &hls_fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &hls_channel,
            user_agent: Cow::Borrowed("libmpv"),
            session_token: Some(&hls_token),
        })
        .await;

    app_state.connection_manager.release_connection(&hls_addr).await;
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "the preserved HLS playback must reserve capacity only virtually"
    );
    assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, &hls_token).await.is_some(),
        "preserved HLS session should still exist before the competing TS request"
    );
    assert_eq!(
        app_state
            .active_users
            .connection_admission(&user.username, user.max_connections, user.soft_connections)
            .await
            .permission(),
        UserConnectionPermission::Exhausted,
        "the preserved HLS playback must still reserve the user's only slot before the TS request is evaluated"
    );
    assert_eq!(
        app_state.active_users.get_eviction_candidates(&user.username, &ts_fingerprint.client_ip).await.len(),
        1,
        "the preserved HLS playback should be the single eviction candidate for the competing TS request"
    );

    let (ts_admission, ts_grace_mode, request_class) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &ts_fingerprint,
        None,
        &ts_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(virtual_id) },
        false,
        false,
    )
    .await;
    assert_eq!(request_class, PlaybackRequestClass::Activate);
    assert_eq!(ts_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(ts_grace_mode, None);
    assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "the competing TS activation must remove the old preserved HLS session even though there is no live socket left to kick"
        );
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "eviction must not leave a real slot before the TS stream commits"
    );
    assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));

    app_state.connection_manager.add_connection(&ts_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &ts_token,
            virtual_id,
            provider: "provider_1",
            stream_url: ts_channel.url.as_ref(),
            addr: &ts_addr,
            connection_permission: ts_admission.permission(),
            connection_kind: ts_admission.kind(),
            socket_bound: true,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &ts_fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &ts_channel,
            user_agent: Cow::Borrowed("libmpv"),
            session_token: Some(&ts_token),
        })
        .await;

    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert_eq!(app_state.active_users.active_users_and_connections().await, (1, 1));
    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams.first().and_then(|stream| stream.session_token.as_deref()), Some(ts_token.as_str()));
    assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "after the competing TS request, the old Xtream HLS session must be gone so later /hls segment fetches cannot revive it"
        );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, &ts_token).await.is_some(),
        "the winning TS playback should remain tracked under its socket-bound Xtream token"
    );
}

#[test]
fn socket_bound_playback_session_matches_only_plain_live_playback() {
    assert!(is_socket_bound_playback_session(PlaylistItemType::Live, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(HLS_EXT)));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(DASH_EXT)));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::LiveHls, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Video, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Series, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Catchup, None));
}

#[test]
fn session_reacquire_cleanup_addrs_excludes_current_and_deduplicates() {
    let primary: SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
    let overlap: SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
    let seek: SocketAddr = "127.0.0.1:55193".parse().unwrap_or_else(|_| unreachable!());
    let session = UserSession {
        token: "tok-vod".to_string(),
        transition_version: 1,
        virtual_id: 9001,
        provider: "provider-a".intern(),
        stream_url: "http://localhost/movie.mkv".intern(),
        provider_session_headers: HashMap::new(),
        user_agent_stream_index: None,
        addr: seek,
        socket_bound: false,
        active_addrs: vec![primary, overlap, seek, overlap],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
    };

    assert_eq!(session_reacquire_cleanup_addrs(&session, &seek), vec![primary, overlap]);
}

#[tokio::test]
async fn intentional_deferred_open_retains_provider_grace_handle() {
    let app_state = create_test_provider_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder occupies the provider slot");
    let input = app_state.app_config.get_input_by_name(&provider_name).expect("provider input");
    let stream_url = "http://provider-1.example/live/user1/pass1/100.m3u8";
    let mut channel = create_test_live_channel(stream_url);
    channel.item_type = PlaylistItemType::LiveHls;
    let fingerprint = create_test_fingerprint(deferred_addr);

    let mut details = create_stream_response_details(
        &app_state,
        &get_stream_options(&app_state.app_config),
        stream_url,
        "deferred-user",
        &fingerprint,
        &HeaderMap::new(),
        &input,
        &channel,
        PlaylistItemType::LiveHls,
        crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        false,
        UserConnectionPermission::Allowed,
        None,
        true,
        true,
        VirtualId::new(channel.virtual_id),
        0,
        crate::api::model::ConnectionKind::Normal,
        false,
        Some("deferred-session"),
        None,
        false,
        Some(true),
        None,
    )
    .await
    .expect("provider grace creates deferred stream details");

    assert!(details.stream.is_none());
    assert!(details.has_deferred_provider_open());
    assert!(details.provider_handle.is_some(), "deferred open must retain its provider allocation");

    app_state.connection_manager.release_managed_provider_handle(details.provider_handle.take());
    app_state.connection_manager.release_provider_handle(Some(holder_handle));
}

#[test]
fn grace_hold_defers_live_and_fresh_video_but_not_catchup_or_affine_reopens() {
    assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::LiveHls, false));
    assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, false));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, false));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, true));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, true));
    assert!(!should_defer_provider_open_for_grace_hold(true, false, PlaylistItemType::Video, true));
}

#[tokio::test]
async fn forced_reopen_cleanup_for_adaptive_streams_does_not_close_client_socket() {
    let app_state = create_test_app_state();
    let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
    let mut close_rx = app_state.connection_manager.get_close_connection_channel();

    cleanup_forced_reopen_addrs(&app_state, "adaptive-owner", &[addr]).await;

    let signal =
        tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
    assert!(signal.is_none(), "adaptive cleanup should not hard-close the previous client socket");
}

#[tokio::test]
async fn forced_reopen_cleanup_for_non_adaptive_streams_preserves_client_socket() {
    let app_state = create_test_app_state();
    let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
    let mut close_rx = app_state.connection_manager.get_close_connection_channel();

    cleanup_forced_reopen_addrs(&app_state, "live-owner", &[addr]).await;

    let signal =
        tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
    assert!(signal.is_none(), "a reopen must not close unrelated requests on the same proxy socket");
}

#[tokio::test]
async fn get_query_path_strips_extension_for_live_with_flag() {
    use crate::model::ConfigInputFlags;
    use shared::model::{InputType, PlaylistItemType, XtreamCluster, XtreamPlaylistItem};

    let mut input = ConfigInput {
        id: 1,
        name: "provider_with_flag".intern(),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let mut options = crate::model::ConfigInputOptions::defaults().clone();
    options.flags.set(ConfigInputFlags::XtreamLiveStreamWithoutExtension);
    input.options = Some(options);

    let sources = SourcesConfig { inputs: vec![Arc::new(input)], ..SourcesConfig::default() };
    let mut app_cfg_raw = create_test_app_config();
    app_cfg_raw.sources = Arc::new(ArcSwap::from_pointee(sources));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg_raw));

    let pli = XtreamPlaylistItem {
        virtual_id: VirtualId::new(100),
        provider_id: 1,
        name: "test".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.com/123".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 0,
        input_name: "provider_with_flag".intern(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "1".intern(),
        upstream_user_agent: None,
    };

    let hls_ext = shared::defaults::HLS_EXT.to_string();
    let (query_path, extension) =
        crate::api::endpoints::xtream_api::get_query_path("", Some(&hls_ext), &pli, &app_state);

    assert_eq!(extension, "");
    assert_eq!(query_path, "1");

    let dash_ext = shared::defaults::DASH_EXT.to_string();
    let (query_path, extension) =
        crate::api::endpoints::xtream_api::get_query_path("", Some(&dash_ext), &pli, &app_state);

    assert_eq!(extension, "");
    assert_eq!(query_path, "1");
}

// =========================================================================================
// evaluate_network_access tests
// =========================================================================================

/// Run `evaluate_network_access` for a synthetic user built from
/// `network_access` and assert the decision matches `expected`. Centralizes
/// the boilerplate (`user_with_network_access` + geoip setup + call +
/// assert) shared by every `evaluate_network_access` test below.
fn assert_network_decision(
    network_access: Option<NetworkAccess>,
    geoip: &Arc<ArcSwapOption<GeoIp>>,
    ip: &str,
    expected: NetworkAccessDecision,
) {
    let user = user_with_network_access(network_access);
    assert_eq!(evaluate_network_access(&user, ip, geoip, GeoIpUnavailablePolicy::Deny), expected);
}

/// `Arc<ArcSwapOption<GeoIp>>` with no `GeoIP` database loaded.
fn empty_geoip() -> Arc<ArcSwapOption<GeoIp>> { Arc::new(ArcSwapOption::<GeoIp>::default()) }

/// `Arc<ArcSwapOption<GeoIp>>` with a mock `GeoIP` that always reports the
/// given country for any lookup.
fn mock_geoip(country: &str) -> Arc<ArcSwapOption<GeoIp>> {
    Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new(country)))))
}

/// Helper to create a test user with specific network access
fn user_with_network_access(network_access: Option<NetworkAccess>) -> ProxyUserCredentials {
    ProxyUserCredentials {
        username: "test".to_string(),
        password: "test".to_string(),
        token: None,
        proxy: ProxyType::default(),
        server: None,
        epg_timeshift: None,
        epg_request_timeshift: None,
        created_at: None,
        exp_date: None,
        max_connections: 0,
        status: None,
        output_clusters: ClusterFlags::all(),
        ui_enabled: true,
        comment: None,
        priority: 0,
        soft_connections: 0,
        soft_priority: 0,
        t_is_api_user: false,
        network_access,
        plan: None,
        filter: None,
        raw_output_clusters: None,
        raw_max_connections: 0,
        raw_soft_connections: 0,
        raw_proxy: Some(ProxyType::default()),
        t_filter: None,
        t_has_unresolved_plan: false,
        t_has_invalid_filter: false,
    }
}

#[test]
fn no_config_allows_all() {
    assert_network_decision(None, &empty_geoip(), "192.168.1.1", NetworkAccessDecision::Allowed);
}

#[test]
fn empty_config_allows_all() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec![] }),
        &empty_geoip(),
        "192.168.1.1",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn cidr_match_allows() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "192.168.1.42",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn cidr_miss_denies() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "10.0.0.1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn country_match_allows() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &mock_geoip("DE"),
        "8.8.8.8",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn country_miss_denies() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &mock_geoip("US"),
        "8.8.8.8",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch),
    );
}

#[test]
fn no_geoip_denies_on_country_restriction() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
        &empty_geoip(),
        "8.8.8.8",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable),
    );
}

#[test]
fn ipv4_vs_ipv6_denies_gracefully() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["2001:db8::/32".parse().unwrap()] }),
        &empty_geoip(),
        "192.168.1.1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn ipv6_vs_ipv4_denies_gracefully() {
    assert_network_decision(
        Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["192.168.1.0/24".parse().unwrap()] }),
        &empty_geoip(),
        "2001:db8::1",
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
    );
}

#[test]
fn either_cidr_or_country_match_allows() {
    assert_network_decision(
        Some(NetworkAccess {
            allowed_countries: vec!["US".to_string()],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }),
        &mock_geoip("DE"),
        "192.168.1.42",
        NetworkAccessDecision::Allowed,
    );
}

#[test]
fn single_ip_cidr() {
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec![],
        allowed_networks: vec!["192.168.1.1/32".parse().unwrap()],
    }));
    let geoip = empty_geoip();
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.2", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
    );
}

// =========================================================================================
// network denied reason tests
// =========================================================================================

// The three `network_denied_reason_*` cases remain because they cover the
// `NetworkAccessDenyReason`-focused API surface directly, while
// `cidr_miss_denies`, `country_miss_denies`, and
// `no_geoip_denies_on_country_restriction` above assert the same deny
// reasons through broader `evaluate_network_access(...)` behavior. The
// overlap is intentional so both the general decision path and the
// reason-reporting-focused path stay pinned by tests.

#[test]
fn network_denied_reason_country_unknown_when_geoip_loaded_but_unknown_ip() {
    // GeoIP is loaded (not None), but lookup returns None for this IP (private/unknown).
    // We need a GeoIP that only covers a private range, so public IPs get None.
    // Use a CIDR-only restriction (no country rules) so we can verify
    // that when countries ARE checked, lookup None gives "country_unknown".
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()], // miss CIDR first
    }));
    // Use the real GeoIp::new() which only seeds private ranges.
    // For 8.8.8.8 (public), lookup returns None.
    let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
    // CIDR miss -> country check -> geoip loaded but lookup returns None
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown)
    );
}

#[test]
fn network_denied_reason_none_when_allowed() {
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("DE")))));
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
}

#[test]
fn network_denied_reason_none_when_no_config() {
    let user = user_with_network_access(None);
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    assert_eq!(
        evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
}

// =========================================================================================
// GeoIP unavailable policy tests
// =========================================================================================

#[test]
fn geoip_unavailable_default_deny_denies() {
    // Country rule exists but GeoIP is unavailable — default policy is Deny
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable));
}

#[test]
fn geoip_unavailable_explicit_allow_allows() {
    // Country rule exists, GeoIP unavailable, but policy is Allow — allows
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::AllowedGeoIpUnavailable);
}

#[test]
fn geoip_unavailable_cidr_only_still_denies() {
    // CIDR only rules, no match — should deny even with Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec![],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch));
}

#[test]
fn geoip_unavailable_cidr_match_allows() {
    // CIDR match always allows, regardless of policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["10.0.0.0/8".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Deny);
    assert_eq!(decision, NetworkAccessDecision::Allowed);
}

#[test]
fn geoip_loaded_country_mismatch_still_denies() {
    // Loaded GeoIP but country doesn't match — should deny under Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("US")))));
    let decision = evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch));
}

#[test]
fn geoip_loaded_unknown_country_still_denies() {
    // Loaded GeoIP but lookup returns None — should deny under Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
    }));
    let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
    let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown));
}

#[test]
fn malformed_ip_denies_even_when_geoip_unavailable_policy_is_allow() {
    let user = user_with_network_access(Some(NetworkAccess::from(&shared::model::NetworkAccessDto {
        allowed_countries: Some(vec!["DE".to_string()]),
        allowed_networks: None,
    })));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());

    let decision = evaluate_network_access(&user, "not-an-ip", &geoip, GeoIpUnavailablePolicy::Allow);

    assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::MalformedClientIp));
}

#[test]
fn evaluate_network_access_respects_allow_policy() {
    // verify evaluate_network_access returns AllowedGeoIpUnavailable with Allow policy
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    // Default deny policy should return Denied
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable)
    );
    // Allow policy should return AllowedGeoIpUnavailable
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow),
        NetworkAccessDecision::AllowedGeoIpUnavailable
    );
}

#[tokio::test]
async fn test_channel_unavailable_fallback_does_not_register_body_owner_or_leak_slot() {
    let app_state = create_test_provider_app_state();
    let provider_name = "provider_1".intern();
    let mut cfg = (**app_state.app_config.config.load()).clone();
    cfg.custom_stream_response_enabled = true;
    app_state.app_config.config.store(std::sync::Arc::new(cfg));
    let addr: std::net::SocketAddr = "127.0.0.1:49080".parse().unwrap();

    let custom_video = crate::model::CustomStreamResponse {
        channel_unavailable: Some(crate::api::model::TransportStreamBuffer::new(b"channel-unavailable-bytes".to_vec())),
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    };
    app_state.app_config.custom_stream_response.store(Some(std::sync::Arc::new(custom_video)));

    let handle = app_state
        .active_provider
        .acquire_connection(&provider_name, &addr, 0, tuliprox_session::ConnectionKind::Normal)
        .expect("connection should be acquired");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let (channel_unavail_stream, info) =
        create_channel_unavailable_stream(&app_state.app_config, &[], axum::http::StatusCode::OK);
    assert!(channel_unavail_stream.is_some());

    let factory_response = tuliprox_session::ProviderStreamFactoryResponse {
        stream: channel_unavail_stream.unwrap(),
        info,
        provider_session_headers: std::collections::HashMap::new(),
        has_upstream_owner: false,
    };

    if factory_response.has_upstream_owner {
        app_state.active_provider.register_body_owner(handle.allocation_id);
    }

    app_state.active_provider.release_handle(&handle);

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert_eq!(app_state.active_provider.active_connections().map(|m| m.values().sum::<usize>()).unwrap_or(0), 0);
}

#[tokio::test]
async fn test_provider_open_cancelled_during_header_wait_releases_slot_without_leak() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    let accepted = std::sync::Arc::new(tokio::sync::Notify::new());
    let accepted_clone = std::sync::Arc::clone(&accepted);

    let server_task = tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            accepted_clone.notify_waiters();
            let mut buf = [0u8; 1024];
            loop {
                use tokio::io::AsyncReadExt;
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    });

    let app_state = create_test_provider_app_state();
    let provider_name = "provider_1".intern();
    let client_addr: std::net::SocketAddr = "127.0.0.1:49081".parse().unwrap();

    let handle = app_state
        .active_provider
        .acquire_connection(&provider_name, &client_addr, 0, tuliprox_session::ConnectionKind::Normal)
        .expect("connection should be acquired");

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);

    let url = url::Url::parse(&format!("http://{local_addr}/stream.ts")).unwrap();
    let req_headers = axum::http::HeaderMap::new();
    let stream_options = get_stream_options(&app_state.app_config);
    let mut options =
        crate::api::model::ProviderStreamFactoryOptions::new(&crate::api::model::ProviderStreamFactoryParams {
            addr: client_addr,
            item_type: PlaylistItemType::Live,
            share_stream: false,
            stream_options: &stream_options,
            stream_url: &url,
            req_headers: &req_headers,
            input_headers: None,
            session_headers: None,
            disabled_headers: None,
            default_user_agent: None,
            username: None,
            client_ip: None,
            stream_channel: None,
            connect_failure_stage: None,
            content_representation: tuliprox_session::ProviderContentRepresentationMode::PreserveOrigin,
        });
    options.set_provider_handle_tokens(
        handle.cancel_token.clone(),
        handle.completion_token.clone(),
        Some(handle.close_reason.clone()),
    );
    app_state.active_provider.mark_opening(handle.allocation_id);

    let ctx = app_state.provider_stream_ctx();
    let client = app_state.http_client.load().as_ref().clone();

    let open_task =
        tokio::spawn(async move { crate::api::model::create_provider_stream(&ctx, &client, options).await });

    // Wait until TCP connection is accepted and waiting for response headers
    accepted.notified().await;

    // Now cancel the handle while it is waiting for response headers
    handle.cancel_token.as_ref().unwrap().cancel();

    // The open task should finish quickly because it is cancelled
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), open_task)
        .await
        .expect("open task should abort upon cancellation")
        .expect("task join");
    assert!(outcome.is_none());

    // Release the handle
    app_state.active_provider.release_handle(&handle);

    // Yield to allow background completion tasks to process
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(app_state.active_provider.get_provider_connections_count(), 0);
    assert_eq!(app_state.active_provider.active_connections().map(|m| m.values().sum::<usize>()).unwrap_or(0), 0);

    let _ = server_task.await;
}

#[tokio::test]
async fn test_reentry_suppression_response_behavior() {
    let response = crate::api::api_utils::reentry_suppressed_response();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!response.headers().contains_key("x-tuliprox-rejection"));
    assert!(!response.headers().contains_key("content-type"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_eviction_strategy_falls_back_when_candidate_protected_by_reentry() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55301".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: SocketAddr = "127.0.0.1:55302".parse().unwrap_or_else(|_| unreachable!());
    let other_ip_addr: SocketAddr = "10.0.0.1:55303".parse().unwrap_or_else(|_| unreachable!());
    let retry_addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());

    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let other_ip_fingerprint = create_test_fingerprint_with_user_agent(other_ip_addr, "other/1.0");
    let retry_fingerprint = create_test_fingerprint_with_user_agent(retry_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9501.ts");
    victim_channel.virtual_id = 9501;
    let winner_channel = create_test_live_channel("http://provider-1.example/live/9502.ts");
    let other_channel = create_test_live_channel("http://provider-1.example/live/9503.ts");

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "fallback-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;
    app_state.connection_manager.add_connection(&other_ip_addr).await;

    // Create user sessions so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9501,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9501.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9502,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9502.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-other",
            virtual_id: 9503,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9503.ts",
            addr: &other_ip_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Stream 0: victim on same IP (127.0.0.1), channel 9501
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 99,
            username: "fallback-user",
            max_connections: 3,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;

    // Stream 1: winner on same IP (127.0.0.1), channel 9502
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    // Stream 2: stream on different IP (10.0.0.1), channel 9503
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &other_ip_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &other_channel,
            user_agent: std::borrow::Cow::Borrowed("other/1.0"),
            session_token: Some("session-other"),
        })
        .await;

    // Mark winner_addr as protected against reentry of victim_addr (channel 9501)
    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    // Retry for channel 9501 arrives from same IP (127.0.0.1).
    // EvictUserSameIpOldest will target winner_addr (same IP), but winner_addr is protected!
    // It should be suppressed, filtered out, and Fall B will continue to EvictUserOldest.
    // EvictUserOldest will then target other_ip_addr (which is NOT protected).
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            client_ip: &retry_fingerprint.client_ip,
            request_addr: &retry_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9501) },
        },
    )
    .await;

    // Request must be Admitted because the fallback strategy evicted the unprotected stream!
    assert_eq!(result.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(result.admission.rejection_reason(), None);

    // Now test rejection when NO other candidate exists (only the protected stream).
    // EvictUserSameIpOldest suppresses winner_addr; EvictUserOldest finds no candidates left.
    let result_exhausted = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallback-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &retry_fingerprint.client_ip,
            request_addr: &retry_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry-2"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9501) },
        },
    )
    .await;

    assert_eq!(result_exhausted.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(result_exhausted.admission.rejection_reason(), Some(AdmissionRejectionReason::RecentEvictionReentry));
    assert!(result_exhausted.admission.is_reentry_suppressed());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_mixed_suppressed_and_legitimate_eviction_reports_exhaustion() {
    // A protected candidate is skipped, then a legitimate (but uncounted) candidate is
    // evicted and admission still fails. That residual failure is real exhaustion and
    // must not be mislabelled as `RecentEvictionReentry`.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55321".parse().unwrap_or_else(|_| unreachable!());
    let protected_addr: SocketAddr = "127.0.0.1:55322".parse().unwrap_or_else(|_| unreachable!());
    let orphan_addr: SocketAddr = "10.0.0.1:55323".parse().unwrap_or_else(|_| unreachable!());
    let request_addr: SocketAddr = "127.0.0.1:55324".parse().unwrap_or_else(|_| unreachable!());

    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let protected_fingerprint = create_test_fingerprint_with_user_agent(protected_addr, "protected/1.0");
    let orphan_fingerprint = create_test_fingerprint_with_user_agent(orphan_addr, "orphan/1.0");
    let request_fingerprint = create_test_fingerprint_with_user_agent(request_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9701.ts");
    victim_channel.virtual_id = 9701;
    let mut protected_channel = create_test_live_channel("http://provider-1.example/live/9702.ts");
    protected_channel.virtual_id = 9702;
    let mut orphan_channel = create_test_live_channel("http://provider-1.example/live/9703.ts");
    orphan_channel.virtual_id = 9703;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "mixed-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&protected_addr).await;
    app_state.connection_manager.add_connection(&orphan_addr).await;

    // The guard source: a counted stream on the requested channel (9701). It supplies the
    // socket reentry key and is released before the retry is resolved.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9701,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9701.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 91,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;

    // The protected candidate: counted, same IP as the retry, different channel.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-protected",
            virtual_id: 9702,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9702.ts",
            addr: &protected_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 92,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &protected_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &protected_channel,
            user_agent: std::borrow::Cow::Borrowed("protected/1.0"),
            session_token: Some("session-protected"),
        })
        .await;

    // The legitimate fallback candidate: an orphan stream (no session), so evicting it
    // frees no counted connection and admission stays exhausted.
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 93,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &orphan_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &orphan_channel,
            user_agent: std::borrow::Cow::Borrowed("orphan/1.0"),
            session_token: None,
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, protected_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &request_fingerprint.client_ip,
            request_addr: &request_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9701) },
        },
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(result.admission.rejection_reason(), Some(AdmissionRejectionReason::UserConnectionsExhausted));
    assert!(!result.admission.is_reentry_suppressed());
}

fn connection_denied_count(app_state: &Arc<AppState>) -> u64 {
    app_state
        .active_users
        .events()
        .stats()
        .emitted()
        .into_iter()
        .find_map(|(kind, count)| (kind == shared::model::EventKind::ConnectionDenied).then_some(count))
        .unwrap_or(0)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_reentry_suppression_does_not_emit_connection_denied() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55331".parse().unwrap_or_else(|_| unreachable!());
    let protected_addr: SocketAddr = "127.0.0.1:55332".parse().unwrap_or_else(|_| unreachable!());
    let request_addr: SocketAddr = "127.0.0.1:55333".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let protected_fingerprint = create_test_fingerprint_with_user_agent(protected_addr, "protected/1.0");
    let request_fingerprint = create_test_fingerprint_with_user_agent(request_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9711.ts");
    victim_channel.virtual_id = 9711;
    let mut protected_channel = create_test_live_channel("http://provider-1.example/live/9712.ts");
    protected_channel.virtual_id = 9712;

    let mut user = ProxyUserCredentials::default();
    user.username = "event-reentry-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&protected_addr).await;

    for (token, virtual_id, url, addr, fingerprint, channel, meter_uid) in [
        (
            "session-victim",
            9711,
            "http://provider-1.example/live/9711.ts",
            &victim_addr,
            &victim_fingerprint,
            &victim_channel,
            201_u32,
        ),
        (
            "session-protected",
            9712,
            "http://provider-1.example/live/9712.ts",
            &protected_addr,
            &protected_fingerprint,
            &protected_channel,
            202_u32,
        ),
    ] {
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id,
                provider: "provider-a",
                stream_url: url,
                addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid,
                username: "event-reentry-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint,
                provider: "provider-a".intern(),
                stream_channel: channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(token),
            })
            .await;
    }

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, protected_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let (admission, _, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &request_fingerprint,
        None,
        "session-event",
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9711) },
        false,
        false,
    )
    .await;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert!(admission.is_reentry_suppressed());
    assert_eq!(connection_denied_count(&app_state), 0, "a suppressed reentry must not emit ConnectionDenied");
    assert_eq!(
        app_state.active_users.reentry_suppressed_total(),
        1,
        "a suppressed reentry must be counted once as a diagnostic"
    );
}

#[tokio::test]
async fn test_real_exhaustion_emits_connection_denied() {
    // Negative control: a genuine connection-limit denial must still emit the event.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![]),
    });

    let addr: SocketAddr = "127.0.0.1:55341".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint_with_user_agent(addr, "player/1.0");
    let mut user = ProxyUserCredentials::default();
    user.username = "event-limit-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "session-limit",
            virtual_id: 9721,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9721.ts",
            addr: &addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    // A stream row is what makes the session count towards `max_connections`.
    let channel = create_test_live_channel("http://provider-1.example/live/9721.ts");
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 211,
            username: "event-limit-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-limit"),
        })
        .await;

    let (admission, _, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &fingerprint,
        None,
        "session-new",
        false,
        EvictionReentryGuard::Session("session-new"),
        false,
        false,
    )
    .await;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(admission.rejection_reason(), Some(AdmissionRejectionReason::UserConnectionsExhausted));
    assert_eq!(connection_denied_count(&app_state), 1, "a real limit denial must emit ConnectionDenied");
    assert_eq!(
        app_state.active_users.reentry_suppressed_total(),
        0,
        "a real limit denial must not be counted as reentry suppression"
    );
}
