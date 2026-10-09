use super::{
    create_stream_response_details, create_test_app_state, create_test_app_state_for_config, create_test_fingerprint,
    create_test_live_channel, create_test_provider_app_config, create_test_provider_app_state, get_stream_options,
    should_pin_provider_for_session, StreamResponseMode,
};
use crate::{
    api::model::{CustomVideoStreamType, StreamDetails},
    model::{ConfigInput, ConfigInputAlias, GracePeriodOptions, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::http::{HeaderMap, StatusCode};
use shared::{
    model::{InputType, PlaylistItemType, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::{io::AsyncWriteExt, net::TcpListener};

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
        &get_stream_options(&app.app_config, StreamResponseMode::Stream),
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
async fn recording_provider_reservation_opens_without_second_slot() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let upstream = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata").await
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
        max_connections: 1,
        ..ConfigInput::default()
    });
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app = create_test_app_state_for_config(Arc::new(config));
    let reserved =
        app.active_provider.acquire_connection_for_download(&input.name, 0).ok_or("recording reservation missing")?;
    let allocation_id = reserved.allocation_id;
    let claimed = app
        .active_provider
        .claim_download_connection(allocation_id, &input.name)
        .ok_or("recording reservation claim missing")?;
    let managed = tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app.active_provider), claimed);
    let addr = "127.0.0.1:55145".parse()?;
    let url = format!("http://{upstream}/live/user1/pass1/100.ts");
    let channel = create_test_live_channel(&url);
    let details = create_stream_response_details(
        &app,
        &get_stream_options(&app.app_config, StreamResponseMode::Stream),
        &url,
        "recording-user",
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
        Some("recording-session"),
        None,
        false,
        None,
        None,
        None,
        Some(managed),
    )
    .await?;

    assert!(details.stream.is_some(), "the claimed reservation must open the provider body");
    assert!(details.custom_reason.is_none(), "the request must not fall back to ProviderExhausted");
    assert_eq!(app.active_provider.get_provider_connections_count(), 1, "recording consumes exactly one slot");

    drop(details);
    app.active_provider.complete_release(allocation_id);
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
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions::default(),
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
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
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions::default(),
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
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
            provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
            user_agent_stream_index: None,
            grace_period: GracePeriodOptions::default(),
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
            custom_reason: None,
            response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
            session_registration: None,
        };
        assert!(
            !should_pin_provider_for_session(&failure_details, &app_state, PlaylistItemType::Catchup),
            "{failure_type:?} is a failure fallback — must NOT pin the provider"
        );
    }
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
        &get_stream_options(&app_state.app_config, StreamResponseMode::Stream),
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
        None,
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
