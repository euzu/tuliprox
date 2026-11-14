use crate::{
    api::{
        api_utils::{
            admission_failure_response, archive_member_order, blocks_new_playback, coalesce_byte_stream,
            create_m3u_catchup_session_key, create_playback_session_fingerprint, create_session_fingerprint,
            failover_group::{handle_stream_failure_for_group, resolve_playback_source},
            force_provider_stream_response, get_session_reservation_ttl_secs, get_user_target,
            get_user_target_by_credentials, is_running_playback, is_seekable_media_request, is_session_based_playback,
            is_stream_share_enabled, local_stream_response, maintenance_rejection_response, redirect_response,
            reentry_suppressed_response, resolve_initial_stalker_playback_url, resource_proxy_response,
            resource_redirect_or_proxy, separate_number_and_remainder, should_allow_exhausted_shared_reconnect,
            stream_response_with_provider_handle, try_option_bad_request, try_result_bad_request, try_result_not_found,
            try_unwrap_body, PlaybackSource, RedirectParams, StreamPlaybackOutcome,
        },
        endpoints::{
            hls_api::{
                build_virtual_hls_entry_path, handle_hls_stream_request, hls_admission_failure_manifest_response,
                hls_custom_video_manifest_response, m3u_archive_epg_reference_ts, user_allows_entry_content,
                HlsEntryOutputScope, HlsEntryStreamContext, HlsRequestStage,
            },
            xtream_api::{ApiStreamContext, ApiStreamRequest},
        },
        model::{AppState, UserApiRequest, UserApiRequestQueryOrBody},
    },
    auth::{ApiUserAuthError, Fingerprint},
    iptv::m3u::{
        build_provider_flussonic_archive_url, decode_m3u_catchup_token, has_m3u_catchup_marker, is_flussonic_live_file,
        parse_flat_flussonic_archive, parse_flussonic_archive_file, resolve_m3u_catchup_url, FlussonicArchiveKind,
        M3uCatchupToken,
    },
    model::{AppConfig, ConfigTarget, ProxyUserCredentials},
    repository::{m3u_get_item_for_stream_id, m3u_load_rewrite_playlist, storage_const, PlaylistStorageState},
    utils::{debug_if_enabled, PROVIDER_SCHEME_PREFIX},
};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::StreamExt;
use log::{debug, error};
use shared::{
    defaults::HLS_EXT,
    error::TuliproxError,
    model::{
        CatchupProperties, ConnectFailureReason, FieldGetAccessor, PlaylistEntry, PlaylistItemType, StreamProperties,
        TargetType, UserConnectionPermission,
    },
    utils::{concat_path, extract_extension_from_url, sanitize_sensitive_info},
};
use std::{borrow::Cow, sync::Arc};
use tuliprox_repository::target_stream_item::{classify_virtual_id, member_to_m3u_item, ResolvedStreamTarget};
use tuliprox_session::failover_group::FailureClass;

async fn m3u_api(
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    app_state: &AppState,
    content_type: &str,
) -> impl IntoResponse + Send {
    let _guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    match m3u_load_rewrite_playlist(&app_state.app_config, &target, &user).await {
        Ok(m3u_iter) => {
            let content_stream = m3u_iter.map(|line| {
                line.map(|mut line| {
                    line.push('\n');
                    Bytes::from(line)
                })
            });

            let mut builder = axum::response::Response::builder().status(axum::http::StatusCode::OK).header(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            if content_type == "m3u_plus" {
                builder =
                    builder.header(axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"playlist.m3u\"");
            }
            try_unwrap_body!(builder.body(axum::body::Body::from_stream(coalesce_byte_stream(content_stream))))
        }
        Err(err) => {
            error!("{}", sanitize_sensitive_info(&err.to_string()));
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
    }
}

fn m3u_api_with_auth(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
) -> Result<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>), ApiUserAuthError> {
    let (user, target) = get_user_target(api_req, app_state).ok_or(ApiUserAuthError::AuthFailed)?;
    app_state.check_network_access(&user, fingerprint)?;
    Ok((user, target))
}

/// Network-only auth for stream endpoints. Permission check is done later by the stream
/// handler with full stream info for `admission_failure_response`.
fn m3u_api_stream_network_auth(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    stream_req: &ApiStreamRequest<'_>,
) -> Result<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>), ApiUserAuthError> {
    let (user, target) = get_user_target_by_credentials(stream_req.username, stream_req.password, api_req, app_state)
        .ok_or(ApiUserAuthError::AuthFailed)?;
    app_state.check_network_access(&user, fingerprint)?;
    Ok((user, target))
}

async fn m3u_api_get(
    fingerprint: Fingerprint,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = match m3u_api_with_auth(&fingerprint, &app_state, &api_req) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_player_response(auth_status),
    };
    m3u_api(user, target, &app_state, &api_req.content_type).await.into_response()
}

async fn m3u_api_post(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = match m3u_api_with_auth(&fingerprint, &app_state, &api_req) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_player_response(auth_status),
    };
    m3u_api(user, target, &app_state, &api_req.content_type).await.into_response()
}

fn effective_playback_extension<'a>(
    item_type: PlaylistItemType,
    stream_url: &'a str,
    requested_extension: Option<&'a str>,
) -> Option<&'a str> {
    let upstream_extension = extract_extension_from_url(stream_url);
    if item_type == PlaylistItemType::Catchup {
        upstream_extension.or(requested_extension)
    } else {
        requested_extension.or(upstream_extension)
    }
}

