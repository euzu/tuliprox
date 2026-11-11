use super::{
    append_user_session_provider_headers, build_hls_manifest_request_headers, build_hls_origin_resolution,
    build_hls_origin_source_for_playback, create_hls_cache_entry_master_playlist_response,
    download_legacy_hls_manifest, ensure_hls_manifest_extension, hls_cache_enabled_for_user,
    hls_custom_video_manifest_response, hls_entry_user_session_token, hls_media_playlist_wrap_enabled,
    hls_panel_provisioning_or_status_response, hls_playback_kind, hls_response, m3u_archive_epg_reference_ts,
    m3u_catchup_epg_reference_from_session_token, normalize_xtream_live_hls_url, release_prepared_hls_manifest_session,
    stalker_hls_stream, stalker_manifest_url, terminate_failed_hls_manifest_session,
    try_reserve_hls_entry_origin_account_for_redirect, wrap_media_playlist, HlsEntryStreamContext,
    HlsMediaPlaylistWrap, HlsOriginEntryUrl, HlsRequestStage, StalkerManifestUrl,
};
use crate::{
    api::{
        api_utils::{
            blocks_new_playback, connection_priority_for_kind, get_hls_playback_ttl_secs, group_playback_settings,
            is_running_playback, resolve_stalker_stream_for_account, select_provider_stream_url, stalker_stream_kind,
            GroupSelectionRequest,
        },
        model::{AppState, CustomVideoStreamType, PlaybackLeaseRef, UserSession},
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
use tuliprox_core::model::PlaybackKind;
use tuliprox_hls::api::{
    build_hls_origin_session_owner, build_proxy_session_id, extract_hls_provider_session_headers, HlsOriginSource,
    HlsSessionKey,
};
use tuliprox_repository::{
    target_stream_item::{classify_virtual_id, ResolvedStreamTarget},
    FailoverGroupRecord,
};
use tuliprox_session::{
    failover_group::{FailureClass, HalfOpenGuard, SelectionMode},
    MemberBinding,
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

/// Proxy path of the HLS entry playlist for `virtual_id`: the M3U path for M3U-only targets, else the Xtream path.
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

/// Serves an HLS manifest request for the selected provider account; a Stalker manifest URL is accepted, resolved or
/// rejected per [`stalker_manifest_url`].
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
    let mut entry_url = url.clone();
    // Recover archive context when callers (esp. Xtream timeshift) pass None but the
    // resolved provider URL / catchup session still carries Flussonic archive markers.
    let archive_reference = archive_reference.or_else(|| m3u_archive_epg_reference_ts(&url)).or_else(|| {
        user_session
            .map(|session| session.token.as_str())
            .or(session_token_hint)
            .and_then(m3u_catchup_epg_reference_from_session_token)
    });

    let is_live_hls = archive_reference.is_none()
        && hls_playback_kind(
            archive_reference,
            &url,
            user_session.map(|session| session.token.as_str()).or(session_token_hint),
        ) == PlaybackKind::LiveHls;

    let classified = if matches!(stage, HlsRequestStage::Entry) {
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(virtual_id)).await
    } else {
        ResolvedStreamTarget::Item
    };
    let (entry_group, prefer_member) = FailoverGroupRecord::classify_entry_target(&classified, is_live_hls);

    // Only a fresh entry is gated. Variant refreshes, segments and entry re-polls of a running
    // session (unwrapped media playlists) belong to a running playback. A group checks
    // maintenance per member during selection.
    if matches!(stage, HlsRequestStage::Entry)
        && entry_group.is_none()
        && !is_running_playback(user_session)
        && blocks_new_playback(app_state, input, user, false)
    {
        return hls_custom_video_manifest_response(
            app_state,
            user,
            CustomVideoStreamType::Maintenance,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    }
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
        if let Some(group) = entry_group {
            // The shared origin belongs to the selected member, so viewers of the same member share
            // it. The cache path reserves its own provider account; the selection only picks.
            let playback = group_playback_settings(app_state, target);
            let kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
            let now_ts = chrono::Utc::now().timestamp();
            let mode =
                archive_reference.map_or(SelectionMode::Live, |start_ts| SelectionMode::Archive { start_ts, now_ts });
            let req = GroupSelectionRequest {
                mode,
                prefer: prefer_member,
                allow_provider_grace: playback.allow_provider_grace,
                bypass_maintenance: user.is_recording_proxy_user(),
                allow_preempt: false,
                ..GroupSelectionRequest::new(
                    target.name.as_str(),
                    &group,
                    playback.start,
                    &fingerprint.addr,
                    &user.username,
                    connection_priority_for_kind(user, kind),
                    kind,
                )
            };
            let Ok((resolved, probed)) = super::group_start::resolve_checked_hls_group_entry(
                app_state,
                target,
                Arc::clone(&group),
                req,
                req_headers,
                false,
                None,
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
            // In shared cache mode, the cache reserves its own provider account during stream startup.
            // Accepted limitation: one extra manifest probe request per fresh entry, and a potential
            // provider account change between probe and cache worker start.
            let mut selection_handle = resolved.acquired.handle;
            let allocated = selection_handle
                .handle()
                .and_then(|handle| handle.allocation.get_provider_name())
                .unwrap_or_else(|| Arc::clone(&resolved.member.input_name));
            app_state.connection_manager.release_provider_handle(selection_handle.take());

            let Some(member_url) = (if let Some(ref probed) = probed {
                Some(probed.request_url.clone())
            } else {
                super::group_start::build_hls_group_member_origin_url(
                    app_state,
                    target,
                    &resolved.input,
                    &resolved.member,
                    &allocated,
                    None,
                )
                .await
            }) else {
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

            let member_origin = build_hls_origin_resolution(&resolved.input, &member_url);
            let member_source = build_hls_origin_source_for_playback(
                &resolved.input,
                resolved.member.input_stream_id.to_string(),
                archive_reference,
                Some(member_url.as_str()),
            );
            return create_hls_cache_entry_master_playlist_response(
                app_state,
                fingerprint,
                user,
                member_source,
                virtual_id,
                user_session,
                stream_context.known_bitrate_bps(),
                session_token_hint,
                if archive_reference.is_some() {
                    member_url.as_str()
                } else {
                    member_origin.as_ref().map_or(member_url.as_str(), |origin| origin.session_entry_url.as_str())
                },
                &resolved.input,
                connection_permission,
                connection_kind,
                server_info.path.as_deref(),
            )
            .await;
        }
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
    let mut bound_member_info: Option<(u32, Arc<ConfigInput>, String, Option<HalfOpenGuard>)> = None;

    // Set when the session cookie for the pinned URL's host replaced the request cookie; holds the prior value.
    let mut replaced_cookie: Option<Option<axum::http::HeaderValue>> = None;
    // `pinned_stalker_stream` is set when a pinned refresh resolved its Stalker link; a rejected playlist then
    // forces one fresh `create_link` for the same account.
    let (
        mut request_url,
        session_token,
        provider_handle,
        selected_provider_config,
        provider_binding_tag,
        pinned_stalker_stream,
        probed_manifest,
        precomputed_user_agent_stream_index,
    ): (String, _, _, _, _, _, _, _) = if let Some(session) = user_session {
        let bound_inp_opt = session.member_binding.as_ref().and_then(|binding| {
            app_state.app_config.get_input_by_name(&binding.input_name).map(|inp| (binding.member_vid, inp))
        });
        if let Some((member_vid, ref bound_inp)) = bound_inp_opt {
            // The group entry's stream ref belongs to its representative channel; a bound member
            // resolves its own portal stream and account on every Stalker refresh.
            let member_stream_ref = if bound_inp.input_type.is_stalker() {
                bound_member_portal_stream_id(app_state, target, member_vid)
                    .await
                    .map(|provider_id| provider_id.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            bound_member_info = Some((member_vid, Arc::clone(bound_inp), member_stream_ref, None));
        }
        let effective_lookup_input = bound_inp_opt.as_ref().map_or(input, |(_, inp)| inp.as_ref());
        let pinned_provider = if session.provider.is_empty() {
            effective_lookup_input.name.clone()
        } else {
            Arc::clone(&session.provider)
        };
        let pinned_kind = hls_playback_kind(archive_reference, &url, Some(session.token.as_str()));
        // A playlist is text, not media: a refresh of a running playback resolves its pinned account
        // without allocating, waiting for or failing on provider capacity. Only segments hold slots.
        let Some(selected_provider_config) = app_state.active_provider.find_provider_config(&pinned_provider) else {
            debug_if_enabled!(
                "HLS pinned provider {} is no longer configured for {}; aborting to prevent mid-session migration",
                sanitize_sensitive_info(&pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            // An ended or rebound session must not trigger panel provisioning for the stale pinned account.
            if app_state.active_users.session_identity(&user.username, &session.token).await != Some(session.identity())
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                effective_lookup_input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        if selected_provider_config.is_account_blocked() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        // URL acceptance follows the provider that produced the sealed URL, not the session:
        // entry URLs are re-resolved to the selected account on every fetch, child URLs
        // (often CDN URLs) are fetched as sealed and only from their origin provider.
        // A Stalker URL is account-bound: refreshes resolve a fresh link, sealed variants stay with their account.
        // A group member session follows the same account-bound rules as any other Stalker session.
        let stalker_url = effective_lookup_input
            .input_type
            .is_stalker()
            .then(|| stalker_manifest_url(stage, selected_provider_config.name.as_ref()));
        if stalker_url == Some(StalkerManifestUrl::Reject) {
            debug_if_enabled!(
                "HLS Stalker manifest token without matching origin rejected for selected provider {}",
                sanitize_sensitive_info(&selected_provider_config.name)
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        let accept_requested_stream_url = stalker_url == Some(StalkerManifestUrl::Accept)
            || match stage {
                HlsRequestStage::Variant { source: HlsManifestSource::Child, origin_provider } => {
                    if origin_provider != Some(selected_provider_config.name.as_ref()) {
                        // A stale child URI from an earlier account: reject only this request,
                        // the playback itself stays valid after the player refetches its master.
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
        let pinned_stalker_stream = (stalker_url == Some(StalkerManifestUrl::Resolve))
            .then(|| {
                let stream_ref = bound_member_info.as_ref().map_or(stream_ref.as_str(), |(_, _, member_ref, _)| {
                    if member_ref.is_empty() {
                        stream_ref.as_str()
                    } else {
                        member_ref.as_str()
                    }
                });
                stalker_hls_stream(stream_ref, archive_reference.is_some())
            })
            .flatten();
        let selected_stream = if stalker_url == Some(StalkerManifestUrl::Resolve) {
            match pinned_stalker_stream {
                Some((provider_id, kind)) => {
                    resolve_stalker_stream_for_account(
                        app_state,
                        effective_lookup_input,
                        &selected_provider_config.name,
                        provider_id,
                        kind,
                        false,
                    )
                    .await
                }
                None => None,
            }
        } else {
            select_provider_stream_url(
                &url,
                effective_lookup_input,
                &selected_provider_config,
                accept_requested_stream_url,
                &app_state.app_config,
            )
            .await
        };
        let Some((_provider_name, stream_url)) = selected_stream else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        // A session that ended or switched accounts meanwhile must not be rebound.
        let session_headers = app_state
            .active_users
            .provider_headers_for_session_identity(&user.username, &session.token, session.identity(), &stream_url)
            .await;
        if session_headers == tuliprox_session::SessionProviderHeaders::NoSession {
            debug_if_enabled!(
                "HLS session {} changed while resolving pinned provider {}",
                sanitize_sensitive_info(&session.token),
                sanitize_sensitive_info(&pinned_provider)
            );
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if session.provider.as_ref() == selected_provider_config.name.as_ref() {
            if let tuliprox_session::SessionProviderHeaders::Headers(session_headers) = session_headers {
                replaced_cookie = Some(headers.get(axum::http::header::COOKIE).cloned());
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
        let provider_binding_tag = app_state.active_provider.binding_tag_for_owner(&session_token);
        (
            stream_url.clone(),
            Some(session_token),
            None,
            Some(selected_provider_config),
            provider_binding_tag,
            pinned_stalker_stream,
            None,
            None,
        )
    } else if let Some(group) = entry_group {
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let playback_kind = hls_playback_kind(archive_reference, &url, Some(user_session_token.as_str()));
        let hls_session_ttl_secs = get_hls_playback_ttl_secs(app_state, playback_kind);

        let playback = group_playback_settings(app_state, target);
        let now_ts = chrono::Utc::now().timestamp();
        let mode =
            archive_reference.map_or(SelectionMode::Live, |start_ts| SelectionMode::Archive { start_ts, now_ts });
        let req = GroupSelectionRequest {
            mode,
            prefer: prefer_member,
            lease: Some(PlaybackLeaseRef::new(user_session_token.as_str(), playback_kind)),
            allow_provider_grace: playback.allow_provider_grace,
            bypass_maintenance: user.is_recording_proxy_user(),
            ..GroupSelectionRequest::new(
                target.name.as_str(),
                &group,
                playback.start,
                &fingerprint.addr,
                &user.username,
                connection_priority_for_kind(user, fallback_connection_kind),
                fallback_connection_kind,
            )
        };

        let Ok((resolved, probed_manifest)) = super::group_start::resolve_checked_hls_group_entry(
            app_state,
            target,
            Arc::clone(&group),
            req,
            req_headers,
            true,
            Some((&user.username, user_session_token.as_str())),
            false,
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

        let precomputed_user_agent_stream_index = probed_manifest.as_ref().and_then(|p| p.user_agent_stream_index);

        let selected_provider_config =
            resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config());
        let allocated = selected_provider_config
            .as_ref()
            .map_or_else(|| Arc::clone(&resolved.member.input_name), |cfg| Arc::clone(&cfg.name));

        let Some(member_url) = (if let Some(probed) = probed_manifest.as_ref() {
            Some(probed.request_url.clone())
        } else {
            super::group_start::build_hls_group_member_origin_url(
                app_state,
                target,
                &resolved.input,
                &resolved.member,
                &allocated,
                selected_provider_config.as_ref(),
            )
            .await
        }) else {
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

        let mut acquired_handle = resolved.acquired.handle;
        let provider_handle = acquired_handle.take();

        let provider_name = selected_provider_config
            .as_ref()
            .map_or_else(|| Arc::clone(&resolved.input.name), |cfg| Arc::clone(&cfg.name));

        let session_token = app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token: &user_session_token,
                virtual_id,
                provider: &provider_name,
                stream_url: &member_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(fallback_connection_kind),
                socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
            })
            .await;

        app_state
            .active_users
            .bind_member(
                &user.username,
                &session_token,
                MemberBinding {
                    group_vid: group.group_vid.get(),
                    member_vid: resolved.member.member_vid.get(),
                    member_index: resolved.acquired.index,
                    input_name: Arc::clone(&resolved.member.input_name),
                    member_title: Arc::clone(&resolved.member.title),
                    provider: Arc::clone(&provider_name),
                },
            )
            .await;

        app_state.active_provider.refresh_adaptive_playback_lease(
            &provider_name,
            &user_session_token,
            playback_kind,
            hls_session_ttl_secs,
        );

        bound_member_info = Some((
            resolved.member.member_vid.get(),
            Arc::clone(&resolved.input),
            resolved.member.input_stream_id.to_string(),
            resolved.acquired.half_open,
        ));

        debug_if_enabled!(
                "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} (group) member_vid={} provider={} stream_url={}",
                sanitize_sensitive_info(&user.username),
                resolved.member.member_vid.get(),
                selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
                sanitize_sensitive_info(&member_url)
            );

        entry_url = member_url.clone();

        let provider_binding_tag = provider_handle.as_ref().and_then(|handle| handle.binding_tag);
        let pinned_stalker_stream = if resolved.input.input_type.is_stalker() {
            Some((resolved.member.provider_id, stalker_stream_kind(resolved.member.cluster, resolved.member.item_type)))
        } else {
            None
        };
        (
            member_url,
            Some(session_token),
            provider_handle,
            selected_provider_config,
            provider_binding_tag,
            pinned_stalker_stream,
            probed_manifest,
            precomputed_user_agent_stream_index,
        )
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
            stalker_hls_stream(stream_context.stream_ref(), archive_reference.is_some()),
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
        let provider_binding_tag = reservation.provider_handle.as_ref().and_then(|handle| handle.binding_tag);
        (
            reservation.request_url,
            Some(reservation.session_token),
            reservation.provider_handle,
            reservation.selected_provider_config,
            provider_binding_tag,
            None,
            None,
            None,
        )
    };

    // The session as created or reserved above; manifest response cookies may only enter this
    // binding, not one an account switch installed while the playlist downloaded.
    let mut session_identity = match session_token.as_deref() {
        Some(token) => app_state.active_users.session_identity(&user.username, token).await,
        None => None,
    };
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

    let (effective_virtual_id, effective_input, effective_stream_ref, half_open_guard) =
        if let Some((m_vid, ref m_input, ref m_sref, guard)) = bound_member_info {
            (m_vid, m_input.as_ref(), if m_sref.is_empty() { stream_ref.as_str() } else { m_sref.as_str() }, guard)
        } else {
            (virtual_id, input, stream_ref.as_str(), None)
        };

    let user_agent_stream_index = if let Some(stream_index) = precomputed_user_agent_stream_index {
        Some(stream_index)
    } else {
        crate::api::api_utils::resolve_stream_user_agent_index(
            app_state,
            effective_input,
            selected_provider_config.is_some(),
            &user.username,
            session_token.as_deref(),
        )
        .await
    };
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

    let mut download_result = if let Some(probed) = probed_manifest {
        Ok((probed.content, probed.response_url, probed.response_headers, probed.playlist_kind))
    } else {
        download_classified_hls_manifest(app_state, effective_input, &request_url, &headers).await
    };
    // A cached Stalker link the portal no longer honors: resolve a fresh one for the pinned account, retry once.
    if let (true, Some((provider_id, kind)), Some(provider)) =
        (is_rejected_hls_manifest_download(&download_result), pinned_stalker_stream, selected_provider_name.as_ref())
    {
        if let Some((_, fresh_url)) =
            resolve_stalker_stream_for_account(app_state, effective_input, provider, provider_id, kind, true).await
        {
            if fresh_url != request_url {
                debug_if_enabled!(
                    "HLS Stalker playlist rejected for pinned provider {}; retrying with a fresh link",
                    sanitize_sensitive_info(provider)
                );
                request_url = fresh_url;
                if let (Some(token), Some(identity)) = (session_token.as_deref(), session_identity) {
                    if !app_state
                        .active_users
                        .update_current_session_stream_url(&user.username, token, identity, &request_url)
                        .await
                    {
                        session_identity = None;
                    }
                }
                // The session cookie was chosen for the old URL's host; re-read it for the fresh URL.
                if let Some(prior_cookie) = replaced_cookie.take() {
                    let fresh_headers = match (session_token.as_deref(), session_identity) {
                        (Some(token), Some(identity)) => {
                            app_state
                                .active_users
                                .provider_headers_for_session_identity(&user.username, token, identity, &request_url)
                                .await
                        }
                        _ => tuliprox_session::SessionProviderHeaders::NoSession,
                    };
                    let fresh_headers = match &fresh_headers {
                        tuliprox_session::SessionProviderHeaders::Headers(headers) => Some(headers),
                        _ => None,
                    };
                    rebind_session_cookie(&mut headers, prior_cookie, fresh_headers);
                }
                download_result =
                    download_classified_hls_manifest(app_state, effective_input, &request_url, &headers).await;
            }
        }
    }
    let download_result = match download_result {
        Ok((content, response_url, response_headers, playlist_kind)) => match playlist_kind {
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
            if let Some(guard) = half_open_guard {
                app_state.member_health.record_success_with(guard);
            }
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id: target.id,
                virtual_id: effective_virtual_id,
                input_id: effective_input.id,
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
                            input: effective_input,
                            virtual_id: effective_virtual_id,
                            session_token,
                            sealed_url: &entry_url,
                            provider,
                            binding_tag: provider_binding_tag,
                            known_bitrate_bps: stream_context.known_bitrate_bps(),
                            stream_ref: Some(effective_stream_ref),
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
            if let Some(guard) = half_open_guard {
                app_state.member_health.record_classified_failure_with(
                    guard,
                    FailureClass::Stream,
                    std::time::Instant::now(),
                );
            }
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

/// Portal stream id of the failover group member a session is bound to. The group entry's own
/// stream ref belongs to the representative channel, not to the member that actually plays.
async fn bound_member_portal_stream_id(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    member_vid: u32,
) -> Option<u32> {
    match classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(member_vid)).await {
        ResolvedStreamTarget::Member { group, index } => group.members.get(index).map(|member| member.provider_id),
        _ => None,
    }
}

/// Replaces the session cookie chosen for an earlier URL: restores the request cookie it replaced, then applies the
/// session cookie valid for the new URL, if any. Other headers stay unchanged.
fn rebind_session_cookie(
    headers: &mut HeaderMap,
    prior_cookie: Option<axum::http::HeaderValue>,
    session_headers: Option<&std::collections::HashMap<String, String>>,
) {
    match prior_cookie {
        Some(cookie) => headers.insert(axum::http::header::COOKIE, cookie),
        None => headers.remove(axum::http::header::COOKIE),
    };
    if let Some(session_headers) = session_headers {
        append_user_session_provider_headers(headers, session_headers);
    }
}

/// Playlist body, final response URL, response headers and playlist kind of a manifest download.
type ClassifiedHlsManifest = (String, String, HeaderMap, HlsPlaylistKind);

/// True when a manifest download indicates an expired or rejected link (4xx client error or invalid body).
pub(in crate::api::endpoints::hls_api) fn hls_download_rejects_link(
    result: Result<HlsPlaylistKind, &std::io::Error>,
) -> bool {
    match result {
        Ok(kind) => kind == HlsPlaylistKind::Invalid,
        Err(err) => request::request_failed_status(&err.to_string()).is_some_and(|status| {
            status.is_client_error()
                && !matches!(
                    status,
                    StatusCode::TOO_MANY_REQUESTS
                        | StatusCode::REQUEST_TIMEOUT
                        | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                )
        }),
    }
}

/// A 4xx response or a non-playlist body, the two ways a portal answers an expired link.
/// Timeouts, 5xx and transport errors do not trigger a fresh `create_link`. Neither do 4xx statuses that do not
/// reject the link: rate limiting (429), request timeout (408) and proxy authentication (407).
pub(in crate::api::endpoints::hls_api) fn is_rejected_hls_manifest_download(
    result: &Result<ClassifiedHlsManifest, std::io::Error>,
) -> bool {
    hls_download_rejects_link(result.as_ref().map(|(_, _, _, kind)| *kind))
}

/// Downloads a legacy HLS playlist and classifies its body.
async fn download_classified_hls_manifest(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    request_url: &str,
    headers: &HeaderMap,
) -> Result<ClassifiedHlsManifest, std::io::Error> {
    let input_source = InputSource::for_request_url(input, request_url.to_string());
    let (content, response_url, response_headers) =
        download_legacy_hls_manifest(app_state, &input_source, headers).await?;
    let playlist_kind = classify_hls_playlist(&content);
    Ok((content, response_url, response_headers, playlist_kind))
}

/// Stream channel of `virtual_id` in the target's Xtream output, else its M3U output.
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
    if let Ok(pli) = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target).await {
        return Some(pli.to_stream_channel(target_id));
    }
    let classified =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(virtual_id)).await;
    match classified {
        ResolvedStreamTarget::Member { group, index } => {
            let member = &group.members[index];
            Some(StreamChannel {
                target_id,
                virtual_id: member.member_vid.get(),
                group_virtual_id: Some(group.group_vid.get()),
                provider_id: member.provider_id,
                input_name: Arc::clone(&member.input_name),
                item_type: member.item_type,
                cluster: member.cluster,
                group: Arc::from(""),
                title: Arc::clone(&member.title),
                url: Arc::clone(&member.url),
                epg_channel_id: member.epg_channel_id.clone(),
                upstream_user_agent: member.upstream_user_agent.clone(),
                ..StreamChannel::default()
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod playback_provider_tests {
    use super::*;
    use crate::model::ConfigProvider;
    use shared::model::{ConfigProviderDto, InputType, ProviderUrlSelectionPolicy};

    /// M3U input whose playlist URL is plain HTTP and whose `cdn` profile serves playback only.
    fn independent_playlist_input() -> ConfigInput {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "cdn".into(),
            urls: vec!["http://pl.example.com".into(), "http://eu.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        ConfigInput {
            name: "Provider".into(),
            input_type: InputType::M3u,
            url: "http://playlist.example.com/list.m3u?access_key=fixture".into(),
            playback_provider: Some("cdn".into()),
            provider_configs: Some(vec![provider]),
            ..ConfigInput::default()
        }
    }

    /// A `provider://cdn` manifest binds the playback profile although the playlist download stays unbound.
    #[test]
    fn standard_hls_binds_explicit_profile_for_token_only_mapped_manifest() {
        let input = independent_playlist_input();
        assert!(InputSource::from(&input).provider.is_none(), "playlist download must remain independent");
        let url = "provider://cdn/ChannelA/mono.m3u8?token=fixture%2Btoken&keep=1";
        let source = InputSource::for_request_url(&input, url.into());
        assert_eq!(source.url, url);
        let profile = source.provider.expect("mapped standard-HLS URL must retain its registered CDN profile");
        assert_eq!(profile.name.as_ref(), "cdn");
        assert_eq!(profile.urls.len(), 2);
    }

    /// A plain HTTP manifest URL gets no profile.
    #[test]
    fn standard_hls_keeps_plain_http_manifest_unbound() {
        let input = independent_playlist_input();
        let url = "http://pl.example.com/ChannelA/mono.m3u8?token=fixture%2Btoken";
        let source = InputSource::for_request_url(&input, url.into());
        assert_eq!(source.url, url);
        assert!(source.provider.is_none());
    }

    /// An unknown profile name does not fall back to the playback profile.
    #[test]
    fn standard_hls_unknown_profile_does_not_borrow_another_profile() {
        let input = independent_playlist_input();
        let source = InputSource::for_request_url(&input, "provider://unknown/ChannelA/mono.m3u8".into());
        assert!(source.provider.is_none());
    }

    /// An unknown profile name does not fall back to the playlist URL's profile.
    #[test]
    fn standard_hls_unknown_profile_does_not_reuse_the_playlist_profile() {
        let mut input = independent_playlist_input();
        input.url = "provider://cdn/playlist.m3u?access_key=fixture".into();
        assert!(InputSource::from(&input).provider.is_some());
        let source = InputSource::for_request_url(&input, "provider://unknown/ChannelA/mono.m3u8".into());
        assert!(source.provider.is_none(), "an unknown mapped profile must fail closed");
    }

    /// A fresh Stalker link drops the old host's session cookie and keeps unrelated headers.
    #[test]
    fn fresh_link_rebinds_the_session_cookie() {
        use axum::http::{header, HeaderValue};
        let stale = || {
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_static("old-host=1"));
            headers.insert(header::USER_AGENT, HeaderValue::from_static("player"));
            headers
        };

        // No session cookie for the new host and no request cookie before: no cookie is sent.
        let mut headers = stale();
        rebind_session_cookie(&mut headers, None, None);
        assert!(headers.get(header::COOKIE).is_none());
        assert_eq!(headers.get(header::USER_AGENT), Some(&HeaderValue::from_static("player")));

        // A configured request cookie comes back.
        let mut headers = stale();
        rebind_session_cookie(&mut headers, Some(HeaderValue::from_static("configured=1")), None);
        assert_eq!(headers.get(header::COOKIE), Some(&HeaderValue::from_static("configured=1")));

        // The session cookie of the new host is applied.
        let mut headers = stale();
        let fresh = std::collections::HashMap::from([(header::COOKIE.as_str().to_string(), "new-host=2".to_string())]);
        rebind_session_cookie(&mut headers, None, Some(&fresh));
        assert_eq!(headers.get(header::COOKIE), Some(&HeaderValue::from_static("new-host=2")));
    }

    /// Only link rejections (most 4xx, non-playlist body) trigger a fresh `create_link`.
    #[test]
    fn pinned_stalker_playlist_retry_triggers_only_on_rejection() {
        let manifest = |kind| Ok((String::new(), String::new(), HeaderMap::new(), kind));
        let http_err =
            |status| std::io::Error::other(request::request_failed_message(status, "http://example.com/live.m3u8"));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::FORBIDDEN))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::NOT_FOUND))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::INTERNAL_SERVER_ERROR))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::SERVICE_UNAVAILABLE))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::UNAUTHORIZED))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::GONE))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::TOO_MANY_REQUESTS))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::REQUEST_TIMEOUT))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::PROXY_AUTHENTICATION_REQUIRED))));
        assert!(!is_rejected_hls_manifest_download(&Err(std::io::Error::from(std::io::ErrorKind::TimedOut))));
        assert!(!is_rejected_hls_manifest_download(&Err(std::io::Error::other("connection reset by peer"))));
        assert!(is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Invalid)));
        assert!(!is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Media)));
        assert!(!is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Master)));
        assert!(super::hls_download_rejects_link(Err(&http_err(StatusCode::FORBIDDEN))));
        assert!(super::hls_download_rejects_link(Ok(HlsPlaylistKind::Invalid)));
        assert!(!super::hls_download_rejects_link(Ok(HlsPlaylistKind::Media)));
        assert!(!super::hls_download_rejects_link(Err(&http_err(StatusCode::INTERNAL_SERVER_ERROR))));
    }

    #[tokio::test]
    async fn get_stream_channel_member_preserves_upstream_user_agent() {
        let env = crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv::new(&["solis", "korath"])
            .build()
            .await;
        let member_vid = env.member_vid("solis");
        let target_path =
            crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
                .expect("target storage path");
        let mut group = (*env.group_record).clone();
        group.members[0].upstream_user_agent = Some(Arc::from("SolisCustomAgent/2.1"));
        tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
            .expect("write group with user agent");
        env.app_state.playlists.bump_failover_group_generation(&env.target.name);

        let channel = get_stream_channel(&env.app_state, &env.target, member_vid).await.expect("member channel");
        assert_eq!(channel.upstream_user_agent, Some(Arc::from("SolisCustomAgent/2.1")));
    }

    #[tokio::test]
    async fn handle_hls_stream_request_member_vid_live_entry_uses_prefer() {
        let env = crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv::new(&["solis", "korath"])
            .hls()
            .build()
            .await;
        // Member 0 is solis, member 1 is korath.
        let korath_member_vid = env.member_vid("korath");
        let response = env.get_hls_manifest(korath_member_vid).await;
        assert_eq!(response.status(), StatusCode::OK);
        // prefer: Some(1) selects korath instead of default member 0 (solis)
        assert_eq!(env.origin_hits("solis"), 0);
        assert_eq!(env.origin_hits("korath"), 1);
        assert_eq!(env.bound_member_input("viewer").await, "korath");
    }
}
