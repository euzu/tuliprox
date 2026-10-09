use super::{
    append_user_session_provider_headers, build_hls_manifest_request_headers, build_hls_origin_resolution,
    build_hls_origin_source_for_playback, create_hls_cache_entry_master_playlist_response,
    download_legacy_hls_manifest, ensure_hls_manifest_extension, hls_cache_enabled_for_user,
    hls_custom_video_manifest_response, hls_entry_user_session_token, hls_media_playlist_wrap_enabled,
    hls_panel_provisioning_or_status_response, hls_playback_kind, hls_response, m3u_archive_epg_reference_ts,
    m3u_catchup_epg_reference_from_session_token, normalize_xtream_live_hls_url, release_prepared_hls_manifest_session,
    terminate_failed_hls_manifest_session, try_reserve_hls_entry_origin_account_for_redirect, wrap_media_playlist,
    HlsEntryStreamContext, HlsMediaPlaylistWrap, HlsOriginEntryUrl, HlsRequestStage,
};
use crate::{
    api::{
        api_utils::{
            acquire_exact_provider_handle, connection_priority_for_kind, get_hls_playback_ttl_secs,
            select_provider_stream_url, ExactProviderAcquire, HLS_MANIFEST_CAPACITY_WAIT,
        },
        model::{AppState, CustomVideoStreamType, PlaybackLeaseRef, ProviderAllocation, UserSession},
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, InputSource, ProxyUserCredentials},
    processing::parser::hls::{
        classify_hls_playlist, rewrite_hls, HlsManifestSource, HlsPlaylistKind, RewriteHlsProps,
    },
    repository::{m3u_get_item_for_stream_id, storage_const, xtream_get_item_for_stream_id},
    utils::{debug_if_enabled, request},
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::{error, warn};
use shared::{
    defaults::HLS_EXT,
    model::{PlaylistItemType, StreamChannel, TargetType, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;
use tuliprox_hls::api::{
    build_hls_origin_session_owner, build_proxy_session_id, extract_hls_provider_session_headers, HlsOriginSource,
    HlsSessionKey,
};

pub(in crate::api::endpoints::hls_api) struct HlsCacheManifestOrigin<'a> {
    pub(in crate::api::endpoints::hls_api) raw_request_url: &'a str,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
    pub(in crate::api::endpoints::hls_api) input: &'a ConfigInput,
    pub(in crate::api::endpoints::hls_api) origin_source: HlsOriginSource,
}

pub(in crate::api::endpoints::hls_api) struct HlsCacheOriginResolution {
    pub(in crate::api::endpoints::hls_api) hls_url: String,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
}

pub(in crate::api) fn build_virtual_hls_entry_path(
    target: &ConfigTarget,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    virtual_id: u32,
) -> String {
    if input.input_type.is_m3u() && !target.has_output(TargetType::Xtream) {
        format!("/{}/live/{}/{}/{}{HLS_EXT}", storage_const::M3U_STREAM_PATH, user.username, user.password, virtual_id)
    } else {
        format!("/live/{}/{}/{}{HLS_EXT}", user.username, user.password, virtual_id)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn handle_hls_stream_request(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    user_session: Option<&UserSession>,
    session_token_hint: Option<&str>,
    hls_url: &str,
    archive_reference: Option<i64>,
    stream_context: HlsEntryStreamContext,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    original_hls_entry_path: &str,
    stage: HlsRequestStage<'_>,
) -> impl IntoResponse + Send {
    let virtual_id = stream_context.virtual_id();
    if app_state.active_users.is_user_blocked_for_stream(&user.username, VirtualId::new(virtual_id)).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ref = stream_context.stream_ref().to_string();
    // Variant URLs were normalized before sealing or come from an upstream master; use them as is.
    let url = if stage.normalizes_url() {
        let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
        if normalized_hls_url != hls_url {
            debug_if_enabled!(
                "Normalized xtream hls url from {} to {}",
                sanitize_sensitive_info(hls_url),
                sanitize_sensitive_info(&normalized_hls_url)
            );
        }
        ensure_hls_manifest_extension(&normalized_hls_url)
    } else {
        hls_url.to_string()
    };
    // The wrapper seals the normalized entry URL, not the provider-resolved one, so every
    // refresh can resolve it to whichever account is selected then.
    let entry_url = url.clone();
    // Recover archive context when callers (esp. Xtream timeshift) pass None but the
    // resolved provider URL / catchup session still carries Flussonic archive markers.
    let archive_reference = archive_reference.or_else(|| m3u_archive_epg_reference_ts(&url)).or_else(|| {
        user_session
            .map(|session| session.token.as_str())
            .or(session_token_hint)
            .and_then(m3u_catchup_epg_reference_from_session_token)
    });
    let hls_cache_origin = build_hls_origin_resolution(input, &url);
    let hls_origin_source = hls_cache_origin.as_ref().map(|_| {
        build_hls_origin_source_for_playback(input, stream_ref.clone(), archive_reference, Some(url.as_str()))
    });
    let server_info = app_state.app_config.get_user_server_info(user);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let mut headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        stream_context.identity().upstream_user_agent(),
    );

    if hls_cache_enabled_for_user(app_state, target, user) {
        let Some(origin_source) = hls_origin_source.clone() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        return create_hls_cache_entry_master_playlist_response(
            app_state,
            fingerprint,
            user,
            origin_source,
            virtual_id,
            user_session,
            stream_context.known_bitrate_bps(),
            session_token_hint,
            if archive_reference.is_some() {
                url.as_str()
            } else {
                hls_cache_origin.as_ref().map_or(url.as_str(), |origin| origin.session_entry_url.as_str())
            },
            input,
            connection_permission,
            connection_kind,
            server_info.path.as_deref(),
        )
        .await;
    }

    let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
    let (request_url, session_token, provider_handle, selected_provider_config) = if let Some(session) = user_session {
        let pinned_provider = if session.provider.is_empty() { &input.name } else { &session.provider };
        let pinned_kind = hls_playback_kind(archive_reference, &url, Some(session.token.as_str()));
        let session_kind =
            session.connection_kind.or(connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal);
        // Manifest refreshes of parallel playbacks on one account free their slots within seconds;
        // waiting briefly beats an immediate 503 that stalls the player.
        let acquired = acquire_exact_provider_handle(
            app_state,
            &ExactProviderAcquire {
                provider: pinned_provider,
                addr: &fingerprint.addr,
                allow_grace: false,
                priority: connection_priority_for_kind(user, session_kind),
                kind: session_kind,
                lease: Some(PlaybackLeaseRef::new(session.token.as_str(), pinned_kind)),
            },
            Some(HLS_MANIFEST_CAPACITY_WAIT),
        )
        .await;
        let provider_handle = if let Some(handle) = acquired {
            Some(handle)
        } else {
            debug_if_enabled!(
                "HLS pinned provider {} unavailable for {}; aborting allocation to prevent mid-session migration",
                sanitize_sensitive_info(pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            None
        };

        if provider_handle.is_none() {
            // The capacity wait can outlive the session snapshot: an ended or rebound session must
            // not trigger panel provisioning for the stale pinned account.
            if app_state.active_users.session_identity(&user.username, &session.token).await != Some(session.identity())
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        }
        match provider_handle.as_ref().map(|handle| &handle.allocation) {
            Some(ProviderAllocation::Exhausted) => (url, None, provider_handle, None),
            Some(ProviderAllocation::Available(cfg) | ProviderAllocation::GracePeriod(cfg)) => {
                let selected_provider_config = Arc::clone(cfg);
                // URL acceptance follows the provider that produced the sealed URL, not the session:
                // entry URLs are re-resolved to the selected account on every fetch, child URLs
                // (often CDN URLs) are fetched as sealed and only from their origin provider.
                let accept_requested_stream_url = match stage {
                    HlsRequestStage::Variant { source: HlsManifestSource::Child, origin_provider } => {
                        if origin_provider != Some(selected_provider_config.name.as_ref()) {
                            // A stale child URI from an earlier account: reject only this request,
                            // the playback itself stays valid after the player refetches its master.
                            app_state.connection_manager.release_provider_handle(provider_handle);
                            debug_if_enabled!(
                                "HLS child manifest from provider {} rejected for selected provider {}",
                                sanitize_sensitive_info(origin_provider.unwrap_or("<unknown>")),
                                sanitize_sensitive_info(&selected_provider_config.name)
                            );
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        true
                    }
                    HlsRequestStage::Entry | HlsRequestStage::LegacyVariant | HlsRequestStage::Variant { .. } => false,
                };
                let Some((_provider_name, stream_url)) = select_provider_stream_url(
                    &url,
                    input,
                    &selected_provider_config,
                    accept_requested_stream_url,
                    &app_state.app_config,
                )
                .await
                else {
                    app_state.connection_manager.release_provider_handle(provider_handle);
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                // The capacity wait can outlive the session snapshot: a session that ended or switched
                // accounts meanwhile must not be rebound, and cookies rotated or expired during the
                // wait are read now.
                let session_headers = app_state
                    .active_users
                    .provider_headers_for_session_identity(
                        &user.username,
                        &session.token,
                        session.identity(),
                        &stream_url,
                    )
                    .await;
                if session_headers == tuliprox_session::SessionProviderHeaders::NoSession {
                    app_state.connection_manager.release_provider_handle(provider_handle);
                    debug_if_enabled!(
                        "HLS session {} changed while waiting for pinned provider {}",
                        sanitize_sensitive_info(&session.token),
                        sanitize_sensitive_info(pinned_provider)
                    );
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if session.provider.as_ref() == selected_provider_config.name.as_ref() {
                    if let tuliprox_session::SessionProviderHeaders::Headers(session_headers) = session_headers {
                        append_user_session_provider_headers(&mut headers, &session_headers);
                    }
                }
                let session_token = app_state
                    .active_users
                    .create_user_session(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token: &session.token,
                        virtual_id,
                        provider: &selected_provider_config.name,
                        stream_url: &stream_url,
                        addr: &fingerprint.addr,
                        connection_permission,
                        connection_kind: session.connection_kind.or(connection_kind),
                        socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                    })
                    .await;
                let session_ttl_secs = get_hls_playback_ttl_secs(app_state, pinned_kind);
                app_state.active_provider.refresh_adaptive_playback_lease(
                    &selected_provider_config.name,
                    &session_token,
                    pinned_kind,
                    session_ttl_secs,
                );
                (stream_url, Some(session_token), provider_handle, Some(selected_provider_config))
            }
            None => (url, None, None, None),
        }
    } else {
        // Append/shift catchup must keep an m3u-catchup session token even when shared HLS
        // cache is off; otherwise rewritten segments register as LiveHls in the panel.
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let hls_session_owner = if hls_cache_enabled_for_user(app_state, target, user) {
            let session_key = HlsSessionKey::new(input.id, stream_context.stream_ref());
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            Some(build_hls_origin_session_owner(&proxy_session_id))
        } else {
            None
        };
        let session_owner = hls_session_owner.as_deref().unwrap_or(user_session_token.as_str());
        // Archive playback keeps its own reconnect window semantics, so the lease must
        // be classified as catchup rather than plain live HLS.
        let playback_kind = hls_playback_kind(archive_reference, &url, Some(user_session_token.as_str()));
        let hls_session_ttl_secs = get_hls_playback_ttl_secs(app_state, playback_kind);
        let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
            app_state,
            fingerprint,
            user,
            input,
            virtual_id,
            &url,
            &user_session_token,
            session_owner,
            playback_kind,
            hls_session_ttl_secs,
            connection_permission,
            fallback_connection_kind,
            true,
        )
        .await
        else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        debug_if_enabled!(
            "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} provider={} stream_url={}",
            sanitize_sensitive_info(&user.username),
            reservation.selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
            sanitize_sensitive_info(&reservation.request_url)
        );
        (
            reservation.request_url,
            Some(reservation.session_token),
            reservation.provider_handle,
            reservation.selected_provider_config,
        )
    };

    // The session as created or reserved above; manifest response cookies may only enter this
    // binding, not one an account switch installed while the playlist downloaded.
    let session_identity = match session_token.as_deref() {
        Some(token) => app_state.active_users.session_identity(&user.username, token).await,
        None => None,
    };
    let provider_binding_tag = provider_handle.as_ref().and_then(|handle| handle.binding_tag);
    let provider_request_id = provider_handle.as_ref().and_then(|handle| handle.playback_request_id);
    let selected_provider_name = selected_provider_config.as_ref().map(|cfg| Arc::clone(&cfg.name));

    // The entry request already downloaded this playlist; serve it once to the immediate variant
    // request when it is still bound to the same provider binding.
    if matches!(stage, HlsRequestStage::Variant { source: HlsManifestSource::Entry, .. }) {
        if let (Some(session_token), Some(provider)) = (session_token.as_deref(), selected_provider_name.as_deref()) {
            if let Some(entry) = app_state
                .hls
                .playlist_handoff
                .take(session_token, hls_url)
                .filter(|entry| entry.matches(provider, provider_binding_tag))
            {
                app_state.connection_manager.release_provider_handle(provider_handle);
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
                return hls_response(entry.content).into_response();
            }
        }
    }

    let user_agent_stream_index = crate::api::api_utils::resolve_stream_user_agent_index(
        app_state,
        input,
        provider_handle.is_some(),
        &user.username,
        session_token.as_deref(),
    )
    .await;
    if let Some(stream_index) = user_agent_stream_index {
        request::append_user_agent_stream_index(&mut headers, stream_index);
        if let Some(session_token) = session_token.as_deref() {
            app_state
                .active_users
                .set_user_agent_stream_index_if_absent(&user.username, session_token, stream_index)
                .await;
        }
    }

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle);

    let input_source = InputSource::from(input).with_url(request_url);
    let download_result = match download_legacy_hls_manifest(app_state, &input_source, &headers).await {
        Ok((content, response_url, response_headers)) => match classify_hls_playlist(&content) {
            // A refresh keeps its session on a malformed body; the player retries the playlist.
            HlsPlaylistKind::Invalid if stage != HlsRequestStage::Entry => {
                warn!("Upstream HLS refresh returned no playlist for virtual_id={virtual_id}; keeping session");
                if let Some(session_token) = session_token.as_deref() {
                    release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                        .await;
                }
                return StatusCode::BAD_GATEWAY.into_response();
            }
            HlsPlaylistKind::Invalid => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "upstream response is not an HLS playlist"))
            }
            playlist_kind => Ok((content, response_url, response_headers, playlist_kind)),
        },
        Err(err) => Err(err),
    };
    match download_result {
        Ok((content, response_url, response_headers, playlist_kind)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id: target.id,
                virtual_id,
                input_id: input.id,
                user_token: session_token.as_deref(),
                origin_provider: selected_provider_name.as_deref(),
                playlist_kind: Some(playlist_kind),
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            if let Some(session_token) = session_token.as_deref() {
                let session_headers = extract_hls_provider_session_headers(&response_headers);
                if let (false, Some(identity)) = (session_headers.is_empty(), session_identity) {
                    app_state
                        .active_users
                        .update_current_session_provider_response_headers_from(
                            &user.username,
                            session_token,
                            identity,
                            &session_headers,
                            &rewrite_hls_props.hls_url,
                        )
                        .await;
                }
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
            }
            if let (HlsRequestStage::Entry, HlsPlaylistKind::Media, Some(session_token), Some(provider)) =
                (stage, playlist_kind, session_token.as_deref(), selected_provider_name.as_ref())
            {
                if hls_media_playlist_wrap_enabled(app_state, target, user) {
                    let master = wrap_media_playlist(
                        HlsMediaPlaylistWrap {
                            app_state,
                            user,
                            base_url: &base_url,
                            target_id: target.id,
                            input,
                            virtual_id,
                            session_token,
                            sealed_url: &entry_url,
                            provider,
                            binding_tag: provider_binding_tag,
                            known_bitrate_bps: stream_context.known_bitrate_bps(),
                            stream_ref: Some(&stream_ref),
                        },
                        hls_content,
                    )
                    .await;
                    return hls_response(master).into_response();
                }
            }
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            error!("Failed to download m3u8: {}", request::text_response_error_log_label(&err));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(
                    app_state,
                    &user.username,
                    session_token,
                    selected_provider_name.as_ref(),
                    provider_binding_tag,
                    provider_request_id,
                )
                .await;
            }

            hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
        .await
        .ok()
        .map(|pli| pli.to_stream_channel(target_id))
}