pub(crate) fn m3u_stream_session_key(
    fingerprint: &Fingerprint,
    username: &str,
    session_vid: u32,
    pli: &shared::model::M3uPlaylistItem,
    stream_ext: Option<&str>,
    archive_discriminator: Option<&str>,
) -> String {
    let effective_stream_ext = effective_playback_extension(pli.item_type, &pli.url, stream_ext);
    let extension = effective_stream_ext.unwrap_or_default();
    if pli.item_type == PlaylistItemType::Catchup {
        create_m3u_catchup_session_key(fingerprint, username, session_vid, archive_discriminator.unwrap_or("live"))
    } else {
        create_playback_session_fingerprint(fingerprint, username, session_vid, pli.item_type, Some(extension))
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn m3u_api_stream_loaded(
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    fingerprint: &Fingerprint,
    req_headers: &axum::http::HeaderMap,
    app_state: &Arc<AppState>,
    mut pli: shared::model::M3uPlaylistItem,
    input: Arc<crate::model::ConfigInput>,
    stream_ext: Option<&str>,
    archive_discriminator: Option<&str>,
    provider_allocation_id: Option<u64>,
    source: Option<PlaybackSource<'_>>,
    resolved_target: Option<ResolvedStreamTarget>,
) -> StreamPlaybackOutcome {
    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return StreamPlaybackOutcome::failure(
            axum::http::StatusCode::BAD_REQUEST.into_response(),
            FailureClass::Stream,
        );
    }

    let is_hls_manifest_request = effective_playback_extension(pli.item_type, &pli.url, stream_ext) == Some(HLS_EXT);

    if !user_allows_entry_content(&user, HlsEntryOutputScope::ItemType(pli.item_type), &pli) {
        if is_hls_manifest_request {
            return StreamPlaybackOutcome::failure(
                hls_custom_video_manifest_response(
                    app_state,
                    &user,
                    crate::api::model::CustomVideoStreamType::ChannelUnavailable,
                    axum::http::StatusCode::FORBIDDEN,
                )
                .await,
                FailureClass::Stream,
            );
        }
        return StreamPlaybackOutcome::failure(
            crate::api::model::create_custom_video_stream_response(
                &app_state.provider_stream_ctx(),
                &fingerprint.addr,
                crate::api::model::CustomVideoStreamType::ChannelUnavailable,
            )
            .into_response(),
            FailureClass::Stream,
        );
    }
    let virtual_id = pli.virtual_id;

    let resolved_target = match resolved_target {
        Some(target) => target,
        None => classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, virtual_id).await,
    };
    let is_group = source
        .as_ref()
        .map_or_else(|| resolved_target.group().is_some(), |s| matches!(s, PlaybackSource::Group { .. }));

    let check_vid = resolved_target.group().map_or(virtual_id, |group| group.group_vid);
    if app_state.active_users.is_user_blocked_for_stream(&user.username, check_vid).await {
        return StreamPlaybackOutcome::failure(
            axum::http::StatusCode::BAD_REQUEST.into_response(),
            FailureClass::Stream,
        );
    }

    if user.permission_denied(&app_state.app_config) {
        if is_hls_manifest_request {
            return StreamPlaybackOutcome::failure(
                hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    pli.to_stream_channel(target.id),
                    pli.input_name.clone(),
                    req_headers,
                    ConnectFailureReason::UserAccountExpired,
                )
                .await,
                FailureClass::Capacity,
            );
        }
        return StreamPlaybackOutcome::failure(
            admission_failure_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                pli.input_name.clone(),
                req_headers,
                ConnectFailureReason::UserAccountExpired,
            ),
            FailureClass::Capacity,
        );
    }

    if pli.item_type.is_local() {
        let playback_session_token = create_session_fingerprint(fingerprint, &user.username, virtual_id.get(), false);
        let user_session =
            app_state.active_users.get_and_update_user_session(&user.username, &playback_session_token).await;
        let (admission, _grace_mode, request_class) = crate::api::api_utils::resolve_playback_request_admission(
            &app_state.admission_ctx(),
            &user,
            fingerprint,
            user_session.as_ref(),
            playback_session_token.as_str(),
            false,
            crate::api::api_utils::EvictionReentryGuard::Session(playback_session_token.as_str()),
            false,
            false,
        )
        .await;
        if admission.is_reentry_suppressed() {
            return StreamPlaybackOutcome::failure(reentry_suppressed_response(), FailureClass::Capacity);
        }
        return StreamPlaybackOutcome::success(
            local_stream_response(
                fingerprint,
                app_state,
                pli.to_stream_channel(target.id),
                req_headers,
                &input,
                &target,
                &user,
                admission.permission(),
                admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
                Some(playback_session_token.as_str()),
                Some(request_class),
                true,
            )
            .await
            .into_response(),
        );
    }

    let cluster = pli.item_type.cluster();
    if !is_group {
        pli.url = match resolve_initial_stalker_playback_url(
            app_state,
            &input,
            &user,
            pli.get_provider_id().unwrap_or_default(),
            cluster,
            pli.item_type,
            &pli.url,
        )
        .await
        {
            Ok(url) => url,
            Err(err) => {
                error!("Failed to resolve initial Stalker playback URL: {}", sanitize_sensitive_info(&err.to_string()));
                return StreamPlaybackOutcome::failure(
                    axum::http::StatusCode::BAD_GATEWAY.into_response(),
                    FailureClass::Transport,
                );
            }
        };
    }

    let stream_channel = || {
        let mut channel = pli.to_stream_channel(target.id);
        if input.input_type == shared::model::InputType::Xtream && pli.item_type == PlaylistItemType::Video {
            if let Some(provider_id) = pli.get_input_stream_id().and_then(|id| id.parse().ok()) {
                channel.provider_id = provider_id;
            }
        }
        if let Some(group) = resolved_target.group() {
            channel.group_virtual_id = Some(group.group_vid.get());
        }
        channel
    };

    debug_if_enabled!("M3U playback for virtual_id={virtual_id}, item_type={}", pli.item_type);
    let effective_stream_ext = effective_playback_extension(pli.item_type, &pli.url, stream_ext);
    let extension = effective_stream_ext.unwrap_or_default();
    let session_vid = resolved_target.group().map_or(virtual_id.get(), |g| g.group_vid.get());
    let session_key =
        m3u_stream_session_key(fingerprint, &user.username, session_vid, &pli, stream_ext, archive_discriminator);
    let eviction_reentry_guard = if pli.item_type == PlaylistItemType::Catchup
        || !crate::api::api_utils::is_socket_bound_playback_session(pli.item_type, Some(extension))
    {
        crate::api::api_utils::EvictionReentryGuard::Session(&session_key)
    } else {
        crate::api::api_utils::EvictionReentryGuard::SocketPlayback { virtual_id: pli.virtual_id }
    };
    let user_session = app_state.active_users.get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            if extension == HLS_EXT {
                return StreamPlaybackOutcome::failure(
                    hls_admission_failure_manifest_response(
                        app_state,
                        fingerprint,
                        &user,
                        stream_channel(),
                        session.provider.clone(),
                        req_headers,
                        ConnectFailureReason::UserConnectionsExhausted,
                    )
                    .await,
                    FailureClass::Capacity,
                );
            }
            return StreamPlaybackOutcome::failure(
                admission_failure_response(
                    app_state,
                    fingerprint,
                    &user,
                    stream_channel(),
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::UserConnectionsExhausted,
                ),
                FailureClass::Capacity,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider) {
            if extension == HLS_EXT {
                return StreamPlaybackOutcome::failure(
                    hls_admission_failure_manifest_response(
                        app_state,
                        fingerprint,
                        &user,
                        stream_channel(),
                        session.provider.clone(),
                        req_headers,
                        ConnectFailureReason::ProviderConnectionsExhausted,
                    )
                    .await,
                    FailureClass::Capacity,
                );
            }
            return StreamPlaybackOutcome::failure(
                admission_failure_response(
                    app_state,
                    fingerprint,
                    &user,
                    stream_channel(),
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::ProviderConnectionsExhausted,
                ),
                FailureClass::Capacity,
            );
        }
        if session.virtual_id == virtual_id.get() && is_seekable_media_request(cluster, req_headers, Some(extension)) {
            // partial request means we are in reverse proxy mode, seek happened
            let (input, stream_channel) =
                crate::api::api_utils::sticky_playback_context(app_state, session, &input, stream_channel());
            return StreamPlaybackOutcome::success(
                force_provider_stream_response(
                    fingerprint,
                    app_state,
                    session,
                    stream_channel,
                    crate::api::api_utils::ForceStreamRequestContext {
                        req_headers,
                        input: &input,
                        user: &user,
                        session_reservation_ttl_secs: get_session_reservation_ttl_secs(app_state, pli.item_type),
                        content_representation:
                            crate::api::model::ProviderContentRepresentationMode::for_playback_extension(extension),
                    },
                    None,
                )
                .await
                .into_response(),
            );
        }
        if pli.item_type == PlaylistItemType::Catchup {
            pli.url.clone()
        } else {
            session.stream_url.clone()
        }
    } else {
        pli.url.clone()
    };

    // A group checks maintenance per member during selection; its representative input says nothing.
    if !is_group && !is_running_playback(user_session.as_ref()) && blocks_new_playback(app_state, &input, &user, false)
    {
        return StreamPlaybackOutcome::failure(
            maintenance_rejection_response(
                app_state,
                fingerprint,
                &user,
                stream_channel(),
                &input,
                req_headers,
                extension == HLS_EXT,
            )
            .await,
            FailureClass::Stream,
        );
    }

    let (connection_admission, grace_mode, request_class) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        fingerprint,
        user_session.as_ref(),
        &session_key,
        false,
        eviction_reentry_guard,
        false,
        false,
    )
    .await;
    let connection_permission = connection_admission.permission();
    let connection_kind = connection_admission
        .kind()
        .or(user_session.as_ref().and_then(|session| session.connection_kind))
        .unwrap_or(crate::api::model::ConnectionKind::Normal);
    let allow_exhausted_shared_reconnect = should_allow_exhausted_shared_reconnect(
        is_stream_share_enabled(pli.item_type, &target),
        user_session.as_ref(),
        virtual_id.get(),
        session_url.as_ref(),
    );
    if connection_permission == UserConnectionPermission::Exhausted && !allow_exhausted_shared_reconnect {
        if connection_admission.is_reentry_suppressed() {
            return StreamPlaybackOutcome::failure(reentry_suppressed_response(), FailureClass::Capacity);
        }
        if extension == HLS_EXT {
            return StreamPlaybackOutcome::failure(
                hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    stream_channel(),
                    input.name.clone(),
                    req_headers,
                    ConnectFailureReason::UserConnectionsExhausted,
                )
                .await,
                FailureClass::Capacity,
            );
        }
        return StreamPlaybackOutcome::failure(
            admission_failure_response(
                app_state,
                fingerprint,
                &user,
                stream_channel(),
                input.name.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            ),
            FailureClass::Capacity,
        );
    }

    let context = ApiStreamContext::try_from(cluster).unwrap_or(ApiStreamContext::Live);

    let redirect_params = RedirectParams {
        item: &pli,
        provider_id: pli.get_provider_id(),
        cluster,
        target_type: TargetType::M3u,
        target: &target,
        input: &input,
        user: &user,
        stream_ext: effective_stream_ext,
        req_context: context,
        action_path: "", // TODO is there timeshift or something like that ?
    };

    if let Some(response) = redirect_response(app_state, &redirect_params).await {
        // Archive/catchup must keep the resolved provider archive URL — do not redirect to live.
        if archive_discriminator.is_none() && pli.item_type != PlaylistItemType::Catchup {
            return StreamPlaybackOutcome::success(response.into_response());
        }
    }

    let is_session_request = is_session_based_playback(pli.item_type, Some(extension));
    // The archive/catchup EPG reference timestamp is parsed from the request
    // URL once and shared between the HLS and the non-HLS branch. The HLS
    // handler threads it into its own manifest construction; the non-HLS
    // branch attaches it to the StreamChannel so the frontend's stream_epg
    // request can centre its EPG window on the archive timestamp instead of
    // falling back to `now`.
    let archive_reference = m3u_archive_epg_reference_ts(&pli.url).or_else(|| {
        archive_discriminator.and_then(|disc| {
            disc.strip_prefix("archive|")
                .or_else(|| disc.strip_prefix("timeshift_abs|"))
                .and_then(|rest| rest.split('|').next())
                .and_then(|start| start.parse::<i64>().ok())
        })
    });
    // Reverse proxy mode — only route genuine HLS into the HLS handler, not DASH
    if is_session_request && extension == shared::defaults::HLS_EXT {
        // HLS owns capacity per manifest/segment session rather than for one
        // long-lived provider body. Release the worker's admission reservation
        // before the HLS machinery performs its own concrete acquisition.
        if let Some(allocation_id) = provider_allocation_id {
            app_state.active_provider.complete_release(allocation_id);
        }
        let Some(stream_context) = HlsEntryStreamContext::from_playlist_item(&pli) else {
            error!("HLS input stream identity missing for virtual_id={}; refresh target playlist", pli.virtual_id);
            return StreamPlaybackOutcome::failure(
                axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                FailureClass::Capacity,
            );
        };
        let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, pli.virtual_id.get());
        return StreamPlaybackOutcome::success(
            handle_hls_stream_request(
                fingerprint,
                app_state,
                &user,
                &target,
                user_session.as_ref(),
                Some(session_key.as_str()),
                &pli.url,
                archive_reference,
                stream_context,
                &input,
                req_headers,
                connection_permission,
                Some(connection_kind),
                &original_hls_entry_path,
                HlsRequestStage::Entry,
            )
            .await
            .into_response(),
        );
    }

    let pinned_provider =
        user_session.as_ref().filter(|_| pli.item_type.requires_provider_affinity()).map(|session| &session.provider);
    let preacquired_provider_handle = if let Some(allocation_id) = provider_allocation_id {
        let Some(handle) = app_state.active_provider.claim_download_connection(allocation_id, &input.name) else {
            return StreamPlaybackOutcome::failure(
                axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                FailureClass::Capacity,
            );
        };
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), handle))
    } else {
        None
    };

    let bound_prefer = user_session
        .as_ref()
        .and_then(|session| session.member_binding.as_ref())
        .map(|b| (b.group_vid, b.member_index));

    let is_catchup = archive_discriminator.is_some() || pli.item_type == PlaylistItemType::Catchup;
    let is_live = !is_catchup && provider_allocation_id.is_none();
    let bound_prefer_idx =
        bound_prefer.and_then(|(gvid, idx)| resolved_target.group().filter(|g| g.group_vid.get() == gvid).map(|_| idx));
    let mut source = source
        .unwrap_or_else(|| resolve_playback_source(&resolved_target, is_live, &input, &session_url, bound_prefer_idx));

    if let PlaybackSource::Group { group, prefer, .. } = &mut source {
        if prefer.is_none() {
            if let Some((gvid, idx)) = bound_prefer {
                if gvid == group.group_vid.get() {
                    *prefer = Some(idx);
                }
            }
        }
    }

    stream_response_with_provider_handle(
        fingerprint,
        app_state,
        &session_key,
        Some(request_class),
        stream_channel().with_epg_reference_ts(archive_reference),
        source,
        pinned_provider,
        req_headers,
        &target,
        &user,
        connection_permission,
        connection_kind,
        allow_exhausted_shared_reconnect,
        grace_mode,
        preacquired_provider_handle,
    )
    .await
}

