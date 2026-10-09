use super::test_fingerprint;
use crate::{
    api::model::{
        build_proxy_session_id, HlsAccessLeaseId, ProviderConfig as RuntimeProviderConfig, ProviderConfigConnection,
    },
    model::{ConfigInput, ConfigProvider},
};
use shared::model::{ConfigProviderDto, InputType, ProviderUrlSelectionPolicy};
use std::sync::Arc;

#[test]
fn live_hls_entry_tokens_are_unique_but_share_a_stable_provider_owner() {
    let fingerprint = test_fingerprint();
    let first = super::super::hls_entry_user_session_token(&fingerprint, "alice", 42, None, None);
    let second = super::super::hls_entry_user_session_token(&fingerprint, "alice", 42, None, None);

    assert_ne!(first, second);
    assert!(first.contains("|hls|"));
    assert!(second.contains("|hls|"));
    let first_lease = tuliprox_session::PlaybackLeaseRef::new(&first, tuliprox_core::model::PlaybackKind::LiveHls);
    let second_lease = tuliprox_session::PlaybackLeaseRef::new(&second, tuliprox_core::model::PlaybackKind::LiveHls);
    assert_ne!(first_lease.owner, second_lease.owner);
    assert_eq!(first_lease.provider_owner(), second_lease.provider_owner());
    assert_ne!(first_lease.request_id, second_lease.request_id);
    let other_channel = super::super::hls_entry_user_session_token(&fingerprint, "alice", 43, None, None);
    let other_user = super::super::hls_entry_user_session_token(&fingerprint, "bob", 42, None, None);
    assert_ne!(
        first_lease.provider_owner(),
        tuliprox_session::PlaybackLeaseRef::new(&other_channel, first_lease.kind).provider_owner()
    );
    assert_ne!(
        first_lease.provider_owner(),
        tuliprox_session::PlaybackLeaseRef::new(&other_user, first_lease.kind).provider_owner()
    );
}

#[test]
fn hls_manifest_materialization_uses_proxy_paths_without_provider_or_legacy_route() {
    let body = format!(
        "#EXTM3U\n#EXTINF:4.0,\n/hls/shared/live/proxy-id/{}/000123.ts\n",
        crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
    );
    let lease_id = HlsAccessLeaseId("access-lease".to_string());

    let materialized = super::super::materialize_hls_access_manifest(&body, &lease_id, Some("/iptv"));

    assert!(materialized.contains("/iptv/hls/shared/live/proxy-id/access-lease/000123.ts"));
    assert!(!materialized.contains("provider://"));
    assert!(!materialized.contains("/hls/hls-user/"));
    assert!(!materialized.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
}

#[test]
fn hls_cache_origin_entry_url_preserves_provider_scheme_as_failover() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://origin.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input = ConfigInput { provider_configs: Some(vec![Arc::clone(&provider)]), ..ConfigInput::default() };

    let origin =
        super::super::resolve_hls_cache_origin_entry_url(&input, "provider://demo/live/account-a/token-a/1025130.m3u8")
            .expect("provider entry url should resolve");

    assert_eq!(origin.session_entry_url.as_str(), "provider://demo/live/account-a/token-a/1025130.m3u8");
    assert_eq!(
        origin.session_entry_url.url_failover_provider().expect("provider failover config").name.as_ref(),
        "demo"
    );
    let provider_key = super::super::build_hls_origin_source(&input, "1025130").session_key();
    let direct_key = super::super::build_hls_origin_source(&input, "1025130").session_key();
    assert_eq!(provider_key, direct_key);
    assert_eq!(provider_key.stable_value(), "input:0|hls|1025130");
    assert!(!provider_key.stable_value().contains("provider://"));
    assert!(!provider_key.stable_value().contains("origin.example.com"));
}

