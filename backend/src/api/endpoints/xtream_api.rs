use crate::auth::{check_network_access_only, check_permission_and_network_access_only};
// https://github.com/tellytv/go.xtream-codes/blob/master/structs.go
// Xtream api -> https://9tzx6f0ozj.apidog.io/
use crate::{
    api::{
        api_utils,
        api_utils::{
            admission_failure_response, coalesce_byte_stream, create_api_proxy_user, create_catchup_session_key,
            create_m3u_catchup_session_key, create_playback_session_fingerprint, create_session_fingerprint,
            empty_json_response_as_array, empty_json_response_as_object, force_provider_stream_response,
            get_session_reservation_ttl_secs, get_user_target, get_user_target_by_credentials,
            internal_server_error, is_seekable_media_request, is_session_based_playback, is_stream_share_enabled,
            local_stream_response, redirect, redirect_response, resource_response, separate_number_and_remainder,
            should_allow_exhausted_shared_reconnect, stream_response, try_option_bad_request, try_result_bad_request,
            try_unwrap_body, RedirectParams, resolve_initial_stalker_playback_url,
        },
        endpoints::{
            hls_api::{
                build_virtual_hls_entry_path, handle_hls_stream_request, hls_admission_failure_manifest_response,
                hls_custom_video_manifest_response, HlsEntryStreamIdentity,
            },
            xmltv_api::{get_empty_epg_response, get_epg_path_for_target_by_type, serve_short_epg},
        },
        model::{
            create_custom_video_stream_response, AppState, CustomVideoStreamType, UserApiRequest,
            UserApiRequestQueryOrBody, XtreamAuthorizationResponse,
        },
    },
    auth::{verify_access_token, Fingerprint},
    iptv::{
        m3u::{is_xtream_m3u_catchup_supported, resolve_xtream_m3u_catchup_url, ResolvedM3uCatchup},
        xtream::{self, create_vod_info_from_item},
    },
    model::{
        xtream_mapping_option_from_target_options, Config, ConfigInput, ConfigInputFlags, ConfigTarget, InputSource,
        ProxyUserCredentials,
    },
    repository::{
        get_target_id_mapping, get_target_storage_path, storage_const, user_get_bouquet_filter,
        xtream_get_collection_path, xtream_get_item_for_stream_id, xtream_load_rewrite_playlist, VirtualIdRecord,
    },
    utils::{
        apply_timeshift, debug_if_enabled, file_exists_async, parse_timeshift, request, trace_if_enabled,
    },
};
use axum::{http::HeaderMap, response::IntoResponse};
use bytes::Bytes;
use futures::{
    stream::{self, StreamExt},
    Stream,
};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{
        create_stream_channel_with_type, ConnectFailureReason, PlaylistEntry, PlaylistItemType, ProxyType,
        ShortEpgResultDto, StreamProperties, TargetType, UserConnectionPermission, XtreamCluster, XtreamPlaylistItem,
    },
    utils::{
        deserialize_as_string, extract_extension_from_url, generate_provider_playlist_uuid, sanitize_sensitive_info,
        trim_slash, Internable,
    },
    defaults::{HLS_EXT, TS_EXT},
};
use std::{
    fmt::{Display, Formatter, Write},
    str::FromStr,
    sync::Arc,
};
// https://github.com/tellytv/go.xtream-codes/blob/master/structs.go
// Xtream api -> https://9tzx6f0ozj.apidog.io/

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
pub enum ApiStreamContext {
    LiveAlt,
    Live,
    Movie,
    Series,
    Timeshift,
}

impl ApiStreamContext {
    const LIVE: &'static str = "live";
    const MOVIE: &'static str = "movie";
    const SERIES: &'static str = "series";
    const TIMESHIFT: &'static str = "timeshift";
}

impl Display for ApiStreamContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Live | Self::LiveAlt => Self::LIVE,
                Self::Movie => Self::MOVIE,
                Self::Series => Self::SERIES,
                Self::Timeshift => Self::TIMESHIFT,
            }
        )
    }
}

impl TryFrom<XtreamCluster> for ApiStreamContext {
    type Error = String;
    fn try_from(cluster: XtreamCluster) -> Result<Self, Self::Error> {
        match cluster {
            XtreamCluster::Live => Ok(Self::Live),
            XtreamCluster::Video => Ok(Self::Movie),
            XtreamCluster::Series => Ok(Self::Series),
        }
    }
}

impl FromStr for ApiStreamContext {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            Self::LIVE => Ok(Self::Live),
            Self::MOVIE => Ok(Self::Movie),
            Self::SERIES => Ok(Self::Series),
            Self::TIMESHIFT => Ok(Self::Timeshift),
            _ => Err(TuliproxError::ApiXtream(format!("Unknown ApiStreamContext: {s}"))),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct ApiStreamRequest<'a> {
    pub context: ApiStreamContext,
    pub access_token: bool,
    pub username: &'a str,
    pub password: &'a str,
    pub stream_id: &'a str,
    pub action_path: &'a str,
}

impl<'a> ApiStreamRequest<'a> {
    pub const fn from(
        context: ApiStreamContext,
        username: &'a str,
        password: &'a str,
        stream_id: &'a str,
        action_path: &'a str,
    ) -> Self {
        Self { context, access_token: false, username, password, stream_id, action_path }
    }
    pub const fn from_access_token(
        context: ApiStreamContext,
        password: &'a str,
        stream_id: &'a str,
        action_path: &'a str,
    ) -> Self {
        Self { context, access_token: true, username: "", password, stream_id, action_path }
    }
}

#[derive(Serialize, Deserialize)]
struct XtreamCategoryEntry {
    #[serde(deserialize_with = "deserialize_as_string")]
    category_id: String,
    category_name: String,
    #[serde(default)]
    parent_id: u32,
}

pub(in crate::api) fn get_xtream_player_api_stream_url(
    input: &ConfigInput,
    context: ApiStreamContext,
    action_path: &str,
    fallback_url: &Arc<str>,
) -> Option<Arc<str>> {
    // The resolved M3U archive URL is authoritative for timeshift requests.
    if context == ApiStreamContext::Timeshift && input.input_type.is_m3u() && !fallback_url.is_empty() {
        return Some(fallback_url.clone());
    }

    if input.input_type.is_media_server() {
        return (!fallback_url.is_empty()).then(|| fallback_url.clone());
    }

    if let Some(input_user_info) = input.get_user_info() {
        let ctx = match context {
            ApiStreamContext::LiveAlt | ApiStreamContext::Live => {
                let use_prefix = input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix);
                String::from(if use_prefix { "live" } else { "" })
            }
            ApiStreamContext::Movie | ApiStreamContext::Series | ApiStreamContext::Timeshift => context.to_string(),
        };
        let mut parts = vec![
            trim_slash(&input_user_info.base_url),
            trim_slash(&ctx),
            trim_slash(&input_user_info.username),
            trim_slash(&input_user_info.password),
            trim_slash(action_path),
        ];
        parts.retain(|s| !s.is_empty());
        Some(parts.join("/").into())
    } else if !fallback_url.is_empty() {
        Some(fallback_url.clone())
    } else {
        None
    }
}

async fn get_user_info(user: &ProxyUserCredentials, app_state: &AppState) -> Option<XtreamAuthorizationResponse> {
    let server_info = app_state.app_config.get_user_server_info(user)?;
    let active_connections = app_state.get_active_connections_for_user(&user.username).await;

    Some(XtreamAuthorizationResponse::new(
        &server_info,
        user,
        active_connections,
        app_state.app_config.config.load().user_access_control,
    ))
}

