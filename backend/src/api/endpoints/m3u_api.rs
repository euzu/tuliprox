use crate::{
    api::{
        api_utils::{
            admission_failure_response, coalesce_byte_stream, create_m3u_catchup_session_key,
            create_playback_session_fingerprint, create_session_fingerprint, force_provider_stream_response,
            get_session_reservation_ttl_secs, get_user_target, get_user_target_by_credentials,
            is_seekable_media_request, is_session_based_playback, is_stream_share_enabled, local_stream_response,
            redirect, redirect_response, resolve_initial_stalker_playback_url, resource_response,
            separate_number_and_remainder, should_allow_exhausted_shared_reconnect, stream_response,
            try_option_bad_request, try_result_bad_request, try_result_not_found, try_unwrap_body, RedirectParams,
        },
        endpoints::{
            hls_api::{
                build_virtual_hls_entry_path, handle_hls_stream_request, hls_admission_failure_manifest_response,
                hls_custom_video_manifest_response, m3u_archive_epg_reference_ts, HlsEntryStreamContext,
            },
            xtream_api::{ApiStreamContext, ApiStreamRequest},
        },
        model::{AppState, UserApiRequest, UserApiRequestQueryOrBody},
    },
    auth::{check_network_access_only, resolve_api_user_context, ApiUserAuthError, Fingerprint},
    iptv::m3u::{
        build_provider_flussonic_archive_url, decode_m3u_catchup_token, has_m3u_catchup_marker, is_flussonic_live_file,
        parse_flat_flussonic_archive, parse_flussonic_archive_file, resolve_m3u_catchup_url, FlussonicArchiveKind,
        M3uCatchupToken,
    },
    model::{ConfigTarget, ProxyUserCredentials},
    repository::{m3u_get_item_for_stream_id, m3u_load_rewrite_playlist, storage_const},
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
        TargetType, UserConnectionPermission, XtreamCluster,
    },
    utils::{concat_path, extract_extension_from_url, sanitize_sensitive_info},
};
use std::{borrow::Cow, sync::Arc};

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

            let mut builder = axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::TEXT_PLAIN_UTF_8.to_string());
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
    check_network_access_only(&user, fingerprint, app_state)?;
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
    check_network_access_only(&user, fingerprint, app_state)?;
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
) -> impl IntoResponse + Send {
    let target_name = &target.name;
    if !target.has_output(TargetType::M3u) {
        debug!("Target has no m3u playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let is_hls_manifest_request = effective_playback_extension(pli.item_type, &pli.url, stream_ext) == Some(HLS_EXT);

    if !user.allows_item_type(pli.item_type) {
        if is_hls_manifest_request {
            return hls_custom_video_manifest_response(
                app_state,
                &user,
                crate::api::model::CustomVideoStreamType::ChannelUnavailable,
                axum::http::StatusCode::FORBIDDEN,
            )
            .await;
        }
        return crate::api::model::create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            crate::api::model::CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();
    }
    let virtual_id = pli.virtual_id;

    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    if user.permission_denied(app_state) {
        if is_hls_manifest_request {
            return hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                pli.input_name.clone(),
                req_headers,
                ConnectFailureReason::UserAccountExpired,
            )
            .await;
        }
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            pli.to_stream_channel(target.id),
            pli.input_name.clone(),
            req_headers,
            ConnectFailureReason::UserAccountExpired,
        );
    }

    if pli.item_type.is_local() {
        let playback_session_token = create_session_fingerprint(fingerprint, &user.username, virtual_id, false);
        let user_session =
            app_state.active_users.get_and_update_user_session(&user.username, &playback_session_token).await;
        let (admission, _grace_mode, request_class) = crate::api::api_utils::resolve_playback_request_admission(
            app_state,
            &user,
            fingerprint,
            pli.item_type,
            user_session.as_ref(),
            playback_session_token.as_str(),
            false,
            crate::api::api_utils::EvictionReentryGuard::Session(playback_session_token.as_str()),
            false,
            false,
        )
        .await;
        return local_stream_response(
            fingerprint,
            app_state,
            pli.to_stream_channel(target.id),
            req_headers,
            &input,
            &target,
            &user,
            admission.permission,
            admission.kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            Some(playback_session_token.as_str()),
            Some(request_class),
            true,
        )
        .await
        .into_response();
    }

    let cluster = XtreamCluster::try_from(pli.item_type).unwrap_or(XtreamCluster::Live);
    pli.url = match resolve_initial_stalker_playback_url(
        app_state,
        &input,
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
            return axum::http::StatusCode::BAD_GATEWAY.into_response();
        }
    };

    debug_if_enabled!("M3U playback for virtual_id={virtual_id}, item_type={}", pli.item_type);
    let effective_stream_ext = effective_playback_extension(pli.item_type, &pli.url, stream_ext);
    let extension = effective_stream_ext.unwrap_or_default();
    let session_key = if pli.item_type == PlaylistItemType::Catchup {
        create_m3u_catchup_session_key(fingerprint, &user.username, virtual_id, archive_discriminator.unwrap_or("live"))
    } else {
        create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, pli.item_type, Some(extension))
    };
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
                return hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    pli.to_stream_channel(target.id),
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::UserConnectionsExhausted,
                )
                .await;
            }
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            if extension == HLS_EXT {
                return hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    pli.to_stream_channel(target.id),
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::ProviderConnectionsExhausted,
                )
                .await;
            }
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }
        if session.virtual_id == virtual_id && is_seekable_media_request(cluster, req_headers, Some(extension)) {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                pli.to_stream_channel(target.id),
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
            .into_response();
        }
        if pli.item_type == PlaylistItemType::Catchup {
            pli.url.clone()
        } else {
            session.stream_url.clone()
        }
    } else {
        pli.url.clone()
    };

    let (connection_admission, grace_mode, request_class) = crate::api::api_utils::resolve_playback_request_admission(
        app_state,
        &user,
        fingerprint,
        pli.item_type,
        user_session.as_ref(),
        &session_key,
        false,
        eviction_reentry_guard,
        false,
        false,
    )
    .await;
    let connection_permission = connection_admission.permission;
    let connection_kind = connection_admission
        .kind
        .or(user_session.as_ref().and_then(|session| session.connection_kind))
        .unwrap_or(crate::api::model::ConnectionKind::Normal);
    let allow_exhausted_shared_reconnect = should_allow_exhausted_shared_reconnect(
        is_stream_share_enabled(pli.item_type, &target),
        user_session.as_ref(),
        virtual_id,
        session_url.as_ref(),
    );
    if connection_permission == UserConnectionPermission::Exhausted && !allow_exhausted_shared_reconnect {
        if extension == HLS_EXT {
            return hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                &user,
                pli.to_stream_channel(target.id),
                input.name.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            pli.to_stream_channel(target.id),
            input.name.clone(),
            req_headers,
            ConnectFailureReason::UserConnectionsExhausted,
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
            return response.into_response();
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
        let Some(stream_context) = HlsEntryStreamContext::from_playlist_item(&pli) else {
            error!(
                "HLS input stream identity missing for virtual_id={}; refresh target playlist",
                pli.virtual_id
            );
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, pli.virtual_id);
        return handle_hls_stream_request(
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
        )
        .await
        .into_response();
    }

    let pinned_provider =
        user_session.as_ref().filter(|_| pli.item_type.requires_provider_affinity()).map(|session| &session.provider);

    stream_response(
        fingerprint,
        app_state,
        &session_key,
        Some(request_class),
        pli.to_stream_channel(target.id).with_epg_reference_ts(archive_reference),
        &session_url,
        pinned_provider,
        req_headers,
        &input,
        &target,
        &user,
        connection_permission,
        connection_kind,
        allow_exhausted_shared_reconnect,
        grace_mode,
    )
    .await
    .into_response()
}