/// Source URL for archive construction; a playback-profile URL stays unresolved so failover still picks the host.
fn resolve_effective_source_url<'a>(
    item: &'a shared::model::M3uPlaylistItem,
    input: &'a crate::model::ConfigInput,
) -> Result<Cow<'a, str>, TuliproxError> {
    if !item.url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Ok(Cow::Borrowed(item.url.as_ref()));
    }
    if input
        .playback_provider
        .as_deref()
        .is_some_and(|name| input.get_resolve_provider(&item.url).is_some_and(|profile| profile.name.as_ref() == name))
    {
        // Archive construction must keep the explicit media profile until fetch
        // time, otherwise the URL is pinned to the first CDN before failover.
        return Ok(Cow::Borrowed(item.url.as_ref()));
    }
    input.resolve_url(&item.url)
}

fn resolve_m3u_catchup_properties<'a>(
    item_type: PlaylistItemType,
    time_shift: &'a Arc<str>,
    additional_properties: Option<&'a StreamProperties>,
) -> Option<Cow<'a, CatchupProperties>> {
    if let Some(StreamProperties::Live(live)) = additional_properties {
        if let Some(catchup) = live.catchup.as_ref() {
            return Some(Cow::Borrowed(catchup));
        }
    }

    (item_type.is_live() && !time_shift.trim().is_empty()).then(|| {
        Cow::Owned(CatchupProperties {
            mode: Some(Arc::from("fs")),
            days: Some(Arc::clone(time_shift)),
            ..CatchupProperties::default()
        })
    })
}

fn playlist_item_native_flussonic_mode(item: &shared::model::M3uPlaylistItem) -> Option<&'static str> {
    match item.additional_properties.as_ref()? {
        StreamProperties::Live(live) => live.catchup.as_ref()?.native_flussonic_player_mode(),
        _ => None,
    }
}

/// Accept Flussonic archive routes when catchup-type is Flussonic, or when a row only has timeshift.
fn playlist_item_allows_flussonic_archive(item: &shared::model::M3uPlaylistItem) -> bool {
    if playlist_item_native_flussonic_mode(item).is_some() {
        return true;
    }
    !item.time_shift.trim().is_empty()
        && item.additional_properties.as_ref().is_none_or(|props| match props {
            StreamProperties::Live(live) => live.catchup.as_ref().is_none_or(CatchupProperties::is_empty),
            _ => true,
        })
}