#[allow(clippy::too_many_lines)]
async fn xtream_player_api_stream(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    stream_req: ApiStreamRequest<'_>,
    user_target: Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)>,
) -> impl IntoResponse + Send {
    // if log::log_enabled!(log::Level::Debug) {
    //     debug!(
    //         "Stream request ctx={} user={} stream_id={} action_path={}",
    //         stream_req.context,
    //         sanitize_sensitive_info(stream_req.username),
    //         sanitize_sensitive_info(stream_req.stream_id),
    //         sanitize_sensitive_info(stream_req.action_path),
    //     );
    //     let message = format!("Client Request headers {req_headers:?}");
    //     debug!("{}", sanitize_sensitive_info(&message));
    //     let message = format!("Client Request headers {req_headers:?}");
    //     debug!("{}", sanitize_sensitive_info(&message));
    // }

    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = match user_target {
        None => {
            let Some((user, target)) =
                get_user_target_by_credentials(stream_req.username, stream_req.password, api_req, app_state)
            else {
                return auth_status.into_response();
            };
            (user, target)
        }
        Some((user, target)) => (user, target),
    };
    // Network access check only - permission check is done later with full stream info
    if let Err(e) = check_network_access_only(&user, fingerprint, app_state) {
        return e.into_player_response(auth_status);
    }

    let _guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;
    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let is_hls_manifest_request = stream_ext == Some(HLS_EXT);

    let target_name = &target.name;
    if !target.has_output(TargetType::Xtream) {
        debug!("Target has no xtream codes playlist {target_name}");
        if is_hls_manifest_request {
            // Preserve plain auth-status behaviour for HLS manifest probes —
            // returning an HLS manifest body (even with 404) breaks auth-probes
            // and monitoring/observability that assert on the original 401/403.
            return auth_status.into_response();
        }
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();
    }

    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let Ok(mut pli) = xtream_get_item_for_stream_id(req_virtual_id, app_state, &target, None).await else {
        error!("Failed to read xtream item for stream id {req_virtual_id}");
        if is_hls_manifest_request {
            return hls_custom_video_manifest_response(
                app_state,
                &user,
                CustomVideoStreamType::ChannelUnavailable,
                axum::http::StatusCode::NOT_FOUND,
            );
        }
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();
    };

    let input_option = app_state.app_config.get_input_by_name(&pli.input_name);
    let stream_ext = input_option
        .as_deref()
        .map_or(stream_ext, |input| override_live_hls_extension(stream_req.context, input, stream_ext));
    let is_hls_manifest_request = stream_ext == Some(HLS_EXT);

    let output_allowed = if stream_req.context == ApiStreamContext::Timeshift {
        user.allows_cluster(XtreamCluster::Live)
    } else {
        user.allows_item_type(pli.item_type)
    };
    if !output_allowed {
        if is_hls_manifest_request {
            return hls_custom_video_manifest_response(
                app_state,
                &user,
                CustomVideoStreamType::ChannelUnavailable,
                axum::http::StatusCode::NOT_FOUND,
            );
        }
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();
    }

    let virtual_id = pli.virtual_id;
    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let input = try_option_bad_request!(
        input_option,
        true,
        format!(
            "Can't find input {} for target {target_name}, context {}, stream_id {virtual_id}",
            pli.input_name, stream_req.context
        )
    );

    if user.permission_denied(app_state) {
        let stream_channel = create_stream_channel_with_type(target.id, &pli, pli.item_type);
        if is_hls_playback_request(stream_ext, &pli) {
            return hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                &user,
                stream_channel,
                pli.input_name.clone(),
                req_headers,
                ConnectFailureReason::UserAccountExpired,
            );
        }
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            stream_channel,
            pli.input_name.clone(),
            req_headers,
            ConnectFailureReason::UserAccountExpired,
        );
    }

    let m3u_timeshift = if stream_req.context == ApiStreamContext::Timeshift {
        try_result_bad_request!(
            resolve_m3u_xtream_timeshift(&input, &pli, stream_req.action_path),
            true,
            format!("M3U Xtream timeshift rejected for stream {virtual_id}")
        )
    } else {
        None
    };
    if let Some(resolved) = m3u_timeshift.as_ref() {
        pli.url = resolved.url.as_str().intern();
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

    let (cluster, item_type) = if stream_req.context == ApiStreamContext::Timeshift {
        (XtreamCluster::Video, PlaylistItemType::Catchup)
    } else {
        (pli.xtream_cluster, pli.item_type)
    };

    pli.url = match resolve_initial_stalker_playback_url(
        app_state,
        &input,
        pli.provider_id,
        pli.xtream_cluster,
        item_type,
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

    let requested_extension = if m3u_timeshift.is_some() {
        extract_extension_from_url(&pli.url).map(|ext| concat_string!(".", ext))
    } else {
        resolve_xtream_playback_extension(stream_ext, &pli)
    }
    .unwrap_or_default();
    let stream_ext = (!requested_extension.is_empty()).then_some(requested_extension.as_str());

    debug_if_enabled!(
        "ID chain for xtream endpoint: request_stream_id={} -> action_stream_id={action_stream_id} -> req_virtual_id={req_virtual_id} -> virtual_id={virtual_id}",
        stream_req.stream_id);
    // Derive the playback extension from get_query_path so session semantics match
    // the actual path it will route. Falls back to requested_extension when empty.
    #[allow(clippy::needless_borrow, clippy::borrow_deref_ref)]
    let (_, playback_ext) = get_query_path(stream_req.action_path, Some(&requested_extension), &pli, app_state);
    #[allow(clippy::needless_borrow, clippy::borrow_deref_ref)]
    let playback_ext: &str = if playback_ext.is_empty() { &requested_extension } else { &playback_ext };

    let session_key = if let Some(resolved) = m3u_timeshift.as_ref() {
        create_m3u_catchup_session_key(fingerprint, &user.username, virtual_id, &resolved.discriminator)
    } else if item_type == PlaylistItemType::Catchup {
        create_catchup_session_key(fingerprint, &user.username, virtual_id)
    } else {
        create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, item_type, Some(playback_ext))
    };
    let eviction_reentry_guard = if item_type == PlaylistItemType::Catchup
        || !crate::api::api_utils::is_socket_bound_playback_session(item_type, Some(playback_ext))
    {
        crate::api::api_utils::EvictionReentryGuard::Session(&session_key)
    } else {
        crate::api::api_utils::EvictionReentryGuard::SocketPlayback { virtual_id }
    };
    let user_session = app_state.active_users.get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);
            if playback_ext == HLS_EXT {
                return hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    stream_channel,
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::UserConnectionsExhausted,
                );
            }
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);
            if playback_ext == HLS_EXT {
                return hls_admission_failure_manifest_response(
                    app_state,
                    fingerprint,
                    &user,
                    stream_channel,
                    session.provider.clone(),
                    req_headers,
                    ConnectFailureReason::ProviderConnectionsExhausted,
                );
            }
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }

        let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);

        if session.virtual_id == virtual_id && is_seekable_media_request(cluster, req_headers, Some(playback_ext)) {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                stream_channel,
                api_utils::ForceStreamRequestContext {
                    req_headers,
                    input: &input,
                    user: &user,
                    session_reservation_ttl_secs: get_session_reservation_ttl_secs(app_state, item_type),
                    content_representation:
                        crate::api::model::ProviderContentRepresentationMode::for_playback_extension(playback_ext),
                },
                None,
            )
            .await
            .into_response();
        }

        session.stream_url.clone()
    } else {
        pli.url.clone()
    };

    let (connection_admission, grace_mode, request_class) = crate::api::api_utils::resolve_playback_request_admission(
        app_state,
        &user,
        fingerprint,
        item_type,
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
        is_stream_share_enabled(item_type, &target),
        user_session.as_ref(),
        virtual_id,
        session_url.as_ref(),
    );
    if connection_permission == UserConnectionPermission::Exhausted && !allow_exhausted_shared_reconnect {
        let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);
        if playback_ext == HLS_EXT {
            return hls_admission_failure_manifest_response(
                app_state,
                fingerprint,
                &user,
                stream_channel,
                input.name.clone(),
                req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            req_headers,
            ConnectFailureReason::UserConnectionsExhausted,
        );
    }

    let context = stream_req.context;

    let redirect_params = RedirectParams {
        item: &pli,
        provider_id: pli.get_provider_id(),
        cluster,
        target_type: TargetType::Xtream,
        target: &target,
        input: &input,
        user: &user,
        stream_ext,
        req_context: context,
        action_path: stream_req.action_path,
    };
    if let Some(response) = redirect_response(app_state, &redirect_params).await {
        return response.into_response();
    }

    #[allow(clippy::needless_borrow)]
    let (query_path, _extension) = get_query_path(stream_req.action_path, Some(&requested_extension), &pli, app_state);

    let stream_url = try_option_bad_request!(
        get_xtream_player_api_stream_url(&input, stream_req.context, &query_path, &session_url),
        true,
        format!(
            "Can't find stream url for target {target_name}, context {}, stream_id {virtual_id}",
            stream_req.context
        )
    );

    let is_session_request = is_session_based_playback(item_type, Some(playback_ext));
    // Reverse proxy mode — only route genuine HLS into the HLS handler, not DASH
    if is_session_request && playback_ext == shared::defaults::HLS_EXT {
        let Some(stream_identity) = HlsEntryStreamIdentity::from_playlist_item(&pli) else {
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
            &stream_url,
            None,
            stream_identity,
            &input,
            req_headers,
            connection_permission,
            connection_admission.kind,
            &original_hls_entry_path,
        )
        .await
        .into_response();
    }

    let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);

    let pinned_provider =
        user_session.as_ref().filter(|_| item_type.requires_provider_affinity()).map(|session| &session.provider);

    stream_response(
        fingerprint,
        app_state,
        session_key.as_str(),
        Some(request_class),
        stream_channel,
        &stream_url,
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

pub(crate) fn get_query_path(
    action_path: &str,
    stream_ext: Option<&str>,
    pli: &XtreamPlaylistItem,
    app_state: &Arc<AppState>,
) -> (String, String) {
    let discard_extension = if pli.item_type.is_live() {
        app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&pli.input_name)
            .as_ref()
            .is_some_and(|i| i.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension))
    } else {
        false
    };

    let extension: String = if discard_extension {
        String::new()
    } else if let Some(ext) = stream_ext {
        ext.into()
    } else {
        extract_extension_from_url(&pli.url).map_or_else(String::new, ToString::to_string)
    };

    let provider_id = pli.provider_id.to_string();

    let query_path = if action_path.is_empty() {
        concat_string!(&provider_id, &extension)
    } else {
        let path = trim_slash(action_path);
        concat_string!(path.as_ref(), "/", &provider_id, &extension)
    };
    (query_path, extension)
}

fn resolve_xtream_playback_extension(stream_ext: Option<&str>, pli: &XtreamPlaylistItem) -> Option<String> {
    let requested_extension = stream_ext.filter(|ext| !ext.is_empty()).map(ToString::to_string);
    let canonical_extension = pli
        .get_container_extension()
        .filter(|ext| !ext.is_empty())
        .map(|ext| concat_string!(".", ext.as_ref()))
        .or_else(|| extract_extension_from_url(&pli.url).map(ToString::to_string));

    if pli.item_type.is_live() {
        requested_extension.or(canonical_extension)
    } else {
        canonical_extension.or(requested_extension)
    }
}

// M3U timeshift must use the resolved archive URL instead of reconstructing an Xtream URL.
fn resolve_m3u_xtream_timeshift(
    input: &ConfigInput,
    item: &XtreamPlaylistItem,
    action_path: &str,
) -> Result<Option<ResolvedM3uCatchup>, TuliproxError> {
    if !input.input_type.is_m3u() {
        return Ok(None);
    }
    let (duration, start) = action_path.split_once('/').ok_or_else(|| {
        TuliproxError::ApiXtream(
            "M3U Xtream timeshift requires action_path in 'duration/start' form".to_string(),
        )
    })?;
    let props = item.additional_properties.as_ref().ok_or_else(|| {
        TuliproxError::ApiXtream("M3U Xtream timeshift item has no stream properties".to_string())
    })?;
    let StreamProperties::Live(live) = props else {
        return Err(TuliproxError::ApiXtream(
            "M3U Xtream timeshift requires a live stream".to_string(),
        ));
    };
    let catchup = live.catchup.as_ref().ok_or_else(|| {
        TuliproxError::ApiXtream(
            "M3U Xtream timeshift item has no catch-up metadata".to_string(),
        )
    })?;
    let resolved_source = input.resolve_url(&item.url)?;
    let resolved = resolve_xtream_m3u_catchup_url(resolved_source.as_ref(), catchup, start, duration)?;
    Ok(Some(resolved))
}