#[test]
fn hls_cache_origin_entry_url_does_not_attach_url_failover_provider_to_http_url() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://origin.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input = ConfigInput { provider_configs: Some(vec![provider]), ..ConfigInput::default() };

    let origin = super::super::resolve_hls_cache_origin_entry_url(
        &input,
        "http://origin.example.com/live/account-a/token-a/1025130.m3u8",
    )
    .expect("http entry url should resolve");

    assert_eq!(origin.session_entry_url.as_str(), "http://origin.example.com/live/account-a/token-a/1025130.m3u8");
    assert!(origin.session_entry_url.url_failover_provider().is_none());
}

#[test]
fn hls_origin_resolution_keeps_provider_failover_out_of_identity() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "mirror-group".into(),
        urls: vec!["http://mirror-a.example.com".into(), "http://mirror-b.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input = ConfigInput {
        id: 7,
        name: Arc::from("xtream"),
        input_type: InputType::Xtream,
        url: "provider://mirror-group".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        provider_configs: Some(vec![Arc::clone(&provider)]),
        ..ConfigInput::default()
    };

    let origin = super::super::build_hls_origin_resolution(
        &input,
        "provider://mirror-group/live/source-user/source-pass/1025126.m3u8",
    )
    .expect("provider failover origin should resolve");
    let failover_key = super::super::build_hls_origin_source(&input, "80510").session_key();
    let direct_key = super::super::build_hls_origin_source(&input, "80510").session_key();

    assert_eq!(origin.session_entry_url.as_str(), "provider://mirror-group/live/source-user/source-pass/1025126.m3u8");
    assert!(origin.session_entry_url.url_failover_provider().is_some());
    assert_eq!(failover_key, direct_key);
    assert!(!failover_key.stable_value().contains("provider://"));
    assert!(!failover_key.stable_value().contains("mirror-a.example.com"));
    assert!(!failover_key.stable_value().contains("mirror-b.example.com"));
}