fn playlist_item_flussonic_mode_or_default(item: &shared::model::M3uPlaylistItem) -> Option<&'static str> {
    playlist_item_native_flussonic_mode(item)
        .or_else(|| playlist_item_allows_flussonic_archive(item).then_some("flussonic"))
}

fn native_live_file_matches_mode(mode: &str, file: &str) -> bool {
    let supported = if mode == "flussonic-ts" {
        &["mpegts", "index.ts", "video.ts", "mono.ts"][..]
    } else {
        &["index.m3u8", "video.m3u8", "mono.m3u8"]
    };
    supported.iter().any(|supported_file| file.trim().eq_ignore_ascii_case(supported_file))
}

fn native_archive_matches_mode(mode: &str, archive: &FlussonicArchiveKind) -> bool {
    let expected_extension = if mode == "flussonic-ts" { ".ts" } else { HLS_EXT };
    matches!(mode, "flussonic" | "flussonic-ts") && archive.extension() == expected_extension
}

fn apply_native_flussonic_archive(
    item: &shared::model::M3uPlaylistItem,
    input: &crate::model::ConfigInput,
    archive: &FlussonicArchiveKind,
) -> Result<(shared::model::M3uPlaylistItem, String), TuliproxError> {
    let source_url = resolve_effective_source_url(item, input)?;
    let archive_url = build_provider_flussonic_archive_url(source_url.as_ref(), archive)
        .ok_or_else(|| TuliproxError::RepositoryM3u("Failed to build native Flussonic archive URL".to_string()))?;
    let mut catchup_item = item.clone();
    catchup_item.item_type = PlaylistItemType::Catchup;
    catchup_item.url = archive_url.into();
    Ok((catchup_item, archive.discriminator()))
}

fn resolve_loaded_m3u_catchup(
    pli: &shared::model::M3uPlaylistItem,
    input: &crate::model::ConfigInput,
    raw_query: Option<&str>,
) -> Result<(shared::model::M3uPlaylistItem, String), TuliproxError> {
    let Some(catchup) =
        resolve_m3u_catchup_properties(pli.item_type, &pli.time_shift, pli.additional_properties.as_ref())
    else {
        if pli.item_type.is_live() {
            return Err(TuliproxError::RepositoryM3u("M3U catchup metadata missing".to_string()));
        }
        return Err(TuliproxError::RepositoryM3u("M3U catchup requested for non-live stream".to_string()));
    };
    let source_url = resolve_effective_source_url(pli, input)?;
    let Some(resolved) = resolve_m3u_catchup_url(source_url.as_ref(), catchup.as_ref(), raw_query)? else {
        return Err(TuliproxError::RepositoryM3u("M3U catchup mode cannot be proxied".to_string()));
    };

    let mut catchup_item = pli.clone();
    catchup_item.item_type = PlaylistItemType::Catchup;
    catchup_item.url = resolved.url.into();
    Ok((catchup_item, resolved.discriminator))
}

fn resolved_m3u_item_is_allowed(user: &ProxyUserCredentials, item_type: PlaylistItemType) -> bool {
    user.allows_item_type(item_type)
}

async fn resolve_m3u_stream_item_and_target(
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    req_virtual_id: u32,
) -> Result<(shared::model::M3uPlaylistItem, ResolvedStreamTarget), std::io::Error> {
    let resolved_target =
        classify_virtual_id(app_config, playlists, target, shared::model::VirtualId::new(req_virtual_id)).await;
    let pli_res = m3u_get_item_for_stream_id(req_virtual_id, app_config.as_ref(), playlists, target).await;
    let pli = match pli_res {
        Ok(pli) => pli,
        Err(err) => match &resolved_target {
            ResolvedStreamTarget::Group(group) => member_to_m3u_item(group, 0),
            ResolvedStreamTarget::Member { group, index } => member_to_m3u_item(group, *index),
            ResolvedStreamTarget::Item => return Err(err),
        },
    };
    Ok((pli, resolved_target))
}

fn live_group_playback_source(resolved_target: &ResolvedStreamTarget) -> Option<PlaybackSource<'static>> {
    match resolved_target {
        ResolvedStreamTarget::Group(group) => Some(PlaybackSource::Group {
            group: Arc::clone(group),
            mode: tuliprox_session::failover_group::SelectionMode::Live,
            prefer: None,
        }),
        ResolvedStreamTarget::Member { group, index } => Some(PlaybackSource::Group {
            group: Arc::clone(group),
            mode: tuliprox_session::failover_group::SelectionMode::Live,
            prefer: Some(*index),
        }),
        ResolvedStreamTarget::Item => None,
    }
}

/// The group member that should serve an archive request starting at `start_ts`; `pli` when the target is no
/// group or no member qualifies.
fn archive_member_item(
    app_state: &AppState,
    target: &ConfigTarget,
    resolved_target: &ResolvedStreamTarget,
    pli: shared::model::M3uPlaylistItem,
    start_ts: i64,
) -> shared::model::M3uPlaylistItem {
    if let Some(group) = resolved_target.group() {
        let order = archive_member_order(app_state, target, group, start_ts);
        if let Some(&best_idx) = order.first() {
            return member_to_m3u_item(group, best_idx);
        }
    }
    pli
}

