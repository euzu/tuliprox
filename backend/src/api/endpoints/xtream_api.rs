// https://github.com/tellytv/go.xtream-codes/blob/master/structs.go
// Xtream api -> https://9tzx6f0ozj.apidog.io/
use crate::{
    api::{
        api_utils,
        api_utils::{
            create_api_proxy_user, create_catchup_session_key, create_session_fingerprint, empty_json_response_as_array,
            empty_json_response_as_object, force_provider_stream_response, get_session_reservation_ttl_secs,
            get_user_target, get_user_target_by_credentials, internal_server_error, is_seek_request,
            admission_failure_response, is_stream_share_enabled, local_stream_response, redirect, redirect_response, resource_response,
            separate_number_and_remainder, should_allow_exhausted_shared_reconnect, stream_response,
            try_option_bad_request, try_option_forbidden, try_result_bad_request, try_result_not_found,
            try_unwrap_body, RedirectParams,
        },
        endpoints::{
            hls_api::handle_hls_stream_request,
            xmltv_api::{get_empty_epg_response, get_epg_path_for_target, serve_short_epg},
        },
        model::{
            create_custom_video_stream_response, AppState, CustomVideoStreamType, UserApiRequestQueryOrBody, UserApiRequest,
            XtreamAuthorizationResponse,
        },
    },
    auth::Fingerprint,
    model::{
        xtream_mapping_option_from_target_options, Config, ConfigInput, ConfigInputFlags, ConfigTarget, InputSource,
        ProxyUserCredentials,
    },
    repository::{
        get_target_id_mapping, get_target_storage_path, storage_const, user_get_bouquet_filter,
        xtream_get_collection_path, xtream_get_item_for_stream_id, xtream_load_rewrite_playlist, VirtualIdRecord,
    },
    utils::{
        apply_timeshift, debug_if_enabled, file_exists_async, parse_timeshift, request, trace_if_enabled, xtream,
        xtream::create_vod_info_from_item,
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
    error::{info_err, info_err_res, TuliproxError},
    model::{
        create_stream_channel_with_type, PlaylistEntry, PlaylistItemType, ProxyType, ShortEpgResultDto, TargetType,
        UserConnectionPermission, XtreamCluster, XtreamPlaylistItem,
    },
    utils::{
        deserialize_as_string, extract_extension_from_url, generate_provider_playlist_uuid, sanitize_sensitive_info, trim_slash,
        Internable, HLS_EXT,
    },
};
use std::{
    fmt::{Display, Formatter, Write},
    str::FromStr,
    sync::Arc,
};

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
            _ => info_err_res!("Unknown ApiStreamContext: {}", s),
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
        Self { context, access_token: false, username: "", password, stream_id, action_path }
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

async fn get_user_info(user: &ProxyUserCredentials, app_state: &AppState) -> XtreamAuthorizationResponse {
    let server_info = app_state.app_config.get_user_server_info(user);
    let active_connections = app_state.get_active_connections_for_user(&user.username).await;

    XtreamAuthorizationResponse::new(
        &server_info,
        user,
        active_connections,
        app_state.app_config.config.load().user_access_control,
    )
}

#[allow(clippy::too_many_lines)]
async fn xtream_player_api_stream(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    stream_req: ApiStreamRequest<'_>,
    user_target: Option<(ProxyUserCredentials, Arc<ConfigTarget>)>,
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
        None => try_option_forbidden!(
            get_user_target_by_credentials(stream_req.username, stream_req.password, api_req, app_state),
            auth_status,
            false,
            format!("Could not find any user for xc stream {}", stream_req.username)
        ),
        Some((user, target)) => (user, target),
    };

    let _guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    let target_name = &target.name;
    if !target.has_output(TargetType::Xtream) {
        debug!("Target has no xtream codes playlist {target_name}");
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let pli = try_result_not_found!(
        xtream_get_item_for_stream_id(req_virtual_id, app_state, &target, None).await,
        true,
        format!("Failed to read xtream item for stream id {req_virtual_id}")
    );
    let virtual_id = pli.virtual_id;
    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ext = stream_ext.filter(|s| !s.is_empty())
        .or_else(|| pli.get_container_extension().map(|e| concat_string!(".", e.as_ref())));

    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!(
            "Can't find input {} for target {target_name}, context {}, stream_id {virtual_id}",
            pli.input_name, stream_req.context
        )
    );

    if user.permission_denied(app_state) {
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            create_stream_channel_with_type(target.id, &pli, pli.item_type),
            pli.input_name.as_ref(),
            req_headers,
            crate::repository::ConnectFailureReason::UserAccountExpired,
        );
    }

    if pli.item_type.is_local() {
        let connection_permission = user.connection_permission(app_state).await;
        let playback_session_token = create_session_fingerprint(fingerprint, &user.username, virtual_id);
        return local_stream_response(
            fingerprint,
            app_state,
            pli.to_stream_channel(target.id),
            req_headers,
            &input,
            &target,
            &user,
            connection_permission,
            Some(playback_session_token.as_str()),
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

    debug_if_enabled!(
        "ID chain for xtream endpoint: request_stream_id={} -> action_stream_id={action_stream_id} -> req_virtual_id={req_virtual_id} -> virtual_id={virtual_id}",
        stream_req.stream_id);
    let session_key = if item_type == PlaylistItemType::Catchup {
        create_catchup_session_key(fingerprint, &user.username, virtual_id)
    } else {
        create_session_fingerprint(fingerprint, &user.username, virtual_id)
    };
    let user_session = app_state.active_users.get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                create_stream_channel_with_type(target.id, &pli, item_type),
                &session.provider,
                req_headers,
                crate::repository::ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            return admission_failure_response(
                app_state,
                fingerprint,
                &user,
                create_stream_channel_with_type(target.id, &pli, item_type),
                &session.provider,
                req_headers,
                crate::repository::ConnectFailureReason::ProviderConnectionsExhausted,
            );
        }

        let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);

        if session.virtual_id == virtual_id && is_seek_request(cluster, req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                stream_channel,
                crate::api::api_utils::ForceStreamRequestContext {
                    req_headers,
                    input: &input,
                    user: &user,
                    session_reservation_ttl_secs: get_session_reservation_ttl_secs(app_state, item_type),
                },
            )
            .await
            .into_response();
        }

        session.stream_url.clone()
    } else {
        pli.url.clone()
    };

    let connection_permission = user.connection_permission(app_state).await;
    let allow_exhausted_shared_reconnect = should_allow_exhausted_shared_reconnect(
        is_stream_share_enabled(item_type, &target),
        user_session.as_ref(),
        virtual_id,
        session_url.as_ref(),
    );
    if connection_permission == UserConnectionPermission::Exhausted && !allow_exhausted_shared_reconnect {
        return admission_failure_response(
            app_state,
            fingerprint,
            &user,
            create_stream_channel_with_type(target.id, &pli, item_type),
            input.name.as_ref(),
            req_headers,
            crate::repository::ConnectFailureReason::UserConnectionsExhausted,
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
        stream_ext: stream_ext.as_deref(),
        req_context: context,
        action_path: stream_req.action_path,
    };
    if let Some(response) = redirect_response(app_state, &redirect_params).await {
        return response.into_response();
    }

    let (query_path, extension) = get_query_path(stream_req.action_path, stream_ext.as_ref(), &pli, app_state);

    let stream_url = try_option_bad_request!(
        get_xtream_player_api_stream_url(&input, stream_req.context, &query_path, &session_url),
        true,
        format!(
            "Can't find stream url for target {target_name}, context {}, stream_id {virtual_id}",
            stream_req.context
        )
    );

    let is_hls_request =
        item_type == PlaylistItemType::LiveHls || item_type == PlaylistItemType::LiveDash || extension == HLS_EXT;
    // Reverse proxy mode
    if is_hls_request {
        return handle_hls_stream_request(
            fingerprint,
            app_state,
            &user,
            user_session.as_ref(),
            &stream_url,
            pli.virtual_id,
            &input,
            req_headers,
            connection_permission,
        )
        .await
        .into_response();
    }

    let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);

    stream_response(
        fingerprint,
        app_state,
        session_key.as_str(),
        stream_channel,
        &stream_url,
        req_headers,
        &input,
        &target,
        &user,
        connection_permission,
        allow_exhausted_shared_reconnect,
    )
    .await
    .into_response()
}