// Advertise M3U archives only when the timeshift bridge supports their template.
fn pli_supports_archive(app_state: &Arc<AppState>, pli: &XtreamPlaylistItem) -> bool {
    let Some(StreamProperties::Live(live)) = pli.additional_properties.as_ref() else {
        return false;
    };
    if live.tv_archive.unwrap_or(0) <= 0 {
        return false;
    }
    let Some(input) = app_state.app_config.get_input_by_name(&pli.input_name) else {
        return false;
    };
    if input.input_type.is_m3u() {
        let Some(catchup) = live.catchup.as_ref() else {
            return false;
        };
        match input.resolve_url(&pli.url) {
            Ok(resolved) => is_xtream_m3u_catchup_supported(resolved.as_ref(), catchup),
            Err(_) => false,
        }
    } else {
        true
    }
}

fn is_hls_playback_request(stream_ext: Option<&str>, pli: &XtreamPlaylistItem) -> bool {
    resolve_xtream_playback_extension(stream_ext, pli).as_deref() == Some(HLS_EXT)
}

/// Rewrites a live Xtream `.m3u8` request to `.ts` when the input has the
/// `disable_hls_streaming` option enabled. Returns the borrowed/static
/// extension string and performs no allocation.
fn override_live_hls_extension<'a>(
    context: ApiStreamContext,
    input: &ConfigInput,
    stream_ext: Option<&'a str>,
) -> Option<&'a str> {
    if input.has_flag(ConfigInputFlags::DisableHlsStreaming)
        && input.input_type.is_xtream()
        && matches!(context, ApiStreamContext::Live | ApiStreamContext::LiveAlt)
        && stream_ext == Some(HLS_EXT)
    {
        Some(TS_EXT)
    } else {
        stream_ext
    }
}

#[allow(clippy::too_many_lines)]
// Used by webui
pub(in crate::api) async fn xtream_player_api_stream_with_token(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    target_id: u16,
    stream_req: ApiStreamRequest<'_>,
) -> impl IntoResponse + Send {
    if stream_req.access_token && !verify_access_token(stream_req.password, &app_state.app_config.access_token_secret) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
        let target_name = &target.name;
        if !target.has_output(TargetType::Xtream) {
            debug!("Target has no xtream output {target_name}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
        let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
        let mut pli = try_result_bad_request!(
            xtream_get_item_for_stream_id(req_virtual_id, app_state, &target, None).await,
            true,
            format!("Failed to read xtream item for stream id {req_virtual_id}")
        );
        let virtual_id = pli.virtual_id;
        let input_option = app_state.app_config.get_input_by_name(&pli.input_name);
        let stream_ext = input_option
            .as_deref()
            .map_or(stream_ext, |input| override_live_hls_extension(stream_req.context, input, stream_ext));
        let input = try_option_bad_request!(
            input_option,
            true,
            format!(
                "Can't find input {} for target {target_name}, context {}, stream_id {}",
                pli.input_name, stream_req.context, pli.virtual_id
            )
        );

        // The token stream route has no action path from which to resolve an M3U archive.
        if stream_req.context == ApiStreamContext::Timeshift && input.input_type.is_m3u() {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let user = create_api_proxy_user(app_state);

        if pli.item_type.is_local() {
            let playback_session_token = create_session_fingerprint(fingerprint, "webui", virtual_id, false);
            return local_stream_response(
                fingerprint,
                app_state,
                pli.to_stream_channel(target.id),
                req_headers,
                &input,
                &target,
                &user,
                UserConnectionPermission::Allowed,
                crate::api::model::ConnectionKind::Normal,
                Some(playback_session_token.as_str()),
                None,
                true,
            )
            .await
            .into_response();
        }

        let resolution_item_type = if stream_req.context == ApiStreamContext::Timeshift {
            PlaylistItemType::Catchup
        } else {
            pli.item_type
        };
        pli.url = match resolve_initial_stalker_playback_url(
            app_state,
            &input,
            pli.provider_id,
            pli.xtream_cluster,
            resolution_item_type,
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

        let requested_extension = resolve_xtream_playback_extension(stream_ext, &pli);

        let (query_path, playback_ext) =
            get_query_path(stream_req.action_path, requested_extension.as_deref(), &pli, app_state);
        let playback_ext: Option<&str> =
            if playback_ext.is_empty() { requested_extension.as_deref() } else { Some(&*playback_ext) };

        let is_session_request = is_session_based_playback(pli.item_type, playback_ext);
        let session_key =
            create_playback_session_fingerprint(fingerprint, "webui", virtual_id, pli.item_type, playback_ext);

        // TODO how should we use fixed provider for hls in multi provider config?

        // Reverse proxy mode — only route genuine HLS into the HLS handler, not DASH
        if is_session_request && playback_ext == Some(shared::defaults::HLS_EXT) {
            let Some(stream_identity) = HlsEntryStreamIdentity::from_playlist_item(&pli) else {
                error!("HLS input stream identity missing for virtual_id={virtual_id}; refresh target playlist");
                return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, virtual_id);
            return handle_hls_stream_request(
                fingerprint,
                app_state,
                &user,
                &target,
                None,
                &pli.url,
                None,
                stream_identity,
                &input,
                req_headers,
                UserConnectionPermission::Allowed,
                Some(crate::api::model::ConnectionKind::Normal),
                &original_hls_entry_path,
            )
            .await
            .into_response();
        }

        let stream_url = try_option_bad_request!(
            get_xtream_player_api_stream_url(&input, stream_req.context, &query_path, &pli.url),
            true,
            format!(
                "Can't find stream url for target {target_name}, context {}, stream_id {}",
                stream_req.context, virtual_id
            )
        );

        trace_if_enabled!("Streaming stream request from {}", sanitize_sensitive_info(&stream_url));
        stream_response(
            fingerprint,
            app_state,
            session_key.as_str(),
            None,
            pli.to_stream_channel(target.id),
            &stream_url,
            None,
            req_headers,
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            false,
            None,
        )
        .await
        .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

async fn xtream_player_api_resource(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    api_req: &UserApiRequest,
    app_state: &Arc<AppState>,
    resource_req: ApiStreamRequest<'_>,
) -> impl IntoResponse {
    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) =
        get_user_target_by_credentials(resource_req.username, resource_req.password, api_req, app_state)
    else {
        return auth_status.into_response();
    };
    if let Err(e) = check_permission_and_network_access_only(&user, fingerprint, app_state) {
        return e.into_player_response(auth_status);
    }
    let target_name = &target.name;
    if !target.has_output(TargetType::Xtream) {
        debug!("Target has no xtream output {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let req_virtual_id: u32 = try_result_bad_request!(resource_req.stream_id.trim().parse());
    let resource = resource_req.action_path.trim();
    let pli = try_result_bad_request!(
        xtream_get_item_for_stream_id(req_virtual_id, app_state, &target, None).await,
        true,
        format!("Failed to read xtream item for stream id {req_virtual_id}")
    );

    if !user.allows_item_type(pli.item_type) {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    let stream_url = pli.resolve_resource_url(resource);

    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) {
                let input = app_state.app_config.get_input_by_name(&pli.input_name);
                let redirect_url = api_utils::resolve_redirect_location(input.as_deref(), &url);
                match redirect_url {
                    Ok(redirect_url) => {
                        trace_if_enabled!(
                            "Redirecting resource request to {}",
                            sanitize_sensitive_info(redirect_url.as_ref())
                        );
                        redirect(redirect_url.as_ref()).into_response()
                    }
                    Err(err) => {
                        error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                        axum::http::StatusCode::BAD_REQUEST.into_response()
                    }
                }
            } else {
                trace_if_enabled!("Resource request to {}", sanitize_sensitive_info(&url));
                resource_response(app_state, &url, req_headers, None).await.into_response()
            }
        }
    }
}

macro_rules! create_xtream_player_api_stream {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            fingerprint: Fingerprint,
            req_headers: HeaderMap,
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(String, String, String)>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
        ) -> impl IntoResponse + Send {
            xtream_player_api_stream(
                &fingerprint,
                &req_headers,
                &app_state,
                &api_req,
                ApiStreamRequest::from($context, &username, &password, &stream_id, ""),
                None,
            )
            .await
            .into_response()
        }
    };
}

macro_rules! create_xtream_player_api_resource {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            fingerprint: Fingerprint,
            axum::extract::Path((username, password, stream_id, resource)): axum::extract::Path<(
                String,
                String,
                String,
                String,
            )>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
            req_headers: HeaderMap,
        ) -> impl IntoResponse {
            xtream_player_api_resource(
                &fingerprint,
                &req_headers,
                &api_req,
                &app_state,
                ApiStreamRequest::from($context, &username, &password, &stream_id, &resource),
            )
            .await
            .into_response()
        }
    };
}

create_xtream_player_api_stream!(xtream_player_api_live_stream, ApiStreamContext::Live);
create_xtream_player_api_stream!(xtream_player_api_live_stream_alt, ApiStreamContext::LiveAlt);
create_xtream_player_api_stream!(xtream_player_api_series_stream, ApiStreamContext::Series);
create_xtream_player_api_stream!(xtream_player_api_movie_stream, ApiStreamContext::Movie);

create_xtream_player_api_resource!(xtream_player_api_live_resource, ApiStreamContext::Live);
create_xtream_player_api_resource!(xtream_player_api_series_resource, ApiStreamContext::Series);
create_xtream_player_api_resource!(xtream_player_api_movie_resource, ApiStreamContext::Movie);