#[allow(clippy::too_many_lines)]
async fn m3u_api_stream(
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    fingerprint: &Fingerprint,
    req_headers: &axum::http::HeaderMap,
    app_state: &Arc<AppState>,
    stream_req: ApiStreamRequest<'_>,
    raw_query: Option<&str>,
) -> impl IntoResponse + Send {
    let _user_guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    if let Some((req_virtual_id, archive)) = parse_flat_flussonic_archive(stream_req.stream_id) {
        let pli = match m3u_get_item_for_stream_id(req_virtual_id, &app_state.app_config, &app_state.playlists, &target)
            .await
        {
            Ok(pli) => pli,
            Err(err) => {
                error!("Failed to read M3U item for native archive stream id {req_virtual_id}: {err}");
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        };
        let Some(mode) = playlist_item_flussonic_mode_or_default(&pli) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        if !native_archive_matches_mode(mode, &archive) {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        let input = try_option_bad_request!(
            app_state.app_config.get_input_by_name(&pli.input_name),
            true,
            format!("Can't find input {} for target {target_name}, stream_id {}", pli.input_name, pli.virtual_id)
        );
        let (resolved_pli, discriminator) = match apply_native_flussonic_archive(&pli, &input, &archive) {
            Ok(result) => result,
            Err(err) => {
                debug!("Failed to resolve native M3U archive: {}", sanitize_sensitive_info(&err.to_string()));
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        };
        return m3u_api_stream_loaded(
            user,
            target,
            fingerprint,
            req_headers,
            app_state,
            resolved_pli,
            input,
            Some(archive.extension()),
            Some(&discriminator),
            None,
            None,
            None,
        )
        .await
        .into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let (pli, resolved_target) =
        match resolve_m3u_stream_item_and_target(&app_state.app_config, &app_state.playlists, &target, req_virtual_id)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                error!("Failed to read m3u item for stream id {req_virtual_id}: {err}");
                if stream_ext == Some(HLS_EXT) {
                    return axum::http::StatusCode::NOT_FOUND.into_response();
                }
                return crate::api::model::create_custom_video_stream_response(
                    &app_state.provider_stream_ctx(),
                    &fingerprint.addr,
                    crate::api::model::CustomVideoStreamType::ChannelUnavailable,
                )
                .into_response();
            }
        };

    let (resolved_pli, archive_discriminator, input) = if has_m3u_catchup_marker(raw_query) {
        let start_ts =
            crate::iptv::m3u::extract_m3u_catchup_start_ts(raw_query).unwrap_or_else(|| chrono::Utc::now().timestamp());
        let catchup_pli = archive_member_item(app_state, &target, &resolved_target, pli, start_ts);
        let input = try_option_bad_request!(
            app_state.app_config.get_input_by_name(&catchup_pli.input_name),
            true,
            format!(
                "Can't find input {} for target {target_name}, stream_id {}",
                catchup_pli.input_name, catchup_pli.virtual_id
            )
        );
        match resolve_loaded_m3u_catchup(&catchup_pli, &input, raw_query) {
            Ok((item, discriminator)) => (item, Some(discriminator), input),
            Err(err) => {
                debug!("Failed to resolve M3U catchup request: {}", sanitize_sensitive_info(&err.to_string()));
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        }
    } else {
        let input = try_option_bad_request!(
            app_state.app_config.get_input_by_name(&pli.input_name),
            true,
            format!("Can't find input {} for target {target_name}, stream_id {}", pli.input_name, pli.virtual_id)
        );
        (pli, None, input)
    };
    let source = if archive_discriminator.is_some() { None } else { live_group_playback_source(&resolved_target) };
    m3u_api_stream_loaded(
        user,
        target,
        fingerprint,
        req_headers,
        app_state,
        resolved_pli,
        input,
        stream_ext,
        archive_discriminator.as_deref(),
        None,
        source,
        Some(resolved_target),
    )
    .await
    .into_response()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn m3u_api_stream_nested(
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    fingerprint: &Fingerprint,
    req_headers: &axum::http::HeaderMap,
    app_state: &Arc<AppState>,
    stream_req: ApiStreamRequest<'_>,
    file: &str,
    raw_query: Option<&str>,
) -> impl IntoResponse + Send {
    let _user_guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;
    if !target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let req_virtual_id: u32 = try_result_bad_request!(stream_req.stream_id.trim().parse());
    let (pli, resolved_target) =
        match resolve_m3u_stream_item_and_target(&app_state.app_config, &app_state.playlists, &target, req_virtual_id)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                error!("Failed to read M3U item for nested stream id {req_virtual_id}: {err}");
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        };
    let stream_ext = extract_extension_from_url(file);
    let archive = parse_flussonic_archive_file(file);
    let is_catchup = archive.is_none() && is_flussonic_live_file(file) && has_m3u_catchup_marker(raw_query);
    // Archive and catch-up requests of a group are served by the member that holds the archive.
    let archive_start_ts = match &archive {
        Some(FlussonicArchiveKind::Archive { start, .. } | FlussonicArchiveKind::TimeshiftAbs { start, .. }) => {
            Some(start.parse::<i64>().unwrap_or_else(|_| chrono::Utc::now().timestamp()))
        }
        Some(FlussonicArchiveKind::TimeshiftRel { ago, .. }) => {
            Some(chrono::Utc::now().timestamp().saturating_sub(ago.parse::<i64>().unwrap_or(0)))
        }
        None if is_catchup => Some(
            crate::iptv::m3u::extract_m3u_catchup_start_ts(raw_query).unwrap_or_else(|| chrono::Utc::now().timestamp()),
        ),
        None => None,
    };
    let pli = match archive_start_ts {
        Some(start_ts) => archive_member_item(app_state, &target, &resolved_target, pli, start_ts),
        None => pli,
    };
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}, stream_id {}", pli.input_name, target.name, pli.virtual_id)
    );
    let resolved = if let Some(archive) = archive {
        let Some(mode) = playlist_item_flussonic_mode_or_default(&pli) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        if !native_archive_matches_mode(mode, &archive) {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        apply_native_flussonic_archive(&pli, &input, &archive).map(|(item, discriminator)| (item, Some(discriminator)))
    } else if is_flussonic_live_file(file) {
        let Some(mode) = playlist_item_flussonic_mode_or_default(&pli) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        if !native_live_file_matches_mode(mode, file) {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        if is_catchup {
            resolve_loaded_m3u_catchup(&pli, &input, raw_query).map(|(item, discriminator)| (item, Some(discriminator)))
        } else {
            Ok((pli, None))
        }
    } else {
        Err(TuliproxError::RepositoryM3u(format!("Unsupported nested M3U stream file: {file}")))
    };
    let (resolved_pli, archive_discriminator) = match resolved {
        Ok(result) => result,
        Err(err) => {
            debug!("Failed to resolve nested M3U stream: {}", sanitize_sensitive_info(&err.to_string()));
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };
    let source = if archive_discriminator.is_some() { None } else { live_group_playback_source(&resolved_target) };
    m3u_api_stream_loaded(
        user,
        target,
        fingerprint,
        req_headers,
        app_state,
        resolved_pli,
        input,
        stream_ext,
        archive_discriminator.as_deref(),
        None,
        source,
        Some(resolved_target),
    )
    .await
    .into_response()
}

#[allow(clippy::too_many_lines)]
async fn m3u_api_catchup(
    fingerprint: Fingerprint,
    req_headers: axum::http::HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    axum::extract::Path(token): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let secret = app_state.get_encrypt_secret();
    let decoded = match decode_m3u_catchup_token(&secret, &token) {
        Ok(decoded) => decoded,
        Err(err) => {
            debug!("Invalid M3U catchup token: {err}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };
    let M3uCatchupToken { username, target_id, virtual_id } = decoded;

    let Some((user, target)) = app_state.app_config.get_target_for_username(&username) else {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    };
    if target.id != target_id {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    if let Err(err) = app_state.check_network_access(&user, &fingerprint) {
        return err.into_player_response(app_state.app_config.get_auth_error_status());
    }
    if user.permission_denied(&app_state.app_config) || !target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let _user_guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    let pli = try_result_not_found!(
        m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, &target).await,
        true,
        format!("Failed to read m3u item for stream id {virtual_id}")
    );

    let classified = classify_virtual_id(
        &app_state.app_config,
        &app_state.playlists,
        &target,
        shared::model::VirtualId::new(virtual_id),
    )
    .await;

    let candidates: Vec<shared::model::M3uPlaylistItem> = if let Some(group) = classified.group() {
        let start_ts = crate::iptv::m3u::extract_m3u_catchup_start_ts(raw_query.as_deref())
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        let order = archive_member_order(&app_state, &target, group, start_ts);
        if order.is_empty() {
            vec![pli]
        } else {
            order.into_iter().map(|idx| member_to_m3u_item(group, idx)).collect()
        }
    } else {
        vec![pli]
    };

    let num_candidates = candidates.len();
    let mut last_response = None;
    for (cand_idx, cand_pli) in candidates.into_iter().enumerate() {
        let is_last = cand_idx + 1 == num_candidates;
        let Some(input) = app_state.app_config.get_input_by_name(&cand_pli.input_name) else {
            if is_last {
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
            continue;
        };
        let (resolved_pli, archive_discriminator) =
            match resolve_loaded_m3u_catchup(&cand_pli, &input, raw_query.as_deref()) {
                Ok(result) => result,
                Err(err) => {
                    debug!(
                        "Failed to resolve M3U catchup token request: {}",
                        sanitize_sensitive_info(&err.to_string())
                    );
                    if is_last {
                        return axum::http::StatusCode::BAD_REQUEST.into_response();
                    }
                    continue;
                }
            };
        if !resolved_m3u_item_is_allowed(user.as_ref(), resolved_pli.item_type) {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }

        let outcome = m3u_api_stream_loaded(
            user.clone(),
            target.clone(),
            &fingerprint,
            &req_headers,
            &app_state,
            resolved_pli,
            input,
            None,
            Some(&archive_discriminator),
            None,
            None,
            Some(classified.clone()),
        )
        .await;

        if outcome.is_success() || is_last {
            return outcome.into_response();
        }

        let session_vid = classified.group().map_or(cand_pli.virtual_id.get(), |g| g.group_vid.get());
        let session_key =
            create_m3u_catchup_session_key(&fingerprint, &user.username, session_vid, &archive_discriminator);
        handle_stream_failure_for_group(
            &app_state,
            &user.username,
            session_key.as_str(),
            outcome.failure_class,
            classified.group().map(|g| &**g),
            cand_pli.virtual_id,
        )
        .await;
        last_response = Some(outcome.into_response());
    }

    last_response.unwrap_or_else(|| axum::http::StatusCode::BAD_REQUEST.into_response())
}

fn m3u_api_resource_auth(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    username: &str,
    password: &str,
) -> Result<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>), ApiUserAuthError> {
    let (user, target) =
        get_user_target_by_credentials(username, password, api_req, app_state).ok_or(ApiUserAuthError::AuthFailed)?;
    app_state.resolve_api_user_context(user.clone(), target.clone(), fingerprint.clone())?;
    Ok((user, target))
}

async fn m3u_api_resource(
    fingerprint: Fingerprint,
    req_headers: axum::http::HeaderMap,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path((username, password, stream_id, resource)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let Ok(m3u_stream_id) = stream_id.parse::<u32>() else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = match m3u_api_resource_auth(&fingerprint, &app_state, &api_req, &username, &password) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_player_response(auth_status),
    };

    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let m3u_item =
        match m3u_get_item_for_stream_id(m3u_stream_id, &app_state.app_config, &app_state.playlists, &target).await {
            Ok(item) => item,
            Err(err) => {
                error!("Failed to get m3u url: {}", sanitize_sensitive_info(&err.to_string()));
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        };

    if !user.allows_item_type(m3u_item.item_type)
        || !(user.t_filter.is_none() || user.allows_content(&shared::model::PlaylistItem::from(&m3u_item)))
    {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    if !shared::model::is_resource_field_name(&resource) {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let stream_url = m3u_item.get_field(resource.as_str());
    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(m3u_item.item_type) || target.is_force_redirect(m3u_item.item_type) {
                let input = app_state.app_config.get_input_by_name(&m3u_item.input_name);
                let redirect_url = crate::api::api_utils::resolve_redirect_location(input.as_deref(), &url);
                match redirect_url {
                    Ok(redirect_url) => {
                        resource_redirect_or_proxy(&app_state, redirect_url.as_ref(), &req_headers, input.as_deref())
                            .await
                    }
                    Err(err) => {
                        error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                        axum::http::StatusCode::BAD_REQUEST.into_response()
                    }
                }
            } else {
                // The response narrows the input to the resource origin itself; passing the input of
                // the item keeps that decision in one place.
                let input = app_state.app_config.get_input_by_name(&m3u_item.input_name);
                resource_proxy_response(&app_state, &url, &req_headers, input.as_deref()).await
            }
        }
    }
}

macro_rules! create_m3u_api_stream {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            fingerprint: Fingerprint,
            req_headers: axum::http::HeaderMap,
            axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(String, String, String)>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
        ) -> impl IntoResponse + Send {
            let stream_req = ApiStreamRequest::from($context, &username, &password, &stream_id, "");
            let auth_status = app_state.app_config.get_auth_error_status();
            match m3u_api_stream_network_auth(&fingerprint, &app_state, &api_req, &stream_req) {
                Ok((user, target)) => m3u_api_stream(
                    user,
                    target,
                    &fingerprint,
                    &req_headers,
                    &app_state,
                    stream_req,
                    raw_query.as_deref(),
                )
                .await
                .into_response(),
                Err(e) => e.into_player_response(auth_status),
            }
        }
    };
}

macro_rules! create_nested_m3u_api_stream {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            fingerprint: Fingerprint,
            req_headers: axum::http::HeaderMap,
            axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
            axum::extract::Path((username, password, stream_id, file)): axum::extract::Path<(
                String,
                String,
                String,
                String,
            )>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
        ) -> impl IntoResponse + Send {
            let stream_req = ApiStreamRequest::from($context, &username, &password, &stream_id, "");
            let auth_status = app_state.app_config.get_auth_error_status();
            match m3u_api_stream_network_auth(&fingerprint, &app_state, &api_req, &stream_req) {
                Ok((user, target)) => m3u_api_stream_nested(
                    user,
                    target,
                    &fingerprint,
                    &req_headers,
                    &app_state,
                    stream_req,
                    &file,
                    raw_query.as_deref(),
                )
                .await
                .into_response(),
                Err(err) => err.into_player_response(auth_status),
            }
        }
    };
}

create_m3u_api_stream!(m3u_api_live_stream_alt, ApiStreamContext::LiveAlt);
create_m3u_api_stream!(m3u_api_live_stream, ApiStreamContext::Live);
create_m3u_api_stream!(m3u_api_series_stream, ApiStreamContext::Series);
create_m3u_api_stream!(m3u_api_movie_stream, ApiStreamContext::Movie);
create_nested_m3u_api_stream!(m3u_api_live_stream_alt_nested, ApiStreamContext::LiveAlt);
create_nested_m3u_api_stream!(m3u_api_live_stream_nested, ApiStreamContext::Live);

macro_rules! register_m3u_api_stream {
     ($router:expr, [$(($path:expr, $fn_name:ident)),*]) => {{
         $router
       $(
        .route(&format!("/{}/{{username}}/{{password}}/{{stream_id}}", $path), axum::routing::get($fn_name))
            // $cfg.service(web::resource(format!("/{M3U_STREAM_PATH}/{}/{{username}}/{{password}}/{{stream_id}}", $path)).route(web::get().to(m3u_api_stream)));
        )*
    }};
}

macro_rules! register_m3u_api_routes {
    ($router:expr, [$($path:expr),*]) => {{
        $router
        $(
            .route(&format!("/{}", $path), axum::routing::get(m3u_api_get).post(m3u_api_post))
            // $cfg.service(web::resource(format!("/{}", $path)).route(web::get().to(m3u_api_get)).route(web::post().to(m3u_api_post)));
        )*
    }};
}

pub fn m3u_api_register() -> axum::Router<Arc<AppState>> {
    let mut router = axum::Router::new();
    router = register_m3u_api_routes!(router, ["get.php", "apiget", "m3u"]);
    router = router.route("/m3u-catchup/{token}", axum::routing::get(m3u_api_catchup));
    router = register_m3u_api_stream!(
        router,
        [
            (storage_const::M3U_STREAM_PATH, m3u_api_live_stream_alt),
            (concat_path(storage_const::M3U_STREAM_PATH, "live"), m3u_api_live_stream),
            (concat_path(storage_const::M3U_STREAM_PATH, "movie"), m3u_api_movie_stream),
            (concat_path(storage_const::M3U_STREAM_PATH, "series"), m3u_api_series_stream)
        ]
    );
    router = router
        .route(
            &format!("/{}/{{username}}/{{password}}/{{stream_id}}/{{file}}", storage_const::M3U_STREAM_PATH),
            axum::routing::get(m3u_api_live_stream_alt_nested),
        )
        .route(
            &format!(
                "/{}/{{username}}/{{password}}/{{stream_id}}/{{file}}",
                concat_path(storage_const::M3U_STREAM_PATH, "live")
            ),
            axum::routing::get(m3u_api_live_stream_nested),
        );

    router.route(
        &format!("/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}", storage_const::M3U_RESOURCE_PATH),
        axum::routing::get(m3u_api_resource),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        apply_native_flussonic_archive, effective_playback_extension, native_archive_matches_mode,
        native_live_file_matches_mode, playlist_item_allows_flussonic_archive, playlist_item_native_flussonic_mode,
        resolve_m3u_catchup_properties, resolved_m3u_item_is_allowed,
    };
    use crate::{
        api::model::UserApiRequest,
        iptv::m3u::{parse_flussonic_archive_file, FlussonicArchiveKind},
        model::{ConfigInput, ProxyUserCredentials},
    };
    use shared::{
        model::{
            CatchupProperties, ClusterFlags, LiveStreamProperties, M3uPlaylistItem, PlaylistItemType, StreamProperties,
            VirtualId,
        },
        utils::Internable,
    };
    use std::{borrow::Cow, sync::Arc};

    #[test]
    fn post_query_only_request_prefers_query_when_form_is_missing() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            content_type: String::from("m3u_plus"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.password, "query-pass");
        assert_eq!(api_req.content_type, "m3u_plus");
    }

    #[test]
    fn post_request_prefers_query_over_form() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            content_type: String::from("query-type"),
            ..UserApiRequest::default()
        };
        let form_req = UserApiRequest {
            username: String::from("form-user"),
            content_type: String::from("form-type"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.content_type, "query-type");
    }

    #[test]
    fn resolved_catchup_item_requires_rechecked_permissions() {
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::Live;

        assert!(!resolved_m3u_item_is_allowed(&user, PlaylistItemType::Catchup));
    }

    #[test]
    fn catchup_uses_resolved_upstream_transport() {
        assert_eq!(
            effective_playback_extension(
                PlaylistItemType::Catchup,
                "http://provider/ch/timeshift_abs-1.ts",
                Some(".m3u8")
            ),
            Some(".ts")
        );
        assert_eq!(
            effective_playback_extension(
                PlaylistItemType::Catchup,
                "http://provider/ch/timeshift_abs-1.m3u8",
                Some(".ts")
            ),
            Some(".m3u8")
        );
        assert_eq!(
            effective_playback_extension(PlaylistItemType::Live, "http://provider/ch/index.m3u8", Some(".ts")),
            Some(".ts")
        );
    }

    #[test]
    fn timeshift_only_live_items_derive_flussonic_catchup() -> Result<(), &'static str> {
        let time_shift = Arc::<str>::from("3");
        for item_type in [PlaylistItemType::Live, PlaylistItemType::LiveHls] {
            let catchup = resolve_m3u_catchup_properties(item_type, &time_shift, None)
                .ok_or("live timeshift item did not derive catchup")?;
            assert_eq!(catchup.mode.as_deref(), Some("fs"));
            assert_eq!(catchup.days.as_deref(), Some("3"));
        }
        Ok(())
    }

    #[test]
    fn timeshift_only_non_live_item_does_not_derive_catchup() {
        let time_shift = Arc::<str>::from("3");
        assert!(resolve_m3u_catchup_properties(PlaylistItemType::Video, &time_shift, None).is_none());
    }

    #[test]
    fn explicit_live_catchup_takes_precedence_over_timeshift_fallback() -> Result<(), &'static str> {
        let explicit = CatchupProperties { mode: Some(Arc::<str>::from("shift")), ..CatchupProperties::default() };
        let properties = StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(explicit),
            ..LiveStreamProperties::default()
        }));
        let time_shift = Arc::<str>::from("3");

        let catchup = resolve_m3u_catchup_properties(PlaylistItemType::LiveHls, &time_shift, Some(&properties))
            .ok_or("explicit catchup was not resolved")?;

        assert!(matches!(catchup, Cow::Borrowed(_)));
        assert_eq!(catchup.mode.as_deref(), Some("shift"));
        Ok(())
    }

    fn native_flussonic_item(mode: &str, url: &str) -> M3uPlaylistItem {
        M3uPlaylistItem {
            virtual_id: VirtualId::new(59),
            provider_id: "59".intern(),
            name: "Channel".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Live".intern(),
            title: "Channel".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "3".intern(),
            rec: "".intern(),
            url: url.intern(),
            epg_channel_id: None,
            input_name: "input".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                catchup: Some(CatchupProperties { catchup_type: Some(mode.intern()), ..CatchupProperties::default() }),
                ..LiveStreamProperties::default()
            }))),
            input_stream_id: "59".intern(),
            upstream_user_agent: None,
            input_url: "".intern(),
        }
    }

    #[test]
    fn native_archive_resolution_handles_hls_and_ts() -> Result<(), shared::error::TuliproxError> {
        let hls_item = native_flussonic_item("flussonic", "http://provider/ch/index.m3u8?token=abc");
        assert_eq!(playlist_item_native_flussonic_mode(&hls_item), Some("flussonic"));
        let (hls_archive, discriminator) = apply_native_flussonic_archive(
            &hls_item,
            &ConfigInput::default(),
            &FlussonicArchiveKind::Archive {
                start: "1784898000".to_string(),
                duration: "3600".to_string(),
                extension: ".m3u8",
            },
        )?;
        assert_eq!(hls_archive.item_type, PlaylistItemType::Catchup);
        assert_eq!(hls_archive.url.as_ref(), "http://provider/ch/index-1784898000-3600.m3u8?token=abc");
        assert_eq!(discriminator, "archive|1784898000|3600|.m3u8");

        let ts_item = native_flussonic_item("flussonic-ts", "http://provider/ch/channel.ts?token=abc");
        let (ts_archive, _) = apply_native_flussonic_archive(
            &ts_item,
            &ConfigInput::default(),
            &FlussonicArchiveKind::TimeshiftAbs { start: "1784898000".to_string(), extension: ".ts" },
        )?;
        assert_eq!(ts_archive.url.as_ref(), "http://provider/ch/timeshift_abs-1784898000.ts?token=abc");
        Ok(())
    }

    #[test]
    fn native_archive_mode_is_not_inferred_from_timeshift_alone() {
        let mut item = native_flussonic_item("append", "http://provider/ch/index.m3u8");
        item.time_shift = "7".intern();
        assert_eq!(playlist_item_native_flussonic_mode(&item), None);
    }

    #[test]
    fn native_archive_fallback_is_not_inferred_from_append_days() {
        let mut item = native_flussonic_item("append", "http://provider/ch/index.m3u8");
        item.time_shift = "".intern();
        if let Some(StreamProperties::Live(live)) = item.additional_properties.as_mut() {
            if let Some(catchup) = live.catchup.as_mut() {
                catchup.days = Some("7".intern());
            }
        }
        assert!(!playlist_item_allows_flussonic_archive(&item));
    }

    #[test]
    fn native_paths_must_match_declared_transport() {
        assert!(native_live_file_matches_mode("flussonic", "index.m3u8"));
        assert!(!native_live_file_matches_mode("flussonic", "mpegts"));
        assert!(native_live_file_matches_mode("flussonic-ts", "mpegts"));
        assert!(!native_live_file_matches_mode("flussonic-ts", "index.m3u8"));

        assert!(native_archive_matches_mode(
            "flussonic",
            &FlussonicArchiveKind::Archive {
                start: "1784898000".to_string(),
                duration: "3600".to_string(),
                extension: ".m3u8",
            }
        ));
        assert!(!native_archive_matches_mode(
            "flussonic-ts",
            &FlussonicArchiveKind::Archive {
                start: "1784898000".to_string(),
                duration: "3600".to_string(),
                extension: ".m3u8",
            }
        ));
        assert!(native_archive_matches_mode(
            "flussonic-ts",
            &FlussonicArchiveKind::TimeshiftAbs { start: "1784898000".to_string(), extension: ".ts" }
        ));
    }

    #[test]
    fn nested_archive_file_is_parsed_and_checked_against_transport() {
        let archive = parse_flussonic_archive_file("timeshift_abs-1784898000.ts");
        assert!(archive.as_ref().is_some_and(|value| native_archive_matches_mode("flussonic-ts", value)));
        assert!(!archive.as_ref().is_some_and(|value| native_archive_matches_mode("flussonic", value)));

        for file in ["archive-1784898000-3600.ts", "timeshift_rel-120.ts"] {
            let archive = parse_flussonic_archive_file(file);
            assert!(archive.as_ref().is_some_and(|value| native_archive_matches_mode("flussonic-ts", value)));
            assert!(!archive.as_ref().is_some_and(|value| native_archive_matches_mode("flussonic", value)));
        }
    }

    /// A native Flussonic archive keeps the `provider://cdn` URL and its encoded query; without a playback profile
    /// it uses the concrete host.
    #[test]
    fn token_only_m3u_archive_keeps_explicit_playback_profile_and_encoded_query() {
        let profile = std::sync::Arc::new(crate::model::ConfigProvider::from(&shared::model::ConfigProviderDto {
            name: "cdn".into(),
            urls: vec!["http://pl.example.com".into(), "http://eu.example.com".into()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        let mut input = ConfigInput {
            url: "http://playlist.example.com/list.m3u?access_key=fixture".into(),
            playback_provider: Some("cdn".into()),
            provider_configs: Some(vec![profile]),
            ..ConfigInput::default()
        };
        let item = native_flussonic_item("flussonic", "provider://cdn/ChannelA/mono.m3u8?token=fixture%2Btoken&keep=1");
        let kind =
            FlussonicArchiveKind::Archive { start: "1784898000".into(), duration: "60".into(), extension: ".m3u8" };
        let (archive, _) = apply_native_flussonic_archive(&item, &input, &kind).unwrap();
        assert_eq!(
            archive.url.as_ref(),
            "provider://cdn/ChannelA/mono-1784898000-60.m3u8?token=fixture%2Btoken&keep=1"
        );
        input.playback_provider = None;
        let (legacy_archive, _) = apply_native_flussonic_archive(&item, &input, &kind).unwrap();
        assert_eq!(
            legacy_archive.url.as_ref(),
            "http://pl.example.com/ChannelA/mono-1784898000-60.m3u8?token=fixture%2Btoken&keep=1"
        );
    }

    #[tokio::test]
    async fn resolve_m3u_stream_item_and_target_for_group_member_and_plain_item() {
        use super::{live_group_playback_source, resolve_m3u_stream_item_and_target};
        use crate::api::api_utils::{tests::group_playback_support::GroupPlaybackEnv, PlaybackSource};
        use tuliprox_repository::target_stream_item::ResolvedStreamTarget;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;

        // 1. Group VID resolves to member 0 with prefer: None
        let (group_item, group_target) = resolve_m3u_stream_item_and_target(
            &env.app_state.app_config,
            &env.app_state.playlists,
            &env.target,
            env.group_vid,
        )
        .await
        .expect("group VID resolves");
        assert_eq!(group_item.input_name.as_ref(), "solis");
        assert!(matches!(&group_target, ResolvedStreamTarget::Group(_)));
        let group_source = live_group_playback_source(&group_target);
        assert!(matches!(
            group_source,
            Some(PlaybackSource::Group {
                prefer: None,
                mode: tuliprox_session::failover_group::SelectionMode::Live,
                ..
            })
        ));

        // 2. Member VID resolves to that member with prefer: Some(index)
        let member_0_vid = env.group_record.members[0].member_vid.get();
        let (m0_item, m0_target) = resolve_m3u_stream_item_and_target(
            &env.app_state.app_config,
            &env.app_state.playlists,
            &env.target,
            member_0_vid,
        )
        .await
        .expect("member 0 VID resolves");
        assert_eq!(m0_item.input_name.as_ref(), "solis");
        assert!(matches!(&m0_target, ResolvedStreamTarget::Member { index: 0, .. }));
        let m0_source = live_group_playback_source(&m0_target);
        assert!(matches!(
            m0_source,
            Some(PlaybackSource::Group {
                prefer: Some(0),
                mode: tuliprox_session::failover_group::SelectionMode::Live,
                ..
            })
        ));

        let member_1_vid = env.group_record.members[1].member_vid.get();
        let (m1_item, m1_target) = resolve_m3u_stream_item_and_target(
            &env.app_state.app_config,
            &env.app_state.playlists,
            &env.target,
            member_1_vid,
        )
        .await
        .expect("member 1 VID resolves");
        assert_eq!(m1_item.input_name.as_ref(), "korath");
        assert!(matches!(&m1_target, ResolvedStreamTarget::Member { index: 1, .. }));
        let m1_source = live_group_playback_source(&m1_target);
        assert!(matches!(
            m1_source,
            Some(PlaybackSource::Group {
                prefer: Some(1),
                mode: tuliprox_session::failover_group::SelectionMode::Live,
                ..
            })
        ));

        // 3. Plain item resolves to Item with no group source
        let (plain_item, plain_target) = resolve_m3u_stream_item_and_target(
            &env.app_state.app_config,
            &env.app_state.playlists,
            &env.target,
            env.plain_vid,
        )
        .await
        .expect("plain VID resolves");
        assert_eq!(plain_item.virtual_id.get(), env.plain_vid);
        assert!(matches!(&plain_target, ResolvedStreamTarget::Item));
        assert!(live_group_playback_source(&plain_target).is_none());

        // 4. Unknown VID returns error (keeps 404 in handler)
        assert!(resolve_m3u_stream_item_and_target(
            &env.app_state.app_config,
            &env.app_state.playlists,
            &env.target,
            999_999,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn nested_m3u_stream_resolves_member_and_group_vids() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use axum::http::StatusCode;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let member_vid = env.group_record.members[1].member_vid.get();
        let group_vid = env.group_vid;

        // The item lookup used to fail with 404 for group and member VIDs. Now the item resolves and the
        // request reaches the native file check, which rejects `live.m3u8` for a non-Flussonic item.
        for vid in [member_vid, group_vid] {
            let res = env.get(&format!("/m3u-stream/viewer/secret/{vid}/live.m3u8")).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "vid {vid} must resolve before file validation");
        }

        let res = env.get("/m3u-stream/viewer/secret/999999/live.m3u8").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn archive_member_item_returns_pli_unchanged_for_non_group_target() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use tuliprox_repository::target_stream_item::ResolvedStreamTarget;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
        let pli = native_flussonic_item("flussonic", "http://provider/ch/index.m3u8");
        let start_ts = chrono::Utc::now().timestamp() - 3600;

        let result =
            super::archive_member_item(&env.app_state, &env.target, &ResolvedStreamTarget::Item, pli.clone(), start_ts);
        assert_eq!(result.url, pli.url);
        assert_eq!(result.input_name, pli.input_name);
    }

    #[tokio::test]
    async fn archive_member_item_selects_archive_capable_member() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use tuliprox_repository::target_stream_item::ResolvedStreamTarget;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"])
            .hls()
            .without_plain_channel()
            .with_member_archives(&[None, Some(7)])
            .with_member_catchup_modes(&[None, Some("flussonic")])
            .build()
            .await;

        let member_0_item = tuliprox_repository::target_stream_item::member_to_m3u_item(&env.group_record, 0);
        let member_1_item = tuliprox_repository::target_stream_item::member_to_m3u_item(&env.group_record, 1);
        let start_ts = chrono::Utc::now().timestamp() - 3600;

        let result = super::archive_member_item(
            &env.app_state,
            &env.target,
            &ResolvedStreamTarget::Group(env.group_record.clone()),
            member_0_item,
            start_ts,
        );
        assert_eq!(result.virtual_id, member_1_item.virtual_id);
        assert_eq!(result.input_name, member_1_item.input_name);
    }

    #[tokio::test]
    async fn nested_m3u_stream_selects_archive_capable_member_for_group_vid() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use axum::http::StatusCode;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"])
            .hls()
            .without_plain_channel()
            .with_member_archives(&[None, Some(7)])
            .with_member_catchup_modes(&[None, Some("flussonic")])
            .build()
            .await;

        let start_ts = chrono::Utc::now().timestamp() - 3600;
        let res = env.get(&format!("/m3u-stream/viewer/secret/{}/archive-{start_ts}-3600.m3u8", env.group_vid)).await;
        assert_ne!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "nested archive request for group VID must select archive-capable member instead of failing with 400"
        );
    }
}
