use super::{test_app_config_with_storage_dir, AppConfig, ConfigInput, InputType};
use crate::{
    api::{
        endpoints::hls_api::{known_binding_fetch_url, resolve_hls_origin_fetch_url, strip_profile_base_path},
        model::ProviderConfig as RuntimeProviderConfig,
    },
    model::ConfigProvider,
};
use shared::{
    model::{
        ConfigProviderDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        ProviderUrlSelectionPolicy, XtreamCluster,
    },
    utils::Internable,
};
use std::sync::Arc;
use tuliprox_repository::{get_input_m3u_playlist_file_path, get_input_storage_path, persist_input_m3u_playlist};

const ROOT_PROFILE_URLS: [&str; 2] = ["http://pl.example.com", "http://eu.example.com"];

fn expand_at(input: &ConfigInput, provider_url: &str, index: usize) -> String {
    let profile = input.get_resolve_provider(provider_url).expect("playback profile should resolve");
    let (_, expanded) =
        tuliprox_core::model::resolve_provider_scheme_url_with_provider_index(provider_url, Some(profile), index)
            .expect("provider scheme url should resolve");
    expanded.into_owned()
}

/// Builds a temp storage dir with a root and an alias M3U account that share the
/// `cdn` playback profile; the alias account indexes `alias_urls`.
async fn cdn_alias_fixture(
    profile_urls: &[&str],
    alias_urls: &[&str],
) -> (tempfile::TempDir, Arc<AppConfig>, ConfigInput, Arc<RuntimeProviderConfig>, Arc<RuntimeProviderConfig>) {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let app_config = test_app_config_with_storage_dir(temp.path());

    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "cdn".into(),
        urls: profile_urls.iter().map(|&url| url.into()).collect(),
        provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let root_input = ConfigInput {
        name: "root-account".intern(),
        input_type: InputType::M3u,
        url: "http://playlist.example.com/list.m3u?access_key=fixture".into(),
        playback_provider: Some("cdn".into()),
        provider_configs: Some(vec![provider]),
        ..ConfigInput::default()
    };
    let alias_input = ConfigInput {
        name: "alias-account".intern(),
        input_type: InputType::M3u,
        url: "http://alias.example.com/list.m3u?access_key=fixture".into(),
        playback_provider: Some("cdn".into()),
        ..ConfigInput::default()
    };
    let root_account = Arc::new(RuntimeProviderConfig::new(
        &root_input,
        Arc::new(std::sync::RwLock::new(crate::api::model::ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ));
    let alias_account = Arc::new(RuntimeProviderConfig::new(
        &alias_input,
        Arc::new(std::sync::RwLock::new(crate::api::model::ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ));

    let storage_path = get_input_storage_path(&alias_account.name, &app_config.config.load().storage_dir)
        .await
        .expect("alias storage should be created");
    let playlist_path = get_input_m3u_playlist_file_path(&storage_path, &alias_account.name);
    let channels = alias_urls
        .iter()
        .enumerate()
        .map(|(index, url)| PlaylistItem {
            header: PlaylistItemHeader {
                id: format!("ch-{index}").intern(),
                input_stream_id: format!("ch-{index}").intern(),
                url: (*url).intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        })
        .collect();
    let playlist = vec![PlaylistGroup { id: 1, title: "Live".intern(), channels, xtream_cluster: XtreamCluster::Live }];
    persist_input_m3u_playlist(&app_config, &playlist_path, &playlist).await.expect("alias playlist should persist");

    (temp, app_config, root_input, root_account, alias_account)
}

/// The alias token indexed under the first mirror replaces the root token; the profile authority stays.
#[tokio::test]
async fn alias_hit_pl_candidate() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &ROOT_PROFILE_URLS,
        &["http://pl.example.com/ChannelA/mono.m3u8?token=alias-token-pl&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-pl&keep=1".to_string()));
}

/// The alias token indexed under the second mirror is found as well.
#[tokio::test]
async fn alias_hit_eu_candidate() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &ROOT_PROFILE_URLS,
        &["http://eu.example.com/ChannelA/mono.m3u8?token=alias-token-eu&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-eu&keep=1".to_string()));
}

/// A Flussonic archive URL takes the token of the alias's indexed live URL.
#[tokio::test]
async fn archive_sibling() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &ROOT_PROFILE_URLS,
        &["http://pl.example.com/ChannelA/mono.m3u8?token=alias-token-live&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono-1784898000-60.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(
        result,
        Some("provider://cdn/ChannelA/mono-1784898000-60.m3u8?token=alias-token-live&keep=1".to_string())
    );
}

/// An alias without an indexed URL yields no URL instead of the root token.
#[tokio::test]
async fn alias_missing_fails_closed() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(&ROOT_PROFILE_URLS, &[]).await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, None);
}

/// The root account keeps the entry URL.
#[tokio::test]
async fn root_unchanged() {
    let (_temp, app_config, input, root, _alias) = cdn_alias_fixture(&ROOT_PROFILE_URLS, &[]).await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&root)).await;
    assert_eq!(result, Some(entry.to_string()));
}

/// Without a playback profile the entry URL is not rewritten.
#[tokio::test]
async fn unbound_input_unchanged() {
    let (_temp, app_config, mut input, _root, alias) = cdn_alias_fixture(&ROOT_PROFILE_URLS, &[]).await;
    input.playback_provider = None;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some(entry.to_string()));
}