fn empty_stream_info_response(cluster: XtreamCluster) -> axum::response::Response {
    match cluster {
        XtreamCluster::Video => try_unwrap_body!(empty_json_response_as_object()),
        XtreamCluster::Live | XtreamCluster::Series => try_unwrap_body!(empty_json_response_as_array()),
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
struct XtreamApiTimeShiftRequest {
    username: String,
    password: String,
    duration: String,
    start: String,
    stream_id: String,
}

async fn xtream_player_api_timeshift_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::Path(timeshift_request): axum::extract::Path<XtreamApiTimeShiftRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(query_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    let path_req = UserApiRequest {
        username: timeshift_request.username,
        password: timeshift_request.password,
        duration: timeshift_request.duration,
        start: timeshift_request.start,
        stream_id: timeshift_request.stream_id,
        ..UserApiRequest::default()
    };
    let api_req = UserApiRequest::merge_prefer_primary(&path_req, &query_req);

    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) =
        get_user_target_by_credentials(&api_req.username, &api_req.password, &api_req, &app_state)
    else {
        return auth_status.into_response();
    };

    let epg_timeshift = parse_timeshift(user.epg_request_timeshift.as_deref());
    let start = apply_timeshift(&api_req.start, &epg_timeshift);
    let action_path = if start.is_empty() {
        format!("{}/{}", api_req.duration, api_req.start)
    } else {
        format!("{}/{}", api_req.duration, start)
    };

    xtream_player_api_stream(
        &fingerprint,
        &req_headers,
        &app_state,
        &api_req,
        ApiStreamRequest::from(
            ApiStreamContext::Timeshift,
            &api_req.username,
            &api_req.password,
            &api_req.stream_id,
            &action_path,
        ),
        Some((user, target)),
    )
    .await
    .into_response()
}

async fn xtream_player_api_timeshift_query_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    if api_req.username.is_empty()
        || api_req.password.is_empty()
        || api_req.stream.is_empty()
        || api_req.duration.is_empty()
        || api_req.start.is_empty()
    {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) =
        get_user_target_by_credentials(&api_req.username, &api_req.password, &api_req, &app_state)
    else {
        return auth_status.into_response();
    };

    let epg_timeshift = parse_timeshift(user.epg_request_timeshift.as_deref());
    let start = apply_timeshift(&api_req.start, &epg_timeshift);
    let action_path = if start.is_empty() {
        format!("{}/{}", api_req.duration, api_req.start)
    } else {
        format!("{}/{}", api_req.duration, start)
    };

    xtream_player_api_stream(
        &fingerprint,
        &req_headers,
        &app_state,
        &api_req,
        ApiStreamRequest::from(
            ApiStreamContext::Timeshift,
            &api_req.username,
            &api_req.password,
            &api_req.stream,
            &action_path,
        ),
        Some((user, target)),
    )
    .await
    .into_response()
}

#[allow(clippy::too_many_lines)]
pub async fn xtream_get_stream_info_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    cluster: XtreamCluster,
) -> impl IntoResponse + Send {
    if !user.allows_cluster(cluster) {
        return match cluster {
            XtreamCluster::Video => try_unwrap_body!(empty_json_response_as_object()),
            XtreamCluster::Live | XtreamCluster::Series => try_unwrap_body!(empty_json_response_as_array()),
        };
    }

    let virtual_id: u32 = match FromStr::from_str(stream_id) {
        Ok(id) => id,
        Err(_) => return try_unwrap_body!(empty_json_response_as_object()),
    };

    let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, Some(cluster)).await else {
        return empty_stream_info_response(cluster);
    };

    let input = app_state.app_config.get_input_by_name(&pli.input_name);
    let is_media_server = input.as_ref().is_some_and(|i| i.input_type.is_media_server());
    // handle local items and media server
    if pli.item_type.is_local() || is_media_server {
        let Some(xtream_output) = target.get_xtream_output() else {
            return empty_stream_info_response(cluster);
        };

        let encrypt_secret = app_state.get_encrypt_secret();
        let options = match xtream_mapping_option_from_target_options(
            target,
            xtream_output,
            &app_state.app_config,
            user,
            encrypt_secret,
        ) {
            Ok(options) => options,
            Err(err) => {
                error!("{err}");
                return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
        return axum::Json(pli.to_info_document(&options)).into_response();
    }

    // handle upstream provider
    if pli.provider_id > 0 {
        if let Some(input) = input {
            if let Some(info_url) = xtream::get_xtream_player_api_info_url(&input, cluster, pli.provider_id) {
                // redirect is only possible for live streams
                if user.proxy == ProxyType::Redirect && cluster == XtreamCluster::Live {
                    return match api_utils::resolve_redirect_location(Some(&input), &info_url) {
                        Ok(redirect_url) => redirect(redirect_url.as_ref()).into_response(),
                        Err(err) => {
                            error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                            axum::http::StatusCode::BAD_REQUEST.into_response()
                        }
                    };
                }

                // fetch info from the upstream provider
                if let Ok(content) = xtream::get_xtream_stream_info(
                    &app_state.http_client.load(),
                    app_state,
                    user,
                    &input,
                    target,
                    &pli,
                    info_url.as_str(),
                    cluster,
                )
                .await
                {
                    return try_unwrap_body!(axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                        .body(axum::body::Body::from(content)));
                }
            }
        }
    }

    // fallback with basic info
    match cluster {
        XtreamCluster::Video => {
            let content = create_vod_info_from_item(&pli);
            try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(axum::body::Body::from(content)))
        }
        XtreamCluster::Live | XtreamCluster::Series => {
            try_unwrap_body!(empty_json_response_as_array())
        }
    }
}

async fn xtream_get_short_epg(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    limit: u32,
) -> impl IntoResponse + Send {
    if !user.allows_cluster(XtreamCluster::Live) {
        return axum::Json(json!(ShortEpgResultDto::default())).into_response();
    }

    let target_name = &target.name;
    if target.has_output(TargetType::Xtream) {
        let virtual_id: u32 = match FromStr::from_str(stream_id.trim()) {
            Ok(id) => id,
            Err(_) => return get_empty_epg_response().into_response(),
        };

        if let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, None).await {
            let config = &app_state.app_config.config.load();
            let has_archive = pli_supports_archive(app_state, &pli);
            if let (Some(epg_path), Some(channel_id)) = (
                get_epg_path_for_target_by_type(config, target, TargetType::Xtream),
                &pli.epg_channel_id,
            ) {
                if file_exists_async(&epg_path).await {
                    return serve_short_epg(
                        app_state,
                        epg_path.as_path(),
                        user,
                        target,
                        channel_id,
                        stream_id.intern(),
                        limit,
                        has_archive,
                    )
                    .await;
                }
            }

            if pli.provider_id > 0 {
                let input_name = &pli.input_name;
                if let Some(input) = app_state.app_config.get_input_by_name(input_name) {
                    if let Some(action_url) =
                        xtream::get_xtream_player_api_action_url(&input, crate::model::XC_ACTION_GET_SHORT_EPG)
                    {
                        let mut info_url =
                            format!("{action_url}&{}={}", crate::model::XC_TAG_STREAM_ID, pli.provider_id);
                        if limit > 0 {
                            info_url = format!("{info_url}&limit={limit}");
                        }
                        if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) {
                            return match api_utils::resolve_redirect_location(Some(&input), &info_url) {
                                Ok(redirect_url) => redirect(redirect_url.as_ref()).into_response(),
                                Err(err) => {
                                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
                                    axum::http::StatusCode::BAD_REQUEST.into_response()
                                }
                            };
                        }

                        let input_source = InputSource::from(&*input).with_url(info_url);
                        return match request::download_text_content(
                            &app_state.app_config,
                            &app_state.http_client.load(),
                            &input_source,
                            None,
                            None,
                            false,
                        )
                        .await
                        {
                            Ok((content, _)) => (
                                axum::http::StatusCode::OK,
                                [(axum::http::header::CONTENT_TYPE.to_string(), mime::APPLICATION_JSON.to_string())],
                                content,
                            )
                                .into_response(),
                            Err(err) => {
                                error!("Failed to download epg {}", sanitize_sensitive_info(&err.to_string()));
                                axum::Json(json!(ShortEpgResultDto::default())).into_response()
                            }
                        };
                    }
                }
            }
        }
    }
    warn!("Can't find short epg with id: {target_name}/{stream_id}");
    axum::Json(json!(ShortEpgResultDto::default())).into_response()
}

async fn xtream_player_api_handle_content_action(
    config: &Config,
    target_name: &str,
    action: &str,
    category_id: Option<u32>,
    user: &ProxyUserCredentials,
) -> Option<impl IntoResponse> {
    let (collection, cluster) = match action {
        crate::model::XC_ACTION_GET_LIVE_CATEGORIES => (storage_const::COL_CAT_LIVE, XtreamCluster::Live),
        crate::model::XC_ACTION_GET_VOD_CATEGORIES => (storage_const::COL_CAT_VOD, XtreamCluster::Video),
        crate::model::XC_ACTION_GET_SERIES_CATEGORIES => (storage_const::COL_CAT_SERIES, XtreamCluster::Series),
        // we dont handle this action
        _ => return None,
    };
    if !user.allows_cluster(cluster) {
        return Some(api_utils::empty_json_list_response().into_response());
    }
    if let Ok(file_path) = xtream_get_collection_path(config, target_name, collection) {
        match tokio::fs::read_to_string(&file_path).await {
            Ok(content) => {
                let filter =
                    user_get_bouquet_filter(config, &user.username, category_id, TargetType::Xtream, cluster).await;

                match serde_json::from_str::<Vec<XtreamCategoryEntry>>(&content) {
                    Ok(mut categories) => {
                        if let Some(fltr) = filter {
                            categories.retain(|c| fltr.contains(&c.category_id));
                        }
                        return Some(axum::Json(categories).into_response());
                    }
                    Err(err) => error!("Failed to parse json file {}: {err}", file_path.display()),
                }
            }
            Err(err) => error!("Failed to read collection file {}: {err}", file_path.display()),
        }
    }

    Some(api_utils::empty_json_list_response().into_response())
}

