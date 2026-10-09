use super::{
    get_stream_channel, hls_qos_meter_init, m3u_archive_epg_reference_ts, m3u_catchup_epg_reference_from_session_token,
};
use crate::{
    api::{
        api_utils::connection_priority_for_kind,
        model::{AppState, ConnectionHistoryMode, UserSession},
    },
    auth::Fingerprint,
};
use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};
use shared::{
    model::{PlaylistItemType, StreamChannel, StreamInfo, XtreamCluster},
    utils::{is_m3u_catchup_session_token, Internable},
};
use std::{borrow::Cow, sync::Arc};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    HlsAccessContext, HlsOriginAccountBinding, HlsOriginSource, HlsQosRuntimeConfig, HlsSessionHandle, ProxySessionId,
};

pub(in crate::api::endpoints::hls_api) async fn ensure_hls_cache_stream_registered(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    access: &HlsAccessContext,
    session: &HlsSessionHandle,
) -> Option<StreamInfo> {
    let (proxy_session_id, origin_source, origin_account_binding) = {
        let session = session.read().await;
        if session.is_gc_marked_for_removal() {
            return None;
        }
        (session.proxy_session_id.clone(), session.origin_source.clone(), session.origin_account_binding.clone())
    };
    let user = app_state.app_config.get_user_credentials(&access.username)?;
    let user_session =
        app_state.active_users.get_and_update_user_session(&access.username, &access.user_session_token).await?;
    let connection_kind = user_session.connection_kind?;
    let priority = connection_priority_for_kind(&user, connection_kind);
    let mut stream_channel = build_hls_cache_stream_channel(app_state, access, &origin_source, &proxy_session_id).await;
    let provider = hls_cache_stats_provider(&origin_source, origin_account_binding.as_ref(), &user_session);
    let user_agent = req_headers
        .get(header::USER_AGENT)
        .map_or_else(|| Cow::Borrowed(""), |value| String::from_utf8_lossy(value.as_bytes()));

    stream_channel.url = Arc::from(hls_cache_stream_stats_url(&proxy_session_id));
    // Panel Streams/History read this item_type. Shared HLS transport is still HLS, but
    // archive/catchup leases must never be published as Live/LiveHls.
    let panel_archive_reference = origin_source
        .archive_reference
        .or(access.epg_reference_ts)
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token))
        .or(stream_channel.epg_reference_ts);
    let is_archive_playback = panel_archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || origin_source.archive_reference.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
        || stream_channel.item_type == PlaylistItemType::Catchup;
    if is_archive_playback {
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Video;
        stream_channel.epg_reference_ts = panel_archive_reference;
    } else {
        stream_channel.item_type = PlaylistItemType::LiveHls;
        stream_channel.cluster = PlaylistItemType::LiveHls.cluster();
    }
    let shared_stream_id = hls_cache_shared_stream_id(&proxy_session_id);
    stream_channel.shared = true;
    stream_channel.shared_stream_id = Some(shared_stream_id);
    stream_channel.shared_joined_existing = Some(
        hls_cache_shared_joined_existing(app_state, shared_stream_id, &access.username, &access.user_session_token)
            .await,
    );
    let qos_config = HlsQosRuntimeConfig::from_app_config(&app_state.app_config);
    let qos_registration = app_state
        .hls
        .proxy
        .qos()
        .ensure_access_lease(
            &access.lease_id,
            &proxy_session_id,
            current_time_millis(),
            hls_qos_meter_init(app_state, qos_config),
        )
        .await;
    if let Some(meter) = qos_registration.register_meter.as_ref() {
        app_state.event_manager.register_meter(Arc::clone(meter)).await;
    }
    let history_mode = if qos_registration.emit_connect_record {
        ConnectionHistoryMode::EmitConnect
    } else {
        ConnectionHistoryMode::RefreshOnly
    };

    app_state
        .connection_manager
        .update_connection_with_history_mode(
            crate::api::model::ConnectionParams {
                meter_uid: qos_registration.meter_uid,
                username: &access.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind,
                priority,
                soft_priority: user.soft_priority,
                fingerprint,
                provider,
                stream_channel: &stream_channel,
                user_agent,
                session_token: Some(&access.user_session_token),
            },
            history_mode,
        )
        .await
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_stats_provider(
    origin_source: &HlsOriginSource,
    origin_account_binding: Option<&HlsOriginAccountBinding>,
    user_session: &UserSession,
) -> Arc<str> {
    origin_account_binding.filter(|binding| binding.is_active()).map_or_else(
        || {
            if user_session.provider.is_empty() {
                Arc::clone(&origin_source.input_name)
            } else {
                Arc::clone(&user_session.provider)
            }
        },
        |binding| Arc::clone(&binding.account_name),
    )
}

pub(in crate::api::endpoints::hls_api) async fn build_hls_cache_stream_channel(
    app_state: &Arc<AppState>,
    access: &HlsAccessContext,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let mut channel = if let Some((_, target)) = app_state.app_config.get_target_for_username(&access.username) {
        if let Some(mut channel) = get_stream_channel(app_state, &target, access.virtual_id).await {
            channel.url = Arc::from(hls_cache_stream_stats_url(proxy_session_id));
            channel
        } else {
            fallback_hls_cache_stream_channel(target.id, access.virtual_id, origin_source, proxy_session_id)
        }
    } else {
        fallback_hls_cache_stream_channel(0, access.virtual_id, origin_source, proxy_session_id)
    };

    let archive_reference = access
        .epg_reference_ts
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token));

    if archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
    {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.cluster = PlaylistItemType::LiveHls.cluster();
        channel.epg_reference_ts = None;
    }
    channel
}

pub(in crate::api::endpoints::hls_api) fn fallback_hls_cache_stream_channel(
    target_id: u16,
    virtual_id: u32,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    StreamChannel {
        target_id,
        virtual_id,
        provider_id: 0,
        input_name: Arc::clone(&origin_source.input_name),
        item_type: PlaylistItemType::LiveHls,
        cluster: XtreamCluster::Live,
        group: unknown.clone(),
        title: unknown,
        url: Arc::from(hls_cache_stream_stats_url(proxy_session_id)),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_stream_stats_url(proxy_session_id: &ProxySessionId) -> String {
    format!("/hls/shared/live/{}/manifest.m3u8", proxy_session_id.0)
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_shared_stream_id(proxy_session_id: &ProxySessionId) -> u64 {
    let digest = Sha256::digest(proxy_session_id.0.as_bytes());
    digest.iter().take(8).fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
}

pub(in crate::api::endpoints::hls_api) async fn hls_cache_shared_joined_existing(
    app_state: &Arc<AppState>,
    shared_stream_id: u64,
    username: &str,
    session_token: &str,
) -> bool {
    let streams = app_state.active_users.active_streams().await;
    if let Some(existing) = streams.iter().find(|stream| {
        stream.username == username
            && stream.session_token.as_deref() == Some(session_token)
            && stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
    }) {
        return existing.channel.shared_joined_existing.unwrap_or(false);
    }

    streams.iter().any(|stream| {
        stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
            && (stream.username != username || stream.session_token.as_deref() != Some(session_token))
    })
}
