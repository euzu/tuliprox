use super::{
    build_virtual_hls_entry_path, epg_reference_ts_from_date_tree_path, get_query_path, get_stream_channel,
    get_xtream_player_api_stream_url, handle_hls_stream_request, hls_admission_failure_manifest_response,
    hls_api_stream_leaked_relative, hls_cache_enabled_for_user, hls_custom_video_manifest_response_for_username,
    hls_manifest_channel_unavailable_response_for_username, is_live_hls_session_token,
    legacy_hls_route_allowed_with_cache, looks_like_archive_media_path, m3u_archive_epg_reference_ts,
    recreate_hls_session_from_token, resolve_m3u_archive_reference, ApiStreamContext, HlsApiPathParams,
    HlsEntryStreamContext, HlsRequestStage, HlsResolvedVirtualSource,
};
use crate::{
    api::{
        api_utils::{
            create_api_proxy_user, create_m3u_catchup_session_key, create_playback_session_fingerprint,
            create_recording_proxy_user, create_session_fingerprint, force_hls_resource_response,
            get_hls_session_ttl_secs, input_for_user, local_stream_response, try_option_bad_request,
        },
        model::{AppState, CustomVideoStreamType},
    },
    auth::{check_network_access_only, Fingerprint},
    model::{ConfigInput, ConfigTarget, PlaybackKind, ProxyUserCredentials},
    processing::parser::hls::{get_hls_session_token_and_url_from_token, HlsResourceKind},
    repository::{m3u_get_item_for_stream_id, xtream_get_item_for_stream_id},
    utils::{debug_if_enabled, request::is_file_url},
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::{debug, error, warn};
use shared::{
    defaults::HLS_EXT,
    model::{
        ConnectFailureReason, PlaylistEntry, PlaylistItemType, StreamChannel, TargetType, UserConnectionPermission,
        XtreamCluster,
    },
    utils::{generate_random_string, is_m3u_catchup_session_token, sanitize_sensitive_info, Internable},
};
use std::sync::Arc;

pub(in crate::api::endpoints::hls_api) fn hls_stream_context_or_unavailable(
    item: &impl PlaylistEntry,
    virtual_id: u32,
) -> Result<HlsEntryStreamContext, StatusCode> {
    HlsEntryStreamContext::from_playlist_item(item).ok_or_else(|| {
        warn!("HLS input stream identity missing for virtual_id={virtual_id}; refresh target playlist");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

pub(in crate::api) async fn resolve_hls_virtual_source_for_target(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Result<HlsResolvedVirtualSource, StatusCode> {
    let (input_name, stream_context) = if target.has_output(TargetType::Xtream) {
        if let Ok(item) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        } else {
            let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
                .await
                .map_err(|_| StatusCode::NOT_FOUND)?;
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        }
    } else {
        let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
        (Arc::clone(&item.input_name), stream_context)
    };
    let input = app_state.app_config.get_input_by_name(&input_name).ok_or(StatusCode::NOT_FOUND)?;
    Ok(HlsResolvedVirtualSource { input, stream_context })
}

pub(in crate::api::endpoints::hls_api) async fn resolve_hls_origin_playlist_url(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    fallback_url: &str,
) -> Result<String, StatusCode> {
    if input.input_type.is_xtream() && target.has_output(TargetType::Xtream) {
        let pli = xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let hls_extension = format!(".{HLS_EXT}");
        let (query_path, _) = get_query_path("", Some(&hls_extension), &pli, app_state);
        return get_xtream_player_api_stream_url(input, ApiStreamContext::Live, &query_path, &pli.url)
            .map(|url| url.to_string())
            .ok_or(StatusCode::SERVICE_UNAVAILABLE);
    }

    Ok(fallback_url.to_string())
}

/// Archive (Catchup) or live HLS playback, decided the same way for every HLS path so leases,
/// reservations and TTLs agree. Append/shift catchup often loses utc/utcstart on rewritten
/// segment URLs; the archive path or the session token still identifies archive playback.
pub(in crate::api::endpoints::hls_api) fn hls_playback_kind(
    archive_reference: Option<i64>,
    url: &str,
    session_token: Option<&str>,
) -> PlaybackKind {
    if archive_reference.is_some()
        || looks_like_archive_media_path(url)
        || session_token.is_some_and(is_m3u_catchup_session_token)
    {
        PlaybackKind::Catchup
    } else {
        PlaybackKind::LiveHls
    }
}

pub(in crate::api::endpoints::hls_api) async fn resolve_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    hls_url: &str,
    archive_reference: Option<i64>,
    session_token: Option<&str>,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    let mut channel = match get_stream_channel(app_state, target, virtual_id).await {
        Some(mut channel) => {
            channel.url = Arc::from(hls_url);
            channel
        }
        None => StreamChannel {
            target_id: target.id,
            virtual_id,
            provider_id: 0,
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: unknown.clone(),
            title: unknown,
            url: Arc::from(hls_url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        },
    };

    let archive_reference = archive_reference.or_else(|| epg_reference_ts_from_date_tree_path(hls_url));
    if hls_playback_kind(archive_reference, hls_url, session_token) == PlaybackKind::Catchup {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.epg_reference_ts = None;
    }
    channel
}

pub(in crate::api::endpoints::hls_api) fn hls_entry_user_session_token(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    session_token_hint: Option<&str>,
    archive_reference: Option<i64>,
) -> String {
    // Live hints only arrive from sealed tokens validated by `hls_session_hint_matches_requester`;
    // entry routes pass the plain fingerprint key, which never carries the `|hls|` suffix.
    if let Some(hint) =
        session_token_hint.filter(|token| is_m3u_catchup_session_token(token) || is_live_hls_session_token(token))
    {
        return hint.to_string();
    }
    if let Some(timestamp) = archive_reference {
        return create_m3u_catchup_session_key(fingerprint, username, virtual_id, &format!("archive|{timestamp}|0"));
    }
    let base = create_playback_session_fingerprint(fingerprint, username, virtual_id, PlaylistItemType::LiveHls, None);
    format!("{base}|hls|{}", generate_random_string(16))
}

#[allow(clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let internal_user = match params.username.as_str() {
        crate::model::RECORDING_PROXY_USERNAME => Some(create_recording_proxy_user(&app_state)),
        "api_user" => Some(create_api_proxy_user(&app_state)),
        _ => None,
    }
    .filter(|internal| crate::auth::constant_time_eq(params.password.as_bytes(), internal.password.as_bytes()));
    let (user, target) = if let Some(internal_user) = internal_user {
        let Some(target) = app_state.app_config.get_target_by_id(params.target_id) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        (Arc::new(internal_user), target)
    } else {
        let Some((user, target)) = app_state.app_config.get_target_for_user(&params.username, &params.password) else {
            // Credential failure is an auth error, not a malformed request
            return app_state.app_config.get_auth_error_status().into_response();
        };
        if target.id != params.target_id {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        (user, target)
    };

    // Nested path = relative origin segment that leaked past rewrite_hls (e.g. dvr-YYYY/...).
    if params.token.contains('/') {
        let Some((token, relative_path)) = params.token.split_once('/') else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let encrypt_secret = app_state.get_encrypt_secret();
        let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, token) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let lookup_session_token = decoded_hls_token
            .session_token
            .clone()
            .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, params.stream_id, false));
        let Some(input) = app_state.app_config.get_input_by_id(params.input_id) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let input = input_for_user(&app_state.app_config, &user, input);
        let Some(session) = app_state
            .active_users
            .find_latest_session_for_target_stream(
                &user.username,
                target.id,
                input.name.as_ref(),
                params.stream_id,
                lookup_session_token.as_str(),
            )
            .await
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !legacy_hls_route_allowed_with_cache(
            hls_cache_enabled_for_user(&app_state, &target, &user),
            decoded_hls_token.session_token.as_deref(),
            Some(session.token.as_str()),
        ) {
            return hls_custom_video_manifest_response_for_username(
                &app_state,
                &user.username,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
        return hls_api_stream_leaked_relative(
            fingerprint,
            req_headers,
            app_state,
            user,
            target,
            input,
            params.stream_id,
            session,
            decoded_hls_token.url,
            relative_path.to_string(),
            raw_query.as_deref(),
        )
        .await;
    }

    hls_api_stream_resolved(
        fingerprint,
        req_headers,
        app_state,
        user,
        target,
        params.input_id,
        params.stream_id,
        params.token,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn hls_api_stream_resolved(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input_id: u16,
    stream_id: u32,
    token: String,
) -> axum::response::Response {
    // Network access check only - permission check is done later with full stream info
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let target_name = &target.name;
    let virtual_id = stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", input_id)
    );
    let input = input_for_user(&app_state.app_config, &user, input);

    if user.permission_denied(&app_state.app_config) {
        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, "", None, None).await;
        return hls_admission_failure_manifest_response(
            &app_state,
            &fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            &req_headers,
            ConnectFailureReason::UserAccountExpired,
        )
        .await;
    }

    debug_if_enabled!("ID chain for hls endpoint: request_stream_id={stream_id} -> virtual_id={virtual_id}");
    let encrypt_secret = app_state.get_encrypt_secret();
    let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, &token) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    // Manifest vs. media is decided once from the token kind; legacy tokens use the URL heuristic.
    let is_manifest = decoded_hls_token.is_manifest();
    let lookup_session_token = decoded_hls_token
        .session_token
        .clone()
        .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, virtual_id, false));
    let mut user_session =
        app_state.active_users.get_and_update_user_session(&user.username, &lookup_session_token).await;
    if !legacy_hls_route_allowed_with_cache(
        hls_cache_enabled_for_user(&app_state, &target, &user),
        decoded_hls_token.session_token.as_deref(),
        user_session.as_ref().map(|session| session.token.as_str()),
    ) {
        return hls_manifest_channel_unavailable_response_for_username(&app_state, &user.username).await;
    }

    if let Some(session) = &mut user_session {
        let decoded_archive_reference =
            resolve_m3u_archive_reference(&decoded_hls_token.url, Some(lookup_session_token.as_str()));
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.url,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }

        if app_state.active_provider.is_over_limit(&session.provider) {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.url,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            )
            .await;
        }

        let hls_url = match decoded_hls_token.session_token.as_deref() {
            Some(session_token) if session.token.eq(session_token) => decoded_hls_token.url.as_str(),
            None => decoded_hls_token.url.as_str(),
            Some(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        // A media URI resolved by another provider account would be fetched with that account's
        // credentials; the player refetches its playlist from the session's provider instead.
        if decoded_hls_token.kind == Some(HlsResourceKind::Media)
            && !session.provider.is_empty()
            && decoded_hls_token.origin_provider.as_deref().is_some_and(|origin| origin != session.provider.as_ref())
        {
            debug_if_enabled!(
                "HLS media URI from provider {} rejected for session provider {}",
                sanitize_sensitive_info(decoded_hls_token.origin_provider.as_deref().unwrap_or_default()),
                sanitize_sensitive_info(&session.provider)
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        let hls_url = hls_url.intern();
        // Recover utc/utcstart from the prior playlist URL before overwriting with a segment URL
        // that usually drops append/shift query params.
        let archive_reference = resolve_m3u_archive_reference(&hls_url, Some(session.token.as_str()))
            .or_else(|| m3u_archive_epg_reference_ts(session.stream_url.as_ref()));
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state.connection_manager.touch_http_activity(&user.username, &session.token, &fingerprint.addr).await;
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let (connection_admission, grace_mode, request_class) =
            crate::api::api_utils::resolve_playback_request_admission(
                &app_state.admission_ctx(),
                &user,
                &fingerprint,
                Some(session),
                &session.token,
                true,
                crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
                // HLS playlist requests are explicit Prepare: they set up session metadata
                // but do not consume an admission slot. Segment and other media requests use Activate.
                is_manifest,
                false,
            )
            .await;
        let connection_permission = connection_admission.permission();
        let connection_kind = connection_admission.kind().or(session.connection_kind);
        session.permission = connection_permission;
        if let Some(connection_kind) = connection_kind {
            session.connection_kind = Some(connection_kind);
        }
        if connection_permission == UserConnectionPermission::Exhausted
            || (connection_permission == UserConnectionPermission::GracePeriod && connection_kind.is_none())
        {
            if connection_admission.is_reentry_suppressed() {
                return crate::api::api_utils::reentry_suppressed_response();
            }
            let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &session.stream_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                provider,
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }
        let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

        if is_manifest {
            let manifest_stage = match decoded_hls_token.kind {
                Some(HlsResourceKind::Manifest(source)) => {
                    HlsRequestStage::Variant { source, origin_provider: decoded_hls_token.origin_provider.as_deref() }
                }
                Some(HlsResourceKind::Media) | None => HlsRequestStage::LegacyVariant,
            };
            let source = match resolve_hls_virtual_source_for_target(&app_state, &target, virtual_id).await {
                Ok(source) if source.input.id == input.id => source,
                Ok(source) => {
                    warn!(
                        "HLS input context mismatch for virtual_id={virtual_id}: expected_input_id={}, resolved_input_id={}",
                        input.id, source.input.id
                    );
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(status) => return status.into_response(),
            };
            let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, virtual_id);
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                &target,
                Some(session),
                None,
                &session.stream_url,
                archive_reference,
                source.stream_context,
                &input,
                &req_headers,
                connection_permission,
                connection_kind,
                &original_hls_entry_path,
                manifest_stage,
            )
            .await
            .into_response();
        }

        if is_file_url(&session.stream_url) {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &hls_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                fallback_connection_kind,
                Some(&session.token),
                Some(request_class),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel = resolve_stream_channel(
            &app_state,
            &target,
            &input,
            virtual_id,
            &hls_url,
            archive_reference,
            Some(session.token.as_str()),
        )
        .await;
        force_hls_resource_response(
            &fingerprint,
            &app_state,
            session,
            stream_channel,
            crate::api::api_utils::ForceStreamRequestContext {
                req_headers: &req_headers,
                input: &input,
                user: &user,
                session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            grace_mode,
        )
        .await
        .into_response()
    } else {
        recreate_hls_session_from_token(
            &fingerprint,
            &req_headers,
            &app_state,
            &user,
            &target,
            &input,
            virtual_id,
            &decoded_hls_token,
        )
        .await
    }
}