fn resolve_effective_source_url<'a>(
    item: &'a shared::model::M3uPlaylistItem,
    input: &'a crate::model::ConfigInput,
) -> Result<Cow<'a, str>, TuliproxError> {
    if !item.url.starts_with(PROVIDER_SCHEME_PREFIX) {
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
        let pli = match m3u_get_item_for_stream_id(req_virtual_id, app_state, &target).await {
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
        )
        .await
        .into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let pli = match m3u_get_item_for_stream_id(req_virtual_id, app_state, &target).await {
        Ok(pli) => pli,
        Err(err) => {
            error!("Failed to read m3u item for stream id {req_virtual_id}: {err}");
            if stream_ext == Some(HLS_EXT) {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
            return crate::api::model::create_custom_video_stream_response(
                app_state,
                &fingerprint.addr,
                crate::api::model::CustomVideoStreamType::ChannelUnavailable,
            )
            .into_response();
        }
    };

    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {}", pli.input_name, pli.virtual_id)
    );
    let (resolved_pli, archive_discriminator) = if has_m3u_catchup_marker(raw_query) {
        match resolve_loaded_m3u_catchup(&pli, &input, raw_query) {
            Ok((item, discriminator)) => (item, Some(discriminator)),
            Err(err) => {
                debug!("Failed to resolve M3U catchup request: {}", sanitize_sensitive_info(&err.to_string()));
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        }
    } else {
        (pli, None)
    };
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
    let pli = match m3u_get_item_for_stream_id(req_virtual_id, app_state, &target).await {
        Ok(pli) => pli,
        Err(err) => {
            error!("Failed to read M3U item for nested stream id {req_virtual_id}: {err}");
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    };
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}, stream_id {}", pli.input_name, target.name, pli.virtual_id)
    );
    let stream_ext = extract_extension_from_url(file);
    let resolved = if let Some(archive) = parse_flussonic_archive_file(file) {
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
        if has_m3u_catchup_marker(raw_query) {
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
    )
    .await
    .into_response()
}

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
    if let Err(err) = check_network_access_only(&user, &fingerprint, &app_state) {
        return err.into_player_response(app_state.app_config.get_auth_error_status());
    }
    if user.permission_denied(&app_state) || !target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let _user_guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    let pli = try_result_not_found!(
        m3u_get_item_for_stream_id(virtual_id, &app_state, &target).await,
        true,
        format!("Failed to read m3u item for stream id {virtual_id}")
    );
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}, stream_id {virtual_id}", pli.input_name, target.name)
    );
    let (resolved_pli, archive_discriminator) = match resolve_loaded_m3u_catchup(&pli, &input, raw_query.as_deref()) {
        Ok(result) => result,
        Err(err) => {
            debug!("Failed to resolve M3U catchup token request: {}", sanitize_sensitive_info(&err.to_string()));
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };
    if !resolved_m3u_item_is_allowed(user.as_ref(), resolved_pli.item_type) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    m3u_api_stream_loaded(
        user,
        target,
        &fingerprint,
        &req_headers,
        &app_state,
        resolved_pli,
        input,
        None,
        Some(&archive_discriminator),
    )
    .await
    .into_response()
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
    resolve_api_user_context(user.clone(), target.clone(), fingerprint.clone(), app_state)?;
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
    let m3u_item = match m3u_get_item_for_stream_id(m3u_stream_id, &app_state, &target).await {
        Ok(item) => item,
        Err(err) => {
            error!("Failed to get m3u url: {}", sanitize_sensitive_info(&err.to_string()));
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    };

    if !user.allows_item_type(m3u_item.item_type) {
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
                        debug!("Redirecting stream request to {}", sanitize_sensitive_info(redirect_url.as_ref()));
                        redirect(redirect_url.as_ref()).into_response()
                    }
                    Err(err) => {
                        error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                        axum::http::StatusCode::BAD_REQUEST.into_response()
                    }
                }
            } else {
                resource_response(&app_state, &url, &req_headers, None).await.into_response()
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
            virtual_id: 59,
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
}
