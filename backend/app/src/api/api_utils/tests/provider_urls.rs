use super::{
    create_test_app_config, create_test_app_state_for_config, create_test_dual_provider_app_config,
    create_test_dual_provider_app_state, create_test_fingerprint, create_test_live_channel, create_test_local_channel,
    create_test_provider_app_config, create_test_shared_target, find_input_account_by_signature,
    force_provider_stream_response, get_stream_alternative_url, load_test_user, resolve_redirect_location,
    resolve_streaming_strategy, resolve_xtream_vod_provider_url, select_provider_stream_url,
    spawn_legacy_hls_test_origin, spawn_range_aware_test_origin, stream_url_matches_provider,
    ForceStreamRequestContext, StreamingAcquireOptions,
};
use crate::{
    api::model::{ProviderConfig as RuntimeProviderConfig, ProviderConfigConnection, ProviderStreamState, UserSession},
    model::{AppConfig, ConfigInput, ConfigInputAlias, ConfigProvider, ProxyUserCredentials, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::{
    model::{
        ConfigProviderDto, InputType, PlaylistItem, PlaylistItemHeader, PlaylistItemType, ProviderUrlSelectionPolicy,
        ProxyType, UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

pub(in crate::api::api_utils::tests) fn test_runtime_provider(
    url: &str,
    username: &str,
    password: &str,
) -> Arc<RuntimeProviderConfig> {
    test_runtime_provider_with_type(url, username, password, InputType::Xtream)
}

pub(in crate::api::api_utils::tests) fn test_runtime_provider_with_type(
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

pub(in crate::api::api_utils::tests) fn test_runtime_provider_without_credentials(
    url: &str,
    input_type: InputType,
) -> Arc<RuntimeProviderConfig> {
    let input = ConfigInput { name: "provider".intern(), url: url.to_string(), input_type, ..ConfigInput::default() };
    Arc::new(RuntimeProviderConfig::new(
        &input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ))
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

pub(in crate::api::api_utils::tests) async fn independent_stream_token_alias_fixture(
) -> (tempfile::TempDir, Arc<AppConfig>, ConfigInput, Arc<RuntimeProviderConfig>) {
    independent_stream_token_alias_fixture_with(&["http://stream.example:4000/323/mono.m3u8?token=backup-stream-token"])
        .await
}

/// Primary and backup M3U accounts whose stream tokens are independent of their playlist
/// keys; only the backup alias playlist with `alias_urls` is persisted.
pub(in crate::api::api_utils::tests) async fn independent_stream_token_alias_fixture_with(
    alias_urls: &[&str],
) -> (tempfile::TempDir, Arc<AppConfig>, ConfigInput, Arc<RuntimeProviderConfig>) {
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
        channels: alias_urls
            .iter()
            .enumerate()
            .map(|(index, url)| PlaylistItem {
                header: PlaylistItemHeader {
                    id: format!("channel-323-{index}").intern(),
                    input_stream_id: format!("channel-323-{index}").intern(),
                    url: (*url).intern(),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    ..PlaylistItemHeader::default()
                },
            })
            .collect(),
        xtream_cluster: XtreamCluster::Live,
    }];
    persist_input_m3u_playlist(&app_config, &playlist_path, &playlist).await.expect("alias playlist should persist");
    (temp, app_config, input, alias)
}

#[tokio::test]
async fn select_provider_stream_url_uses_persisted_alias_url_for_independent_stream_token() {
    let (_temp, app_config, input, alias) = independent_stream_token_alias_fixture().await;

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

#[tokio::test]
async fn select_provider_stream_url_maps_flussonic_archive_to_alias_stream_token() {
    let (_temp, app_config, input, alias) = independent_stream_token_alias_fixture().await;

    for archive_file in ["archive-1791225557-14400.m3u8", "mono-1791225557-14400.m3u8", "timeshift_abs-1791225557.ts"] {
        let selected = select_provider_stream_url(
            &format!("http://stream.example:4000/323/{archive_file}?token=primary-stream-token"),
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
                format!("http://stream.example:4000/323/{archive_file}?token=backup-stream-token"),
            )),
            "{archive_file}"
        );
    }
}

#[tokio::test]
async fn select_provider_stream_url_prefers_archive_source_live_file() {
    let (_temp, app_config, input, alias) = independent_stream_token_alias_fixture_with(&[
        "http://stream.example:4000/323/index.m3u8?token=index-token",
        "http://stream.example:4000/323/mono.ts?token=mono-ts-token",
        "http://stream.example:4000/323/mono.m3u8?token=mono-hls-token",
    ])
    .await;

    for (archive_file, token) in [
        ("mono-1791225557-14400.m3u8", "mono-hls-token"),
        ("mono-1791225557-14400.ts", "mono-ts-token"),
        ("index-1791225557-14400.m3u8", "index-token"),
    ] {
        let selected = select_provider_stream_url(
            &format!("http://stream.example:4000/323/{archive_file}?token=primary-stream-token"),
            &input,
            &alias,
            false,
            &app_config,
        )
        .await;

        assert_eq!(
            selected.map(|(_, url)| url),
            Some(format!("http://stream.example:4000/323/{archive_file}?token={token}")),
            "{archive_file}"
        );
    }
}

#[tokio::test]
async fn select_provider_stream_url_matches_uppercase_alias_live_file_for_archives_only() {
    let (_temp, app_config, input, alias) = independent_stream_token_alias_fixture_with(&[
        "http://stream.example:4000/323/MONO.M3U8?token=backup-stream-token",
        "http://stream.example:4000/324/INDEX.m3u8?token=upper-token",
        "http://stream.example:4000/324/index.m3u8?token=lower-token",
    ])
    .await;

    for (requested, expected) in [
        // A Flussonic archive finds its live file in any case.
        (
            "http://stream.example:4000/323/mono-1791225557-14400.m3u8?token=primary-stream-token",
            "http://stream.example:4000/323/mono-1791225557-14400.m3u8?token=backup-stream-token",
        ),
        // Exact lookups keep case-sensitive paths of other providers distinct.
        (
            "http://stream.example:4000/324/INDEX.m3u8?token=primary-stream-token",
            "http://stream.example:4000/324/INDEX.m3u8?token=upper-token",
        ),
        (
            "http://stream.example:4000/324/index.m3u8?token=primary-stream-token",
            "http://stream.example:4000/324/index.m3u8?token=lower-token",
        ),
    ] {
        let selected = select_provider_stream_url(requested, &input, &alias, false, &app_config).await;
        assert_eq!(selected.map(|(_, url)| url).as_deref(), Some(expected), "{requested}");
    }
}

#[tokio::test]
async fn select_provider_stream_url_keeps_unindexed_non_archive_url() {
    let (_temp, app_config, input, alias) = independent_stream_token_alias_fixture().await;
    let segment = "http://stream.example:4000/323/tracks-v1a1/dvr-2026/10/05/18/26/seg.ts?token=backup-stream-token";

    let selected = select_provider_stream_url(segment, &input, &alias, false, &app_config).await;

    assert_eq!(selected, Some(("backup-account".intern(), segment.to_string())));
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
fn xtream_vod_provider_url_requires_stored_input_identity() -> Result<(), Box<dyn std::error::Error>> {
    let config = create_test_dual_provider_app_config();
    let input = config.sources.load().inputs.first().cloned().ok_or("missing input")?;
    let provider = RuntimeProviderConfig::new(
        &input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    );
    let stream_url = "http://cdn.example/r2/movie.mp4";
    let mut channel = create_test_live_channel(stream_url);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.provider_id = 100;
    assert_eq!(resolve_xtream_vod_provider_url(stream_url, &input, &provider, &channel), Some(stream_url.to_string()));
    assert_eq!(
        resolve_xtream_vod_provider_url("http://unrelated.example/movie.mp4", &input, &provider, &channel),
        None
    );
    channel.input_name = "other-input".intern();
    assert_eq!(resolve_xtream_vod_provider_url(stream_url, &input, &provider, &channel), None);
    channel.input_name = Arc::clone(&input.name);
    channel.item_type = PlaylistItemType::Live;
    assert_eq!(resolve_xtream_vod_provider_url(stream_url, &input, &provider, &channel), None);
    channel.item_type = PlaylistItemType::Video;
    channel.provider_id = 0;
    assert_eq!(resolve_xtream_vod_provider_url(stream_url, &input, &provider, &channel), None);
    channel.provider_id = 100;
    channel.url = "file:///internal/movie.mp4".intern();
    assert_eq!(resolve_xtream_vod_provider_url(&channel.url, &input, &provider, &channel), None);
    let unrelated = test_runtime_provider("http://unrelated.example", "other-user", "other-pass");
    channel.url = stream_url.intern();
    assert_eq!(resolve_xtream_vod_provider_url(stream_url, &input, &unrelated, &channel), None);
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn xtream_vod_m3u_reverse_proxies_external_sources_and_alias_redirects_without_leaking_urls(
) -> Result<(), Box<dyn std::error::Error>> {
    for use_alias in [false, true] {
        for source_path in ["/r2/movie.mp4", "/stream/account-bound-token"] {
            let (cdn_addr, cdn_task) = spawn_legacy_hls_test_origin(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/12\r\nAccept-Ranges: bytes\r\nReferer: http://private-provider.example/user/pass\r\nContent-Location: http://private-cdn.example/movie.mp4\r\nLocation: http://private-cdn.example/movie.mp4\r\nLink: <http://private-cdn.example/movie.mp4>\r\nConnection: close\r\n\r\n".to_string(),
                b"DATA".to_vec(),
            ).await;
            let cdn_url = format!("http://{cdn_addr}/internal/media.mp4?token=private-cdn-token");
            let (redirect_addr, redirect_task) = spawn_legacy_hls_test_origin(
                format!("HTTP/1.1 302 Found\r\nLocation: {cdn_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                Vec::new(),
            )
            .await;
            let config = create_test_dual_provider_app_config();
            let mut input = (**config.sources.load().inputs.first().ok_or("missing input")?).clone();
            let alias = input.aliases.as_mut().and_then(|aliases| aliases.first_mut()).ok_or("missing alias")?;
            alias.url = format!("http://{redirect_addr}");
            let input = Arc::new(input);
            config
                .sources
                .store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
            let app_state = create_test_app_state_for_config(Arc::new(config));
            let busy_addr = SocketAddr::from(([127, 0, 0, 1], 55401));
            let busy = if use_alias {
                app_state.active_provider.acquire_exact_connection_with_grace(
                    &input.name,
                    &busy_addr,
                    false,
                    0,
                    crate::api::model::ConnectionKind::Normal,
                )
            } else {
                None
            };
            assert!(!use_alias || busy.is_some());
            let source_url = format!("http://{redirect_addr}{source_path}");
            let item = shared::model::M3uPlaylistItem::from(&PlaylistItem {
                header: PlaylistItemHeader {
                    id: "900".intern(),
                    input_stream_id: "100".intern(),
                    virtual_id: VirtualId::new(100),
                    input_name: Arc::clone(&input.name),
                    url: source_url.intern(),
                    item_type: PlaylistItemType::Video,
                    xtream_cluster: XtreamCluster::Video,
                    additional_properties: Some(shared::model::StreamProperties::Video(Box::new(
                        shared::model::VideoStreamProperties {
                            container_extension: "mp4".intern(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                },
            });
            let mut target = create_test_shared_target();
            target.options = None;
            target.output = vec![tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput::from(
                &shared::model::M3uTargetOutputDto::default(),
            ))];
            let mut user = load_test_user("vod-reverse-viewer");
            user.proxy = ProxyType::Reverse(None);
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, HeaderValue::from_static("bytes=4-7"));
            let response = crate::api::endpoints::m3u_api::m3u_api_stream_loaded(
                Arc::new(user),
                Arc::new(target),
                &create_test_fingerprint(SocketAddr::from(([127, 0, 0, 1], 55402))),
                &headers,
                &app_state,
                item,
                Arc::clone(&input),
                Some("mp4"),
                None,
                None,
            )
            .await
            .into_response();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers().get(header::CONTENT_RANGE), Some(&HeaderValue::from_static("bytes 4-7/12")));
            for name in ["location", "content-location", "referer", "link"] {
                assert!(!response.headers().contains_key(name));
            }
            for value in response.headers().values() {
                let value = value.to_str()?;
                assert!(!value.contains(&redirect_addr.to_string()));
                assert!(!value.contains(&cdn_addr.to_string()));
                assert!(!value.contains("private-cdn-token"));
                assert!(!value.contains("user2/pass2"));
            }
            let body = response.into_body().collect().await?.to_bytes();
            assert_eq!(body.as_ref(), b"DATA");
            let redirect_request = tokio::time::timeout(Duration::from_secs(5), redirect_task).await??;
            let cdn_request = tokio::time::timeout(Duration::from_secs(5), cdn_task).await??;
            let expected_path = if use_alias { "/movie/user2/pass2/100.mp4" } else { source_path };
            assert!(redirect_request.starts_with(&format!("GET {expected_path} ")));
            assert!(cdn_request.starts_with("GET /internal/media.mp4?token=private-cdn-token "));
            assert!(redirect_request.to_ascii_lowercase().contains("range: bytes=4-7"));
            assert!(cdn_request.to_ascii_lowercase().contains("range: bytes=4-7"));
            app_state.active_provider.release_connection(&busy_addr);
        }
    }
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
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
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
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
            capacity_wait_timeout: None,
        },
        None,
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