/// A known alias binding without a resolved URL fails instead of reusing the root-token entry URL.
#[tokio::test]
async fn known_alias_binding_without_url_does_not_fall_back_to_root_entry() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(&ROOT_PROFILE_URLS, &[]).await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token";
    let resolved = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(known_binding_fetch_url(resolved, &input, &alias.name, entry), None);
}

/// Playback profile URLs with a base path do not duplicate the base path in the resolved provider URL.
#[tokio::test]
async fn alias_hit_with_base_path() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &["http://pl.example.com/media", "http://eu.example.com/media"],
        &["http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-pl&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-pl&keep=1".to_string()));
    let resolved = result.as_deref().expect("result should be Some");
    assert_eq!(
        expand_at(&input, resolved, 0),
        "http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-pl&keep=1"
    );
}

/// A base path with a trailing slash is stripped equivalently at the segment boundary.
#[tokio::test]
async fn alias_hit_with_base_path_trailing_slash() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &["http://pl.example.com/media/", "http://eu.example.com/media/"],
        &["http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-pl&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-pl&keep=1".to_string()));
    let resolved = result.as_deref().expect("result should be Some");
    assert_eq!(
        expand_at(&input, resolved, 0),
        "http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-pl&keep=1"
    );
}

/// A Flussonic archive URL preserves the live URL token without duplicating the profile base path.
#[tokio::test]
async fn archive_sibling_with_base_path() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &["http://pl.example.com/media", "http://eu.example.com/media"],
        &["http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-live&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono-1784898000-60.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(
        result,
        Some("provider://cdn/ChannelA/mono-1784898000-60.m3u8?token=alias-token-live&keep=1".to_string())
    );
    let resolved = result.as_deref().expect("result should be Some");
    assert_eq!(
        expand_at(&input, resolved, 0),
        "http://pl.example.com/media/ChannelA/mono-1784898000-60.m3u8?token=alias-token-live&keep=1"
    );
}

/// A playback profile with mixed bases correctly resolves an alias indexed under a non-base-path mirror.
#[tokio::test]
async fn mixed_bases() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &["http://pl.example.com/media", "http://eu.example.com"],
        &["http://eu.example.com/ChannelA/mono.m3u8?token=alias-token-eu&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-eu&keep=1".to_string()));
    let resolved = result.as_deref().expect("result should be Some");
    assert_eq!(expand_at(&input, resolved, 1), "http://eu.example.com/ChannelA/mono.m3u8?token=alias-token-eu&keep=1");
    assert_eq!(
        expand_at(&input, resolved, 0),
        "http://pl.example.com/media/ChannelA/mono.m3u8?token=alias-token-eu&keep=1"
    );
}

/// An alias hit on the second mirror strips that mirror's base path without duplication.
#[tokio::test]
async fn second_index_with_base_path() {
    let (_temp, app_config, input, _root, alias) = cdn_alias_fixture(
        &["http://pl.example.com/media", "http://eu.example.com/media"],
        &["http://eu.example.com/media/ChannelA/mono.m3u8?token=alias-token-eu&keep=1"],
    )
    .await;
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token&keep=1";
    let result = resolve_hls_origin_fetch_url(&app_config, &input, entry, entry, Some(&alias)).await;
    assert_eq!(result, Some("provider://cdn/ChannelA/mono.m3u8?token=alias-token-eu&keep=1".to_string()));
    let resolved = result.as_deref().expect("result should be Some");
    assert_eq!(
        expand_at(&input, resolved, 1),
        "http://eu.example.com/media/ChannelA/mono.m3u8?token=alias-token-eu&keep=1"
    );
}

/// The root account keeps the entry URL fallback; a resolved URL always wins.
#[test]
fn known_root_binding_keeps_the_entry_url_fallback() {
    let input = ConfigInput { name: "root-account".intern(), ..ConfigInput::default() };
    let entry = "provider://cdn/ChannelA/mono.m3u8?token=root-token";
    assert_eq!(known_binding_fetch_url(None, &input, "root-account", entry), Some(entry.to_string()));
    let resolved = Some("http://pl.example.com/x.m3u8".to_string());
    assert_eq!(known_binding_fetch_url(resolved.clone(), &input, "alias-account", entry), resolved);
}

#[test]
fn strip_profile_base_path_cases() {
    // Root bases return path unchanged.
    assert_eq!(strip_profile_base_path("http://pl.example.com", "/media/ch"), Some("/media/ch"));
    assert_eq!(strip_profile_base_path("http://pl.example.com/", "/media/ch"), Some("/media/ch"));
    assert_eq!(strip_profile_base_path("http://pl.example.com///", "/media/ch"), Some("/media/ch"));
    assert_eq!(strip_profile_base_path("pl.example.com", "/media/ch"), Some("/media/ch"));

    // Base with path stripped at segment boundary.
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/media/ch"), Some("/ch"));
    assert_eq!(strip_profile_base_path("http://pl.example.com/media/", "/media/ch"), Some("/ch"));
    assert_eq!(strip_profile_base_path("pl.example.com/media", "/media/ch"), Some("/ch"));
    assert_eq!(strip_profile_base_path("http://pl.example.com/media/sub", "/media/sub/ch"), Some("/ch"));

    // Remainder empty or slash.
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/media"), Some(""));
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/media/"), Some("/"));

    // Segment boundary mismatch must return None.
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/mediaX/ch"), None);
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/media-test/ch"), None);
    assert_eq!(strip_profile_base_path("http://pl.example.com/media", "/other/ch"), None);

    // Malformed base returns None without panic.
    assert_eq!(strip_profile_base_path("http://", "/media/ch"), None);
}