#[allow(clippy::too_many_lines)]
async fn xtream_get_catchup_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    start_time: &str,
    end_time: &str,
) -> impl IntoResponse + Send {
    if !user.allows_cluster(XtreamCluster::Live) {
        return axum::Json(json!(ShortEpgResultDto::default())).into_response();
    }

    let req_virtual_id: u32 = if let Ok(id) = stream_id.parse::<u32>() {
        id
    } else {
        return axum::Json(json!(ShortEpgResultDto::default())).into_response();
    };

    let pli = try_result_bad_request!(
        xtream_get_item_for_stream_id(req_virtual_id, app_state, target, Some(XtreamCluster::Live)).await
    );

    let input = try_option_bad_request!(app_state.app_config.get_input_by_name(&pli.input_name));

    let mut info_url = try_option_bad_request!(xtream::get_xtream_player_api_action_url(
        &input,
        crate::model::XC_ACTION_GET_CATCHUP_TABLE
    )
    .map(|action_url| format!("{action_url}&{}={}", crate::model::XC_TAG_STREAM_ID, pli.provider_id)));

    if !start_time.is_empty() && !end_time.is_empty() {
        let epg_timeshift = parse_timeshift(user.epg_request_timeshift.as_deref());
        let start = apply_timeshift(start_time, &epg_timeshift);
        let end = apply_timeshift(end_time, &epg_timeshift);
        if !start.is_empty() && !end.is_empty() {
            let _ = write!(info_url, "&start={start}&end={end}");
        }
    }

    let input_source = InputSource::from(&*input).with_url(info_url);
    let content = try_result_bad_request!(
        xtream::get_xtream_stream_info_content(
            &app_state.app_config,
            &app_state.http_client.load(),
            &input_source,
            false,
        )
        .await
    );

    let mut doc: Map<String, Value> = try_result_bad_request!(serde_json::from_str(&content));
    let epg_listings =
        try_option_bad_request!(doc.get_mut(crate::model::XC_TAG_EPG_LISTINGS).and_then(Value::as_array_mut));

    // Collect data and generate UUIDs without holding the lock.
    let mut tasks = Vec::new();
    let pli_uuid_str = pli.get_uuid().to_string();

    for (idx, epg_list_item) in epg_listings.iter().enumerate() {
        if let Some(cp_id) =
            epg_list_item.get(crate::model::XC_TAG_ID).and_then(Value::as_str).and_then(|id| id.parse::<u32>().ok())
        {
            let uuid = generate_provider_playlist_uuid(&pli_uuid_str, &cp_id.to_string(), pli.item_type);
            tasks.push((idx, uuid, cp_id));
        }
    }

    let config = &app_state.app_config.config.load();
    let target_path = try_option_bad_request!(get_target_storage_path(config, target.name.as_str()));

    let mut mapping_results = Vec::with_capacity(tasks.len());
    let mut in_memory_updates = Vec::new();

    if !tasks.is_empty() {
        {
            let Ok((mut target_id_mapping, file_lock)) =
                get_target_id_mapping(&app_state.app_config, &target_path, target.use_memory_cache).await
            else {
                return internal_server_error!();
            };

            for (idx, uuid, cp_id) in tasks {
                let virtual_id = target_id_mapping.get_and_update_virtual_id(
                    &uuid,
                    cp_id,
                    PlaylistItemType::Catchup,
                    pli.provider_id,
                );

                mapping_results.push((idx, virtual_id));

                if target.use_memory_cache {
                    in_memory_updates.push(VirtualIdRecord::new(
                        cp_id,
                        virtual_id,
                        PlaylistItemType::Catchup,
                        pli.provider_id,
                        uuid,
                    ));
                }
            }

            if let Err(err) = target_id_mapping.persist() {
                error!("Failed to write catchup id mapping {err}");
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }

            // Lock is released here immediately after persist()
            drop(file_lock);
        }
    }

    // Apply the new virtual IDs back to the JSON document
    for (idx, v_id) in mapping_results {
        if let Some(item) = epg_listings.get_mut(idx).and_then(Value::as_object_mut) {
            item.insert(crate::model::XC_TAG_ID.to_string(), Value::String(v_id.to_string()));
        }
    }

    if target.use_memory_cache && !in_memory_updates.is_empty() {
        app_state.playlists.update_target_id_mapping(target, in_memory_updates).await;
    }

    serde_json::to_string(&doc).map_or_else(
        |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
        |result| {
            try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(result))
        },
    )
}

macro_rules! skip_json_response_if_flag_set {
    ($flag:expr, $stmt:expr) => {
        if $flag {
            return api_utils::empty_json_list_response().into_response();
        }
        return $stmt.into_response();
    };
}

macro_rules! skip_flag_optional {
    ($flag:expr, $stmt:expr) => {
        if $flag {
            None
        } else {
            Some($stmt)
        }
    };
}