#[test]
fn provider_failover_mirror_change_keeps_same_hls_session_identity() {
    let provider_a = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "mirror-group".into(),
        urls: vec!["http://mirror-a.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let provider_b = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "mirror-group".into(),
        urls: vec!["http://mirror-b.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input_a = ConfigInput {
        id: 7,
        name: Arc::from("xtream"),
        input_type: InputType::Xtream,
        url: "provider://mirror-group".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        provider_configs: Some(vec![provider_a]),
        ..ConfigInput::default()
    };
    let input_b = ConfigInput { provider_configs: Some(vec![provider_b]), ..input_a.clone() };

    let origin_a = super::super::build_hls_origin_resolution(&input_a, "provider://mirror-group/a.m3u8")
        .expect("provider failover origin a should resolve");
    let origin_b = super::super::build_hls_origin_resolution(&input_b, "provider://mirror-group/b.m3u8")
        .expect("provider failover origin b should resolve");
    let key_a = super::super::build_hls_origin_source(&input_a, "80510").session_key();
    let key_b = super::super::build_hls_origin_source(&input_b, "80510").session_key();
    let secret = b"rewrite-secret";

    assert!(origin_a.session_entry_url.url_failover_provider().is_some());
    assert!(origin_b.session_entry_url.url_failover_provider().is_some());
    assert_eq!(key_a, key_b);
    assert_eq!(key_a.stable_value(), "input:7|hls|80510");
    assert_eq!(build_proxy_session_id(&key_a, secret), build_proxy_session_id(&key_b, secret));
    assert!(!key_a.stable_value().contains("provider://"));
    assert!(!key_a.stable_value().contains("mirror-a.example.com"));
    assert!(!key_a.stable_value().contains("mirror-b.example.com"));
}

#[test]
fn hls_runtime_origin_fetch_url_uses_selected_provider_account() {
    let input = ConfigInput {
        name: Arc::from("source"),
        url: "http://source.example.com".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        ..ConfigInput::default()
    };
    let provider_input = ConfigInput {
        id: 7,
        name: Arc::from("selected-provider"),
        url: "http://provider.example.com".to_string(),
        username: Some("provider-user".to_string()),
        password: Some("provider-pass".to_string()),
        input_type: InputType::Xtream,
        max_connections: 1,
        ..ConfigInput::default()
    };
    let provider = Arc::new(RuntimeProviderConfig::new(
        &provider_input,
        Arc::new(std::sync::RwLock::new(ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    ));

    let fetch_url = super::super::build_hls_origin_fetch_url(
        &input,
        "http://source.example.com/live/source-user/source-pass/12345.m3u8",
        "http://source.example.com/live/source-user/source-pass/12345.m3u8",
        Some(&provider),
    )
    .expect("fetch url should be rewritten");

    assert_eq!(fetch_url, "http://provider.example.com/live/provider-user/provider-pass/12345.m3u8");

    let failover_provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://mirror-a.example.com".into(), "http://mirror-b.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let provider_scheme_input = ConfigInput {
        name: Arc::from("source"),
        url: "provider://demo".to_string(),
        username: Some("source-user".to_string()),
        password: Some("source-pass".to_string()),
        input_type: InputType::Xtream,
        provider_configs: Some(vec![Arc::clone(&failover_provider)]),
        ..ConfigInput::default()
    };
    let provider_scheme_without_account_rewrite = super::super::build_hls_origin_fetch_url(
        &provider_scheme_input,
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        None,
    )
    .expect("provider scheme fetch url should remain failover capable");
    assert_eq!(provider_scheme_without_account_rewrite, "provider://demo/live/source-user/source-pass/12345.m3u8");

    let provider_scheme_fetch_url = super::super::build_hls_origin_fetch_url(
        &provider_scheme_input,
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        Some(&provider),
    )
    .expect("provider scheme fetch url should use selected runtime account without losing failover");

    assert_eq!(provider_scheme_fetch_url, "provider://demo/live/provider-user/provider-pass/12345.m3u8");
    let origin_entry = super::LiveHlsOriginEntry::parse_with_provider_configs(
        &provider_scheme_without_account_rewrite,
        Some(Arc::clone(&failover_provider)),
        Some(Arc::clone(&provider)),
    )
    .expect("provider origin entry");
    let input_source = origin_entry.to_input_source();
    assert_eq!(input_source.url, provider_scheme_without_account_rewrite);
    assert_eq!(input_source.username.as_deref(), Some("provider-user"));
    assert_eq!(input_source.password.as_deref(), Some("provider-pass"));

    let failover_context = super::super::hls_url_failover_provider_for_origin_context(
        &provider_scheme_input,
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        "provider://demo/live/source-user/source-pass/12345.m3u8",
        &provider_scheme_fetch_url,
    )
    .expect("provider failover context");
    assert_eq!(failover_context.name.as_ref(), "demo");
}

#[test]
fn hls_origin_entry_attaches_url_failover_provider_only_to_provider_scheme_fetch_url() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "demo".into(),
        urls: vec!["http://mirror.example.com".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));

    let http_provider = super::super::effective_hls_url_failover_provider_for_fetch_url(
        "http://provider.example.com/live/user/pass/12345.m3u8",
        None,
        Some(Arc::clone(&provider)),
    );
    let http_entry = super::LiveHlsOriginEntry::parse_with_url_failover_provider(
        "http://provider.example.com/live/user/pass/12345.m3u8",
        http_provider,
    )
    .expect("http origin entry");
    assert!(http_entry.url_failover_provider().is_none());

    let provider_scheme_provider = super::super::effective_hls_url_failover_provider_for_fetch_url(
        "provider://demo/live/user/pass/12345.m3u8",
        None,
        Some(Arc::clone(&provider)),
    );
    let provider_entry = super::LiveHlsOriginEntry::parse_with_url_failover_provider(
        "provider://demo/live/user/pass/12345.m3u8",
        provider_scheme_provider,
    )
    .expect("provider origin entry");
    assert_eq!(provider_entry.url_failover_provider().expect("provider").name.as_ref(), "demo");
}
