use super::{
    create_test_app_state_for_config, create_test_dual_provider_app_config, create_test_fingerprint,
    create_test_provider_app_config, needs_initial_stalker_resolution, resolve_streaming_strategy,
    should_refresh_stalker_playback, stalker_stream_kind, StreamingAcquireOptions,
};
use crate::{
    api::model::{ProviderStreamCustomReason, ProviderStreamState},
    model::SourcesConfig,
};
use axum::http::StatusCode;
use shared::{
    model::{InputType, PlaylistItemType, StalkerStreamKind, XtreamCluster},
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc};

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
            capacity_wait_timeout: None,
        },
        None,
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
            capacity_wait_timeout: None,
        },
        None,
    )
    .await;

    assert!(matches!(
        strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::UnmappedProviderUrl, .. }
    ));

    app_state.active_provider.release_connection(&busy_addr);
    app_state.active_provider.release_connection(&fallback_addr);
}