#[allow(clippy::too_many_lines)]
async fn xtream_player_api(
    fingerprint: &Fingerprint,
    api_req: UserApiRequest,
    app_state: &Arc<AppState>,
) -> impl IntoResponse + Send {
    api_req.log_sanitized("xtream_player_api");
    let auth_status = app_state.app_config.get_auth_error_status();
    let Some((user, target)) = get_user_target(&api_req, app_state) else {
        return auth_status.into_response();
    };
    if let Err(e) = check_network_access_only(&user, fingerprint, app_state) {
        return e.into_player_response(auth_status);
    }
    if !target.has_output(TargetType::Xtream) {
        return get_user_info(&user, app_state).await.map_or_else(
            || axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
            |info| axum::response::Json(info).into_response(),
        );
    }

    let action = api_req.action.trim();
    if action.is_empty() {
        return get_user_info(&user, app_state).await.map_or_else(
            || axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
            |info| axum::response::Json(info).into_response(),
        );
    }

    if user.permission_denied(app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    // Process specific playlist actions
    let (skip_live, skip_vod, skip_series) =
        if let Some(inputs) = app_state.app_config.get_inputs_for_target(&target.name) {
            inputs.iter().fold((true, true, true), |acc, i| {
                let (live, vod, series) = acc;
                (
                    live && i.has_flag(ConfigInputFlags::SkipLive),
                    vod && i.has_flag(ConfigInputFlags::SkipVod),
                    series && i.has_flag(ConfigInputFlags::SkipSeries),
                )
            })
        } else {
            (false, false, false)
        };
    let skip_live = skip_live || !user.allows_cluster(XtreamCluster::Live);
    let skip_vod = skip_vod || !user.allows_cluster(XtreamCluster::Video);
    let skip_series = skip_series || !user.allows_cluster(XtreamCluster::Series);

    match action {
        crate::model::XC_ACTION_GET_ACCOUNT_INFO => {
            return get_user_info(&user, app_state).await.map_or_else(
                || axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                |info| axum::response::Json(info).into_response(),
            );
        }
        crate::model::XC_ACTION_GET_SERIES_INFO => {
            skip_json_response_if_flag_set!(
                skip_series,
                xtream_get_stream_info_response(
                    app_state,
                    &user,
                    &target,
                    api_req.series_id.trim(),
                    XtreamCluster::Series
                )
                .await
            );
        }
        crate::model::XC_ACTION_GET_VOD_INFO => {
            skip_json_response_if_flag_set!(
                skip_vod,
                xtream_get_stream_info_response(app_state, &user, &target, api_req.vod_id.trim(), XtreamCluster::Video)
                    .await
            );
        }
        crate::model::XC_ACTION_GET_EPG | crate::model::XC_ACTION_GET_SHORT_EPG => {
            return xtream_get_short_epg(app_state, &user, &target, api_req.stream_id.trim(), api_req.get_limit())
                .await
                .into_response();
        }
        crate::model::XC_ACTION_GET_CATCHUP_TABLE => {
            skip_json_response_if_flag_set!(
                skip_live,
                xtream_get_catchup_response(
                    app_state,
                    &user,
                    &target,
                    api_req.stream_id.trim(),
                    api_req.start.trim(),
                    api_req.end.trim()
                )
                .await
            );
        }
        _ => {}
    }

    let category_id = api_req.category_id.trim().parse::<u32>().ok();
    // Handle general content actions
    if let Some(response) = xtream_player_api_handle_content_action(
        &app_state.app_config.config.load(),
        &target.name,
        action,
        category_id,
        &user,
    )
    .await
    {
        return response.into_response();
    }

    let result = match action {
        crate::model::XC_ACTION_GET_LIVE_STREAMS => skip_flag_optional!(
            skip_live,
            xtream_load_rewrite_playlist(XtreamCluster::Live, app_state, &target, category_id, &user).await
        ),
        crate::model::XC_ACTION_GET_VOD_STREAMS => skip_flag_optional!(
            skip_vod,
            xtream_load_rewrite_playlist(XtreamCluster::Video, app_state, &target, category_id, &user).await
        ),
        crate::model::XC_ACTION_GET_SERIES => skip_flag_optional!(
            skip_series,
            xtream_load_rewrite_playlist(XtreamCluster::Series, app_state, &target, category_id, &user).await
        ),
        _ => Some(Err(TuliproxError::ApiXtream(format!("Unknown api call: {action} for target: {}", target.name)))),
    };

    match result {
        Some(result_iter) => {
            match result_iter {
                Ok(xtream_iter) => {
                    // Convert the iterator into a stream of `Bytes`
                    let content_stream = xtream_create_content_stream(xtream_iter);
                    try_unwrap_body!(axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                        .body(axum::body::Body::from_stream(content_stream)))
                }
                Err(err) => {
                    error!("Failed response for xtream target: {} action: {} error: {}", target.name, action, err);
                    get_user_info(&user, app_state).await.map_or_else(
                        || axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        |info| axum::response::Json(info).into_response(),
                    )
                }
            }
        }
        None => {
            // Some players fail on NoContent, so we return an empty array
            api_utils::empty_json_list_response().into_response()
        }
    }
}

fn xtream_create_content_stream<S>(xtream_iter: S) -> impl Stream<Item = Result<Bytes, String>>
where
    S: Stream<Item = Result<(String, bool), TuliproxError>> + Send + Unpin + 'static,
{
    let mapped = xtream_iter.map(move |entry| {
        entry.map_err(|error| error.to_string()).map(|(mut line, has_next)| {
            if has_next {
                line.push(',');
            }
            Bytes::from(line)
        })
    });
    coalesce_byte_stream(stream::once(async { Ok::<Bytes, String>(Bytes::from("[")) })
        .chain(mapped)
        .chain(stream::once(async { Ok::<Bytes, String>(Bytes::from("]")) })))
}

async fn xtream_player_api_get(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
) -> impl IntoResponse + Send {
    xtream_player_api(&fingerprint, api_req, &app_state).await
}

async fn xtream_player_api_post(
    fingerprint: Fingerprint,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    xtream_player_api(&fingerprint, api_req, &app_state).await
}

macro_rules! register_xtream_api {
    ($router:expr, [$($path:expr),*]) => {{
        $router
       $(
          .route($path, axum::routing::get(xtream_player_api_get).post(xtream_player_api_post))
            // $router.service(web::resource($path).route(web::get().to(xtream_player_api_get)).route(web::post().to(xtream_player_api_post)))
        )*
    }};
}

macro_rules! register_xtream_api_stream {
     ($router:expr, [$(($path:expr, $fn_name:ident)),*]) => {{
         $router
       $(
          .route(format!("{}/{{username}}/{{password}}/{{stream_id}}", $path).as_str(), axum::routing::get($fn_name))
            // $cfg.service(web::resource(format!("{}/{{username}}/{{password}}/{{stream_id}}", $path)).route(web::get().to($fn_name)));
        )*
    }};
}

macro_rules! register_xtream_api_resource {
     ($router:expr, [$(($path:expr, $fn_name:ident)),*]) => {{
         $router
       $(
           .route(format!("/resource/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}", $path).as_str(), axum::routing::get($fn_name))
            // $cfg.service(web::resource(format!("/resource/{}/{{username}}/{{password}}/{{stream_id}}/{{resource}}", $path)).route(web::get().to($fn_name)));
        )*
    }};
}

macro_rules! register_xtream_api_timeshift {
     ($router:expr, [$($path:expr),*]) => {{
         $router
       $(
          .route($path, axum::routing::get(xtream_player_api_timeshift_query_stream).post(xtream_player_api_timeshift_query_stream))
            //$cfg.service(web::resource($path).route(web::get().to(xtream_player_api_timeshift_stream)).route(web::post().to(xtream_player_api_timeshift_stream)));
        )*
    }};
}

async fn xtream_player_token_stream(
    fingerprint: Fingerprint,
    axum::extract::Path((token, target_id, cluster, stream_id)): axum::extract::Path<(String, u16, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: HeaderMap,
) -> impl IntoResponse + Send {
    let ctxt = try_result_bad_request!(ApiStreamContext::from_str(cluster.as_str()));
    xtream_player_api_stream_with_token(
        &fingerprint,
        &req_headers,
        &app_state,
        target_id,
        ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, ""),
    )
    .await
    .into_response()
}

pub fn xtream_api_register() -> axum::Router<Arc<AppState>> {
    let router = axum::Router::new();
    let mut router = register_xtream_api!(router, ["/player_api.php", "/panel_api.php", "/xtream"]);
    router = router
        .route("/token/{token}/{target_id}/{cluster}/{stream_id}", axum::routing::get(xtream_player_token_stream));
    router = register_xtream_api_stream!(
        router,
        [
            ("", xtream_player_api_live_stream_alt),
            ("/live", xtream_player_api_live_stream),
            ("/movie", xtream_player_api_movie_stream),
            ("/series", xtream_player_api_series_stream)
        ]
    );
    router = router.route(
        "/timeshift/{username}/{password}/{duration}/{start}/{stream_id}",
        axum::routing::get(xtream_player_api_timeshift_stream),
    );
    router = register_xtream_api_timeshift!(router, ["/timeshift.php", "/streaming/timeshift.php"]);
    register_xtream_api_resource!(
        router,
        [
            ("live", xtream_player_api_live_resource),
            ("movie", xtream_player_api_movie_resource),
            ("series", xtream_player_api_series_resource)
        ]
    )
}

#[cfg(test)]
mod tests {
    use super::{
        empty_stream_info_response, get_xtream_player_api_stream_url, resolve_m3u_xtream_timeshift,
        is_hls_playback_request, override_live_hls_extension, resolve_xtream_playback_extension,
        xtream_get_short_epg, xtream_player_api_stream, xtream_player_api_stream_with_token, ApiStreamContext,
        ApiStreamRequest, XtreamApiTimeShiftRequest,
    };
    use crate::auth::Fingerprint;
    use crate::{
        api::model::{
            create_test_app_state, AppState, PlaylistStorage, PlaylistXtreamStorage, UserApiRequest,
        },
        model::{
            Config, ConfigInput, ConfigTarget, Epg, IcsEpgSourceConfig, ProxyUserCredentials, SourcesConfig,
            TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
        },
        processing::parser::ics::parse_ics_file_to_channel,
        repository::{
            epg_write_file, xtream_get_epg_file_path_for_target, xtream_get_storage_path, BPlusTree,
            VirtualIdRecord,
        },
    };
    use arc_swap::ArcSwapOption;
    use axum::{http::HeaderMap, response::IntoResponse};
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
    use shared::{error::TuliproxError, foundation::Filter};
    use shared::{
        model::{
            ClusterFlags, InputType, PlaylistItemType, ProcessingOrder, ProxyUserStatus, StreamProperties, UUIDType,
            VideoStreamProperties, XtreamCluster, XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::sync::Arc;
    use tempfile::tempdir;

    #[test]
    fn live_hls_override_is_scoped_to_enabled_xtream_live_requests() {
        let enabled_xtream = provider_input_with_flag(InputType::Xtream, true);
        let disabled_xtream = provider_input_with_flag(InputType::Xtream, false);
        let enabled_m3u = provider_input_with_flag(InputType::M3u, true);
        let cases = [
            (ApiStreamContext::Live, &enabled_xtream, Some(".m3u8"), Some(".ts")),
            (ApiStreamContext::LiveAlt, &enabled_xtream, Some(".m3u8"), Some(".ts")),
            (ApiStreamContext::Live, &disabled_xtream, Some(".m3u8"), Some(".m3u8")),
            (ApiStreamContext::Live, &enabled_m3u, Some(".m3u8"), Some(".m3u8")),
            (ApiStreamContext::Timeshift, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
            (ApiStreamContext::Movie, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
            (ApiStreamContext::Series, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
            (ApiStreamContext::Live, &enabled_xtream, Some(".ts"), Some(".ts")),
            (ApiStreamContext::Live, &enabled_xtream, Some(".mpd"), Some(".mpd")),
            (ApiStreamContext::Live, &enabled_xtream, None, None),
        ];

        for (context, input, extension, expected) in cases {
            assert_eq!(
                override_live_hls_extension(context, input, extension),
                expected,
                "case: {context:?} input_type={:?} ext={extension:?}",
                input.input_type
            );
        }
    }

    fn provider_input_with_flag(input_type: InputType, disable_hls: bool) -> ConfigInput {
        let dto = shared::model::ConfigInputDto {
            name: "ts-provider".into(),
            input_type,
            url: "http://provider.test".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            enabled: true,
            options: Some(shared::model::ConfigInputOptionsDto {
                disable_hls_streaming: disable_hls,
                ..shared::model::ConfigInputOptionsDto::default()
            }),
            ..shared::model::ConfigInputDto::default()
        };
        ConfigInput::from(&dto)
    }

    #[test]
    fn hls_failure_response_uses_resolved_playback_extension() {
        let vod = create_test_vod_item("provider://strong/movie/user/pass/813563.mp4", "mp4", PlaylistItemType::Video);
        let hls_vod =
            create_test_vod_item("provider://strong/movie/user/pass/813564.m3u8", "m3u8", PlaylistItemType::Video);

        assert!(!is_hls_playback_request(Some(".m3u8"), &vod));
        assert!(is_hls_playback_request(None, &hls_vod));
    }

    async fn response_body_text(response: axum::response::Response) -> Result<String, String> {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|err| format!("failed to read response body: {err}"))?;
        String::from_utf8(body.to_vec()).map_err(|err| format!("response body is not UTF-8: {err}"))
    }

    fn short_epg_target() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "ics-xtream".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![TargetOutput::Xtream(XtreamTargetOutput {
                flags: XtreamTargetFlagsSet::new(),
                trakt: None,
                filter: None,
            })],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: true,
        }
    }

    fn short_epg_live_item() -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 100,
            provider_id: 0,
            name: "Formula 1".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Sports".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://example.invalid/live.ts".intern(),
            epg_channel_id: Some("f1.calendar".intern()),
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 1,
            input_name: "local".intern(),
            channel_no: 1,
            source_ordinal: 0,
            input_stream_id: "".intern(),
            source_user_agent: None,
        }
    }

    #[tokio::test]
    async fn xtream_short_epg_returns_imported_ics_programme_for_matching_channel_id() {
        let dir = tempdir().expect("temp dir");
        let ics_path = dir.path().join("calendar.ics");
        let start = chrono::Utc::now() + chrono::Duration::days(1);
        let stop = start + chrono::Duration::hours(1);
        std::fs::write(
            &ics_path,
            format!(
                concat!(
                    "BEGIN:VCALENDAR\r\n",
                    "VERSION:2.0\r\n",
                    "BEGIN:VEVENT\r\n",
                    "UID:f1-qualifying\r\n",
                    "DTSTART:{}\r\n",
                    "DTEND:{}\r\n",
                    "SUMMARY:Formula 1 Qualifying\r\n",
                    "DESCRIPTION:Imported from ICS\r\n",
                    "END:VEVENT\r\n",
                    "END:VCALENDAR\r\n",
                ),
                start.format("%Y%m%dT%H%M%SZ"),
                stop.format("%Y%m%dT%H%M%SZ"),
            ),
        )
        .expect("write ICS fixture");
        let channel = parse_ics_file_to_channel(
            &ics_path,
            "f1.calendar".intern(),
            Some("Formula 1".intern()),
            &IcsEpgSourceConfig::default(),
        )
        .await
        .expect("parse ICS fixture");

        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let target = Arc::new(short_epg_target());
        let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
        std::fs::create_dir_all(&xtream_storage).expect("create xtream storage");
        let epg_path = xtream_get_epg_file_path_for_target(&xtream_storage);
        epg_write_file(
            &target.name,
            &Epg {
                priority: 0,
                logo_override: false,
                attributes: None,
                children: vec![Arc::new(channel)],
            },
            &epg_path,
            &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
            &shared::model::EpgOutputOptions::default(),
        )
        .expect("write target EPG");

        let app_state = create_test_app_state(config);
        let live_item = short_epg_live_item();
        let mut live = BPlusTree::new();
        live.insert(live_item.virtual_id, live_item.clone());
        app_state
            .playlists
            .cache_playlist(
                &target.name,
                PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage {
                    live,
                    vod: BPlusTree::new(),
                    series: BPlusTree::new(),
                })),
            )
            .await;
        let mut id_mapping = BPlusTree::new();
        id_mapping.insert(
            live_item.virtual_id,
            VirtualIdRecord::new(
                live_item.provider_id,
                live_item.virtual_id,
                PlaylistItemType::Live,
                0,
                UUIDType::default(),
            ),
        );
        app_state.playlists.cache_id_mapping(&target.name, id_mapping).await;

        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::all();
        let response = xtream_get_short_epg(&app_state, &user, &target, "100", 4)
            .await
            .into_response();
        let body = response_body_text(response).await.expect("read short EPG response");
        let json: serde_json::Value = serde_json::from_str(&body).expect("parse short EPG JSON");
        let decode_listing_text = |field: &str| {
            let encoded = json["epg_listings"][0][field].as_str().expect("encoded listing text");
            String::from_utf8(BASE64_STANDARD.decode(encoded).expect("decode listing text"))
                .expect("decoded listing text is UTF-8")
        };

        assert_eq!(json["epg_listings"][0]["channel_id"], "f1.calendar");
        assert_eq!(decode_listing_text("title"), "Formula 1 Qualifying");
        assert_eq!(decode_listing_text("description"), "Imported from ICS");
    }

    fn m3u_catchup_item(
        name: &str,
        input_name: &str,
        url: &str,
        catchup_source: Option<&str>,
    ) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 100,
            provider_id: 0,
            name: name.intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Live".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: url.intern(),
            epg_channel_id: Some("f1.calendar".intern()),
            xtream_cluster: XtreamCluster::Live,
            item_type: PlaylistItemType::Live,
            category_id: 1,
            input_name: input_name.intern(),
            channel_no: 1,
            source_ordinal: 0,
            input_stream_id: "100".intern(),
            additional_properties: Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
                name: name.intern(),
                stream_id: 100,
                tv_archive: Some(1),
                tv_archive_duration: Some(7),
                catchup: catchup_source.map(|src| shared::model::CatchupProperties {
                    mode: Some("flussonic".intern()),
                    source: Some(src.intern()),
                    ..shared::model::CatchupProperties::default()
                }),
                ..shared::model::LiveStreamProperties::default()
            }))),
            source_user_agent: None,
        }
    }

    async fn cache_xtream_test_item(
        app_state: &Arc<AppState>,
        target: &Arc<ConfigTarget>,
        item: XtreamPlaylistItem,
        input: Option<ConfigInput>,
    ) {
        if let Some(input) = input {
            let sources = SourcesConfig {
                inputs: vec![Arc::new(input)],
                sources: vec![crate::model::ConfigSource {
                    inputs: vec![item.input_name.clone()],
                    targets: vec![target.clone()],
                }],
                ..SourcesConfig::default()
            };
            app_state.app_config.sources.store(Arc::new(sources));
        }
        let mut live = BPlusTree::new();
        live.insert(item.virtual_id, item.clone());
        app_state
            .playlists
            .cache_playlist(
                &target.name,
                PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage {
                    live,
                    vod: BPlusTree::new(),
                    series: BPlusTree::new(),
                })),
            )
            .await;
        let mut id_mapping = BPlusTree::new();
        id_mapping.insert(
            item.virtual_id,
            VirtualIdRecord::new(item.provider_id, item.virtual_id, PlaylistItemType::Live, 0, UUIDType::default()),
        );
        app_state.playlists.cache_id_mapping(&target.name, id_mapping).await;
    }

    async fn build_short_epg_app_state(
        dir: &tempfile::TempDir,
        item: XtreamPlaylistItem,
        input: Option<ConfigInput>,
    ) -> (Arc<AppState>, Arc<ConfigTarget>) {
        let ics_path = dir.path().join("calendar.ics");
        let start = chrono::Utc::now() + chrono::Duration::days(1);
        let stop = start + chrono::Duration::hours(1);
        std::fs::write(
            &ics_path,
            format!(
                concat!(
                    "BEGIN:VCALENDAR\r\n",
                    "VERSION:2.0\r\n",
                    "BEGIN:VEVENT\r\n",
                    "UID:f1-qualifying\r\n",
                    "DTSTART:{}\r\n",
                    "DTEND:{}\r\n",
                    "SUMMARY:Formula 1 Qualifying\r\n",
                    "DESCRIPTION:Imported from ICS\r\n",
                    "END:VEVENT\r\n",
                    "END:VCALENDAR\r\n",
                ),
                start.format("%Y%m%dT%H%M%SZ"),
                stop.format("%Y%m%dT%H%M%SZ"),
            ),
        )
        .expect("write ICS fixture");
        let channel = parse_ics_file_to_channel(
            &ics_path,
            "f1.calendar".intern(),
            Some("Formula 1".intern()),
            &IcsEpgSourceConfig::default(),
        )
        .await
        .expect("parse ICS fixture");

        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let target = Arc::new(short_epg_target());
        let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
        std::fs::create_dir_all(&xtream_storage).expect("create xtream storage");
        let epg_path = xtream_get_epg_file_path_for_target(&xtream_storage);
        epg_write_file(
            &target.name,
            &Epg {
                priority: 0,
                logo_override: false,
                attributes: None,
                children: vec![Arc::new(channel)],
            },
            &epg_path,
            &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
            &shared::model::EpgOutputOptions::default(),
        )
        .expect("write target EPG");

        let app_state = create_test_app_state(config);
        cache_xtream_test_item(&app_state, &target, item, input).await;

        (app_state, target)
    }

    #[tokio::test]
    async fn xtream_short_epg_emits_has_archive_for_m3u_with_bridge_template() {
        let dir = tempdir().expect("temp dir");
        let input = m3u_timeshift_input();
        let item = m3u_catchup_item(
            "Formula 1",
            &input.name,
            "channel/index.m3u8",
            Some("http://provider.example/channel/video-{utc}-{duration}.m3u8"),
        );
        let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::all();

        let response = xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response();
        let body = response_body_text(response).await.expect("body");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");

        assert_eq!(json["epg_listings"][0]["has_archive"], 1);
    }

    #[tokio::test]
    async fn xtream_short_epg_omits_has_archive_for_m3u_with_unsupported_catchup() {
        let dir = tempdir().expect("temp dir");
        let input = m3u_timeshift_input();
        let item = m3u_catchup_item(
            "Formula 1",
            &input.name,
            "channel/index.m3u8",
            Some("http://provider.example/channel/${timestamp}.m3u8"),
        );
        let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::all();

        let body = response_body_text(xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response())
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");

        assert!(json["epg_listings"][0].get("has_archive").is_none());
    }

    #[tokio::test]
    async fn xtream_short_epg_emits_has_archive_for_native_xtream_input() {
        let dir = tempdir().expect("temp dir");
        let mut input = m3u_timeshift_input();
        input.input_type = InputType::Xtream;
        let item = m3u_catchup_item("Formula 1", &input.name, "channel/index.m3u8", None);
        let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::all();

        let body = response_body_text(xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response())
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_str(&body).expect("json");

        assert_eq!(json["epg_listings"][0]["has_archive"], 1);
    }

    #[tokio::test]
    async fn empty_stream_info_response_uses_vod_object_and_list_shapes() -> Result<(), String> {
        assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Video)).await?, "{}");
        assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Live)).await?, "[]");
        assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Series)).await?, "[]");
        Ok(())
    }

    fn create_test_vod_item(url: &str, container_extension: &str, item_type: PlaylistItemType) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 176_141,
            provider_id: 813_563,
            name: "Test".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: Arc::<str>::from(url),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Video,
            additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                name: "Test".intern(),
                category_id: 0,
                stream_id: 813_563,
                stream_icon: "".intern(),
                direct_source: "".intern(),
                custom_sid: None,
                added: "".intern(),
                container_extension: container_extension.intern(),
                rating: None,
                rating_5based: None,
                stream_type: Some("movie".intern()),
                trailer: None,
                tmdb: None,
                is_adult: 0,
                details: None,
            }))),
            item_type,
            category_id: 0,
            input_name: "strong".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "813563".intern(),
            source_user_agent: None,
        }
    }

    #[test]
    fn post_query_only_request_prefers_query_when_form_is_missing() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            action: String::from("get_live_streams"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.password, "query-pass");
        assert_eq!(api_req.action, "get_live_streams");
    }

    #[test]
    fn post_request_prefers_query_over_form() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            action: String::from("query-action"),
            ..UserApiRequest::default()
        };
        let form_req = UserApiRequest {
            username: String::from("form-user"),
            action: String::from("form-action"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.action, "query-action");
    }

    #[test]
    fn timeshift_query_request_prefers_query_when_form_is_missing() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            stream: String::from("42"),
            duration: String::from("60"),
            start: String::from("2024-01-01:00-00"),
            ..UserApiRequest::default()
        };
        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.password, "query-pass");
        assert_eq!(api_req.stream, "42");
        assert_eq!(api_req.duration, "60");
        assert_eq!(api_req.start, "2024-01-01:00-00");
    }

    #[test]
    fn timeshift_query_request_prefers_query_over_form() {
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            stream: String::from("42"),
            duration: String::from("60"),
            start: String::from("2024-01-01:00-00"),
            ..UserApiRequest::default()
        };
        let form_req = UserApiRequest {
            username: String::from("form-user"),
            stream: String::from("99"),
            duration: String::from("10"),
            start: String::from("form-start"),
            ..UserApiRequest::default()
        };

        let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.stream, "42");
        assert_eq!(api_req.duration, "60");
        assert_eq!(api_req.start, "2024-01-01:00-00");
    }

    #[test]
    fn timeshift_path_request_prefers_query_when_form_is_missing() {
        let timeshift_request = XtreamApiTimeShiftRequest {
            username: String::new(),
            password: String::new(),
            duration: String::new(),
            start: String::new(),
            stream_id: String::new(),
        };
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            stream_id: String::from("42"),
            duration: String::from("60"),
            start: String::from("2024-01-01:00-00"),
            ..UserApiRequest::default()
        };
        let query_req = UserApiRequest::merge_query_over_form(&api_query_req, None);
        let path_req = UserApiRequest {
            username: timeshift_request.username,
            password: timeshift_request.password,
            duration: timeshift_request.duration,
            start: timeshift_request.start,
            stream_id: timeshift_request.stream_id,
            ..UserApiRequest::default()
        };
        let api_req = UserApiRequest::merge_prefer_primary(&path_req, &query_req);

        assert_eq!(api_req.username, "query-user");
        assert_eq!(api_req.password, "query-pass");
        assert_eq!(api_req.stream_id, "42");
        assert_eq!(api_req.duration, "60");
        assert_eq!(api_req.start, "2024-01-01:00-00");
    }

    #[test]
    fn timeshift_path_request_prefers_path_over_query_and_form() {
        let timeshift_request = XtreamApiTimeShiftRequest {
            username: String::from("path-user"),
            password: String::from("path-pass"),
            duration: String::from("120"),
            start: String::from("path-start"),
            stream_id: String::from("7"),
        };
        let api_query_req = UserApiRequest {
            username: String::from("query-user"),
            password: String::from("query-pass"),
            stream_id: String::from("42"),
            duration: String::from("60"),
            start: String::from("query-start"),
            ..UserApiRequest::default()
        };
        let form_req = UserApiRequest {
            username: String::from("form-user"),
            password: String::from("form-pass"),
            stream_id: String::from("99"),
            duration: String::from("10"),
            start: String::from("form-start"),
            ..UserApiRequest::default()
        };

        let query_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));
        let path_req = UserApiRequest {
            username: timeshift_request.username,
            password: timeshift_request.password,
            duration: timeshift_request.duration,
            start: timeshift_request.start,
            stream_id: timeshift_request.stream_id,
            ..UserApiRequest::default()
        };
        let api_req = UserApiRequest::merge_prefer_primary(&path_req, &query_req);

        assert_eq!(api_req.username, "path-user");
        assert_eq!(api_req.password, "path-pass");
        assert_eq!(api_req.stream_id, "7");
        assert_eq!(api_req.duration, "120");
        assert_eq!(api_req.start, "path-start");
    }

    #[test]
    fn non_live_playback_extension_prefers_canonical_item_extension_over_client_override() {
        let pli = create_test_vod_item("provider://strong/movie/user/pass/813563.mp4", "mp4", PlaylistItemType::Video);

        assert_eq!(resolve_xtream_playback_extension(Some(".mkv"), &pli).as_deref(), Some(".mp4"));
    }

    #[test]
    fn media_server_xtream_playback_uses_internal_stream_ref_even_with_direct_pms_url_credentials() {
        let input = ConfigInput {
            input_type: InputType::Plex,
            url: "http://pms-user:pms-pass@pms.example.invalid:32400".to_string(),
            ..ConfigInput::default()
        };
        let fallback =
            Arc::<str>::from("media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv");

        let resolved = get_xtream_player_api_stream_url(&input, ApiStreamContext::Movie, "813563.mkv", &fallback)
            .expect("media-server fallback should be preserved");

        assert_eq!(resolved, fallback);
    }

    #[test]
    fn live_playback_extension_keeps_client_requested_extension() {
        let mut pli = create_test_vod_item("provider://strong/live/user/pass/813563.ts", "ts", PlaylistItemType::Live);
        pli.xtream_cluster = XtreamCluster::Live;

        assert_eq!(resolve_xtream_playback_extension(Some(".m3u8"), &pli).as_deref(), Some(".m3u8"));
    }

    #[test]
    fn catchup_playback_extension_preserves_requested_adaptive_extension_for_underlying_live_item() {
        let mut pli = create_test_vod_item("provider://strong/live/user/pass/813563.ts", "ts", PlaylistItemType::Live);
        pli.xtream_cluster = XtreamCluster::Live;

        assert_eq!(resolve_xtream_playback_extension(Some(".m3u8"), &pli).as_deref(), Some(".m3u8"));
    }

    fn m3u_timeshift_input() -> ConfigInput {
        ConfigInput {
            id: 0,
            name: "m3u-flussonic".intern(),
            input_type: InputType::M3u,
            url: "http://provider.example".to_string(),
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            ..ConfigInput::default()
        }
    }

    fn m3u_timeshift_item() -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: 100,
            provider_id: 0,
            name: "Live TV".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Live".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "channel/index.m3u8".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
                name: "Live TV".intern(),
                stream_id: 100,
                tv_archive: Some(1),
                tv_archive_duration: Some(7),
                catchup: Some(shared::model::CatchupProperties {
                    mode: Some("flussonic".intern()),
                    source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
                    ..shared::model::CatchupProperties::default()
                }),
                ..shared::model::LiveStreamProperties::default()
            }))),
            item_type: PlaylistItemType::Live,
            category_id: 1,
            input_name: "m3u-flussonic".intern(),
            channel_no: 1,
            source_ordinal: 0,
            input_stream_id: "100".intern(),
            source_user_agent: None,
        }
    }

    #[test]
    fn m3u_timeshift_resolver_returns_resolved_url_for_flussonic_template() {
        let input = m3u_timeshift_input();
        let item = m3u_timeshift_item();

        let resolved = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00")
            .expect("resolver ok")
            .expect("M3U input handled by bridge");

        assert_eq!(
            resolved.url,
            "http://provider.example/channel/video-1704067200-3600.m3u8"
        );
        assert!(!resolved.discriminator.is_empty());
    }

    #[test]
    fn m3u_timeshift_resolver_distinguishes_time_windows() {
        let input = m3u_timeshift_input();
        let item = m3u_timeshift_item();

        let first =
            resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00").expect("first ok").expect("some");
        let second =
            resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:01-00").expect("second ok").expect("some");

        assert_ne!(first.url, second.url);
        assert_ne!(first.discriminator, second.discriminator);
        let fp = Fingerprint::new(
            "fp".to_string(),
            "127.0.0.1".to_string(),
            "127.0.0.1:0".parse().unwrap(),
        );
        assert_ne!(
            crate::api::api_utils::create_m3u_catchup_session_key(&fp, "alice", 100, &first.discriminator),
            crate::api::api_utils::create_m3u_catchup_session_key(&fp, "alice", 100, &second.discriminator),
        );
    }

    #[test]
    fn m3u_timeshift_resolver_rejects_missing_catchup_metadata() {
        let mut item = m3u_timeshift_item();
        if let Some(StreamProperties::Live(live)) = item.additional_properties.as_mut() {
            live.catchup = None;
        }
        let input = m3u_timeshift_input();

        let err = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00")
            .expect_err("missing catchup must error");
        assert!(matches!(err, TuliproxError::ApiXtream(_)));
    }

    #[test]
    fn m3u_timeshift_resolver_returns_none_for_non_m3u_input() {
        let mut input = m3u_timeshift_input();
        input.input_type = InputType::Xtream;
        let item = m3u_timeshift_item();

        let resolved = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00")
            .expect("non-M3U inputs are skipped, not errored");
        assert!(resolved.is_none(), "non-M3U input must be skipped by helper");
    }

    #[test]
    fn m3u_timeshift_stream_url_uses_resolved_archive_for_credential_bearing_input() {
        let input = m3u_timeshift_input();
        let resolved_url: Arc<str> =
            Arc::from("http://provider.example/channel/video-1704067200-3600.m3u8");

        let url =
            get_xtream_player_api_stream_url(&input, ApiStreamContext::Timeshift, "60/2024-01-01:00-00", &resolved_url)
                .expect("resolved archive URL must win for M3U timeshift");

        assert_eq!(url, resolved_url);
        assert!(!url.contains("alice"));
        assert!(!url.contains("secret"));
    }

    #[tokio::test]
    async fn expired_user_is_rejected_before_m3u_timeshift_is_resolved() -> Result<(), String> {
        let dir = tempdir().map_err(|err| err.to_string())?;
        let input = m3u_timeshift_input();
        let item = m3u_timeshift_item();
        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            user_access_control: true,
            ..Config::default()
        };
        let app_state = create_test_app_state(config);
        let target = Arc::new(short_epg_target());
        cache_xtream_test_item(&app_state, &target, item, Some(input)).await;

        let mut user = ProxyUserCredentials::default();
        user.username = "expired".to_string();
        user.status = Some(ProxyUserStatus::Expired);
        user.output_clusters = ClusterFlags::all();
        let user = Arc::new(user);
        let addr = "127.0.0.1:0".parse().map_err(|err: std::net::AddrParseError| err.to_string())?;
        let fingerprint = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), addr);

        let response = xtream_player_api_stream(
            &fingerprint,
            &HeaderMap::new(),
            &app_state,
            &UserApiRequest::default(),
            ApiStreamRequest::from(ApiStreamContext::Timeshift, "expired", "", "100.ts", "invalid"),
            Some((user, target)),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        Ok(())
    }

    #[tokio::test]
    async fn xtream_token_stream_rejects_m3u_timeshift_with_bad_request() {
        use crate::auth::create_access_token;

        let dir = tempdir().expect("temp dir");
        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            ..Config::default()
        };
        let app_state = create_test_app_state(config);
        let target = Arc::new(short_epg_target());
        let input = m3u_timeshift_input();
        let item = m3u_timeshift_item();
        cache_xtream_test_item(&app_state, &target, item, Some(input)).await;

        let token = create_access_token(&app_state.app_config.access_token_secret, 60);

        let response = xtream_player_api_stream_with_token(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:0".parse().unwrap()),
            &HeaderMap::new(),
            &app_state,
            target.id,
            ApiStreamRequest::from_access_token(
                ApiStreamContext::Timeshift,
                &token,
                "100.ts",
                "",
            ),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