fn get_query_path(
    action_path: &str,
    stream_ext: Option<&String>,
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
        extract_extension_from_url(&pli.url).unwrap_or_default()
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

#[allow(clippy::too_many_lines)]
// Used by webui
async fn xtream_player_api_stream_with_token(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    target_id: u16,
    stream_req: ApiStreamRequest<'_>,
) -> impl IntoResponse + Send {
    if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
        let target_name = &target.name;
        if !target.has_output(TargetType::Xtream) {
            debug!("Target has no xtream output {target_name}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
        let req_virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
        let pli = try_result_bad_request!(
            xtream_get_item_for_stream_id(req_virtual_id, app_state, &target, None).await,
            true,
            format!("Failed to read xtream item for stream id {req_virtual_id}")
        );
        let virtual_id = pli.virtual_id;
        let input = try_option_bad_request!(
            app_state.app_config.get_input_by_name(&pli.input_name),
            true,
            format!(
                "Can't find input {} for target {target_name}, context {}, stream_id {}",
                pli.input_name, stream_req.context, pli.virtual_id
            )
        );

        let user = create_api_proxy_user(app_state);

        if pli.item_type.is_local() {
            let playback_session_token = create_session_fingerprint(fingerprint, "webui", virtual_id);
            return local_stream_response(
                fingerprint,
                app_state,
                pli.to_stream_channel(target.id),
                req_headers,
                &input,
                &target,
                &user,
                UserConnectionPermission::Allowed,
                Some(playback_session_token.as_str()),
                true,
            )
            .await
            .into_response();
        }

        let stream_ext = stream_ext.filter(|s| !s.is_empty())
            .or_else(|| pli.get_container_extension().map(|e| concat_string!(".", e.as_ref())));

        let session_key = create_session_fingerprint(fingerprint, "webui", virtual_id);

        let is_hls_request = pli.item_type == PlaylistItemType::LiveHls || stream_ext.as_deref() == Some(HLS_EXT);

        // TODO how should we use fixed provider for hls in multi provider config?

        // Reverse proxy mode
        if is_hls_request {
            return handle_hls_stream_request(
                fingerprint,
                app_state,
                &user,
                None,
                &pli.url,
                virtual_id,
                &input,
                req_headers,
                UserConnectionPermission::Allowed,
            )
            .await
            .into_response();
        }

        let (query_path, _extension) = get_query_path(stream_req.action_path, stream_ext.as_ref(), &pli, app_state);

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
            pli.to_stream_channel(target.id),
            &stream_url,
            req_headers,
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            false,
        )
        .await
        .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

async fn xtream_player_api_resource(
    req_headers: &HeaderMap,
    api_req: &UserApiRequest,
    app_state: &Arc<AppState>,
    resource_req: ApiStreamRequest<'_>,
) -> impl IntoResponse {
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target_by_credentials(resource_req.username, resource_req.password, api_req, app_state),
        auth_status,
        false,
        format!("Could not find any user xc resource {}", resource_req.username)
    );
    if user.permission_denied(app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
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

    let stream_url = pli.resolve_resource_url(resource);

    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) {
                trace_if_enabled!("Redirecting resource request to {}", sanitize_sensitive_info(&url));
                redirect(&url).into_response()
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
    let (user, target) = try_option_forbidden!(
        get_user_target_by_credentials(&api_req.username, &api_req.password, &api_req, &app_state),
        auth_status,
        false,
        format!("Could not find any user {}", api_req.username)
    );

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
    let (user, target) = try_option_forbidden!(
        get_user_target_by_credentials(&api_req.username, &api_req.password, &api_req, &app_state),
        auth_status,
        false,
        format!("Could not find any user {}", api_req.username)
    );

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

pub async fn xtream_get_stream_info_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    cluster: XtreamCluster,
) -> impl IntoResponse + Send {
    let virtual_id: u32 = match FromStr::from_str(stream_id) {
        Ok(id) => id,
        Err(_) => return try_unwrap_body!(empty_json_response_as_array()),
    };

    if let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, Some(cluster)).await {
        if pli.item_type.is_local() {
            let Ok(xtream_output) = target
                .get_xtream_output()
                .ok_or_else(|| info_err!("Unexpected: xtream output required for target {}", target.name))
            else {
                return try_unwrap_body!(empty_json_response_as_array());
            };

            let encrypt_secret = app_state.get_encrypt_secret();

            let options = xtream_mapping_option_from_target_options(
                target,
                xtream_output,
                &app_state.app_config,
                user,
                encrypt_secret,
            );
            return axum::Json(pli.to_info_document(&options)).into_response();
        }

        if pli.provider_id > 0 {
            let input_name = &pli.input_name;
            if let Some(input) = app_state.app_config.get_input_by_name(input_name) {
                if let Some(info_url) = xtream::get_xtream_player_api_info_url(&input, cluster, pli.provider_id) {
                    // Redirect is only possible for live streams, vod and series info needs to be modified
                    if user.proxy == ProxyType::Redirect && cluster == XtreamCluster::Live {
                        return redirect(&info_url).into_response();
                    } else if let Ok(content) = xtream::get_xtream_stream_info(
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

        return match cluster {
            XtreamCluster::Video => {
                let content = create_vod_info_from_item(target, user, &pli);
                try_unwrap_body!(axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                    .body(axum::body::Body::from(content)))
            }
            XtreamCluster::Live | XtreamCluster::Series => {
                try_unwrap_body!(empty_json_response_as_array())
            }
        };
    }
    try_unwrap_body!(empty_json_response_as_object())
}

async fn xtream_get_short_epg(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    limit: u32,
) -> impl IntoResponse + Send {
    let target_name = &target.name;
    if target.has_output(TargetType::Xtream) {
        let virtual_id: u32 = match FromStr::from_str(stream_id.trim()) {
            Ok(id) => id,
            Err(_) => return get_empty_epg_response().into_response(),
        };

        if let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, None).await {
            let config = &app_state.app_config.config.load();
            if let (Some(epg_path), Some(channel_id)) = (get_epg_path_for_target(config, target), &pli.epg_channel_id) {
                if file_exists_async(&epg_path).await {
                    return serve_short_epg(
                        app_state,
                        epg_path.as_path(),
                        user,
                        target,
                        channel_id,
                        stream_id.intern(),
                        limit,
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
                            return redirect(&info_url).into_response();
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
                                error!("Failed to download epg {}", sanitize_sensitive_info(err.to_string().as_str()));
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
async fn xtream_player_api(api_req: UserApiRequest, app_state: &Arc<AppState>) -> impl IntoResponse + Send {
    api_req.log_sanitized("xtream_player_api");
    let auth_status = app_state.app_config.get_auth_error_status();
    let (user, target) = try_option_forbidden!(
        get_user_target(&api_req, app_state),
        auth_status,
        false,
        format!("Could not find any user for xc player api {}", api_req.username)
    );
    if !target.has_output(TargetType::Xtream) {
            return axum::response::Json(get_user_info(&user, app_state).await).into_response();
        }

        let action = api_req.action.trim();
        if action.is_empty() {
            return axum::response::Json(get_user_info(&user, app_state).await).into_response();
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
                        live && i.has_flag(ConfigInputFlags::XtreamSkipLive),
                        vod && i.has_flag(ConfigInputFlags::XtreamSkipVod),
                        series && i.has_flag(ConfigInputFlags::XtreamSkipSeries),
                    )
                })
            } else {
                (false, false, false)
            };

        match action {
            crate::model::XC_ACTION_GET_ACCOUNT_INFO => {
                return axum::response::Json(get_user_info(&user, app_state).await).into_response();
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
                    xtream_get_stream_info_response(
                        app_state,
                        &user,
                        &target,
                        api_req.vod_id.trim(),
                        XtreamCluster::Video
                    )
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
                xtream_load_rewrite_playlist(XtreamCluster::Live, app_state, &target, category_id, &user)
                    .await
            ),
            crate::model::XC_ACTION_GET_VOD_STREAMS => skip_flag_optional!(
                skip_vod,
                xtream_load_rewrite_playlist(XtreamCluster::Video, app_state, &target, category_id, &user)
                    .await
            ),
            crate::model::XC_ACTION_GET_SERIES => skip_flag_optional!(
                skip_series,
                xtream_load_rewrite_playlist(XtreamCluster::Series, app_state, &target, category_id, &user)
                    .await
            ),
            _ => Some(info_err_res!("Unknown api call: {action} for target: {}", &target.name)),
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
                        error!("Failed response for xtream target: {} action: {} error: {}", &target.name, action, err);
                        axum::response::Json(get_user_info(&user, app_state).await).into_response()
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
    S: Stream<Item = (String, bool)> + Send + Unpin + 'static,
{
    let mapped = xtream_iter.map(move |(mut line, has_next)| {
        if has_next {
            line.push(',');
        }
        Ok::<Bytes, String>(Bytes::from(line))
    });
    stream::once(async { Ok::<Bytes, String>(Bytes::from("[")) })
        .chain(mapped)
        .chain(stream::once(async { Ok::<Bytes, String>(Bytes::from("]")) }))
}

async fn xtream_player_api_get(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
) -> impl IntoResponse + Send {
    xtream_player_api(api_req, &app_state).await
}

async fn xtream_player_api_post(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    UserApiRequestQueryOrBody(api_req): UserApiRequestQueryOrBody,
) -> impl IntoResponse + Send {
    xtream_player_api(api_req, &app_state).await
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
    use crate::api::model::UserApiRequest;
    use super::XtreamApiTimeShiftRequest;

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
}
