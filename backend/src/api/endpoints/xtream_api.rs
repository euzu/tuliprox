// https://github.com/tellytv/go.xtream-codes/blob/master/structs.go
// Xtream api -> https://9tzx6f0ozj.apidog.io/
use crate::api::api_utils;
use crate::api::api_utils::{create_api_proxy_user, create_session_fingerprint, empty_json_response_as_array, empty_json_response_as_object, force_provider_stream_response, get_user_target, get_user_target_by_credentials, internal_server_error, is_seek_request, local_stream_response, redirect, redirect_response, resource_response, separate_number_and_remainder, stream_response, try_option_bad_request, try_result_bad_request, try_result_not_found, try_unwrap_body, RedirectParams};
use crate::api::endpoints::hls_api::handle_hls_stream_request;
use crate::api::endpoints::xmltv_api::{get_empty_epg_response, get_epg_path_for_target, serve_short_epg};
use crate::api::model::AppState;
use crate::api::model::UserApiRequest;
use crate::api::model::XtreamAuthorizationResponse;
use crate::api::model::{create_custom_video_stream_response, CustomVideoStreamType};
use crate::auth::Fingerprint;
use crate::model::{xtream_mapping_option_from_target_options, ConfigTarget};
use crate::model::{Config, ConfigInput};
use crate::model::{InputSource, ProxyUserCredentials};
use crate::repository::get_target_storage_path;
use crate::repository::storage_const;
use crate::repository::VirtualIdRecord;
use crate::repository::{get_target_id_mapping, user_get_bouquet_filter, xtream_get_collection_path, xtream_get_item_for_stream_id, xtream_load_rewrite_playlist};
use crate::utils::xtream::create_vod_info_from_item;
use crate::utils::{debug_if_enabled, file_exists_async, trace_if_enabled};
use crate::utils::{request, xtream};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures::Stream;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use shared::concat_string;
use shared::error::{info_err, info_err_res, TuliproxError};
use shared::model::{create_stream_channel_with_type, PlaylistEntry, PlaylistItemType, ProxyType, ShortEpgResultDto, TargetType, UserConnectionPermission, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{deserialize_as_string, extract_extension_from_url, generate_playlist_uuid, sanitize_sensitive_info, trim_slash, Internable, HLS_EXT};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;

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
        write!(f, "{}",
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
        Self {
            context,
            access_token: false,
            username,
            password,
            stream_id,
            action_path,
        }
    }
    pub const fn from_access_token(
        context: ApiStreamContext,
        password: &'a str,
        stream_id: &'a str,
        action_path: &'a str,
    ) -> Self {
        Self {
            context,
            access_token: false,
            username: "",
            password,
            stream_id,
            action_path,
        }
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
                let use_prefix = input
                    .options
                    .as_ref()
                    .is_none_or(|o| o.xtream_live_stream_use_prefix);
                String::from(if use_prefix { "live" } else { "" })
            }
            ApiStreamContext::Movie | ApiStreamContext::Series | ApiStreamContext::Timeshift => {
                context.to_string()
            }
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

    let (user, target) = try_option_bad_request!(
        get_user_target_by_credentials( stream_req.username, stream_req.password, api_req, app_state),
        false,
        format!("Could not find any user for xc stream {}", stream_req.username)
    );

    let _guard = app_state.app_config.file_locks.write_lock_str(&user.username).await;

    if user.permission_denied(app_state) {
        return create_custom_video_stream_response(app_state, &fingerprint.addr, CustomVideoStreamType::UserAccountExpired).await.into_response();
    }

    let target_name = &target.name;
    if !target.has_output(TargetType::Xtream) {
        debug!("Target has no xtream codes playlist {target_name}");
        return create_custom_video_stream_response(app_state, &fingerprint.addr, CustomVideoStreamType::ChannelUnavailable).await.into_response();
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

    let input = try_option_bad_request!(
      app_state.app_config.get_input_by_name(&pli.input_name),
      true,
      format!( "Can't find input {} for target {target_name}, context {}, stream_id {virtual_id}", pli.input_name, stream_req.context)
    );

    if pli.item_type.is_local() {
        let connection_permission = user.connection_permission(app_state).await;
        return local_stream_response(
            fingerprint,
            app_state,
            pli.to_stream_channel(target.id),
            req_headers,
            &input,
            &target,
            &user,
            connection_permission,
            true,
        ).await.into_response();
    }

    let (cluster, item_type) = if stream_req.context == ApiStreamContext::Timeshift {
        (XtreamCluster::Video, PlaylistItemType::Catchup)
    } else {
        (pli.xtream_cluster, pli.item_type)
    };

    debug_if_enabled!(
        "ID chain for xtream endpoint: request_stream_id={} -> action_stream_id={action_stream_id} -> req_virtual_id={req_virtual_id} -> virtual_id={virtual_id}",
        stream_req.stream_id);
    let session_key = create_session_fingerprint(fingerprint, &user.username, virtual_id);
    let user_session = app_state
        .active_users
        .get_and_update_user_session(&user.username, &session_key).await;

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(
                app_state, &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            ).await
                .into_response();
        }

        if app_state
            .active_provider
            .is_over_limit(&session.provider)
            .await
        {
            return create_custom_video_stream_response(
                app_state, &fingerprint.addr,
                CustomVideoStreamType::ProviderConnectionsExhausted,
            ).await
                .into_response();
        }

        let stream_channel = create_stream_channel_with_type(target.id, &pli, item_type);

        if session.virtual_id == virtual_id && is_seek_request(cluster, req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(
                fingerprint,
                app_state,
                session,
                stream_channel,
                req_headers,
                &input,
                &user,
            )
                .await
                .into_response();
        }

        session.stream_url.clone()
    } else {
        pli.url.clone()
    };

    let connection_permission = user.connection_permission(app_state).await;
    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(
            app_state, &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        ).await
            .into_response();
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

    let is_hls_request = item_type == PlaylistItemType::LiveHls
        || item_type == PlaylistItemType::LiveDash
        || extension == HLS_EXT;
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
    )
        .await
        .into_response()
}

fn get_query_path(action_path: &str, stream_ext: Option<&String>, pli: &XtreamPlaylistItem, app_state: &Arc<AppState>) -> (String, String) {
    let discard_extension = if pli.item_type.is_live() {
        app_state.app_config.sources.load().get_input_by_name(&pli.input_name)
            .as_ref().and_then(|i| i.options.as_ref())
            .is_some_and(|o| o.xtream_live_stream_without_extension)
    } else {
        false
    };

    let extracted_ext;
    let extension: &str = if discard_extension {
        ""
    } else if let Some(ext) = stream_ext {
        ext
    } else {
        extracted_ext = extract_extension_from_url(&pli.url);
        extracted_ext.unwrap_or("")
    };

    let provider_id = pli.provider_id.to_string();

    let query_path = if action_path.is_empty() {
        concat_string!(&provider_id, extension)
    } else {
        let path = trim_slash(action_path);
        concat_string!(path.as_ref(), "/", &provider_id, extension)
    };
    (query_path, extension.to_string())
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
            xtream_get_item_for_stream_id(
                req_virtual_id,
                app_state,
                &target,
                None
            ).await,
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
            return local_stream_response(
                fingerprint,
                app_state,
                pli.to_stream_channel(target.id),
                req_headers,
                &input,
                &target,
                &user,
                UserConnectionPermission::Allowed,
                true,
            ).await.into_response();
        }

        let session_key = create_session_fingerprint(fingerprint, "webui", virtual_id);

        let is_hls_request =
            pli.item_type == PlaylistItemType::LiveHls || stream_ext.as_deref() == Some(HLS_EXT);

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
            get_xtream_player_api_stream_url(
                &input,
                stream_req.context,
                &query_path,
                &pli.url
            ),
            true,
            format!(
                "Can't find stream url for target {target_name}, context {}, stream_id {}",
                stream_req.context, virtual_id
            )
        );

        trace_if_enabled!(
            "Streaming stream request from {}",
            sanitize_sensitive_info(&stream_url)
        );
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
    let (user, target) = try_option_bad_request!(
        get_user_target_by_credentials(
            resource_req.username,
            resource_req.password,
            api_req,
            app_state
        ),
        false,
        format!(
            "Could not find any user xc resource {}",
            resource_req.username
        )
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
        xtream_get_item_for_stream_id(
            req_virtual_id,
            app_state,
            &target,
            None
        ).await,
        true,
        format!("Failed to read xtream item for stream id {req_virtual_id}")
    );

    let stream_url = pli.resolve_resource_url(resource);

    match stream_url {
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
        Some(url) => {
            if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) {
                trace_if_enabled!(
                    "Redirecting resource request to {}",
                    sanitize_sensitive_info(&url)
                );
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
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(
                String,
                String,
                String,
            )>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
        ) -> impl IntoResponse + Send {
            xtream_player_api_stream(
                &fingerprint,
                &req_headers,
                &app_state,
                &api_req,
                ApiStreamRequest::from($context, &username, &password, &stream_id, ""),
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

fn get_non_empty<'a>(first: &'a str, second: &'a str, third: &'a str) -> &'a str {
    if !first.is_empty() {
        first
    } else if !second.is_empty() {
        second
    } else {
        third
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
    axum::extract::Query(mut api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path(timeshift_request): axum::extract::Path<XtreamApiTimeShiftRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_form_req): axum::extract::Form<UserApiRequest>,
) -> impl IntoResponse + Send {
    let username = get_non_empty(
        &timeshift_request.username,
        &api_form_req.username,
        &api_req.username,
    )
        .to_string();
    let password = get_non_empty(
        &timeshift_request.password,
        &api_form_req.password,
        &api_req.password,
    )
        .to_string();
    let stream_id = get_non_empty(
        &timeshift_request.stream_id,
        &api_req.stream_id,
        &api_form_req.stream_id,
    )
        .to_string();
    let duration = get_non_empty(
        &timeshift_request.duration,
        &timeshift_request.duration,
        &api_form_req.duration,
    );
    let start = get_non_empty(
        &timeshift_request.start,
        &timeshift_request.start,
        &api_form_req.start,
    );

    let action_path = format!("{duration}/{start}");
    api_req.username.clone_from(&username);
    api_req.password.clone_from(&password);
    api_req.stream_id.clone_from(&stream_id);

    xtream_player_api_stream(
        &fingerprint,
        &req_headers,
        &app_state,
        &api_req,
        ApiStreamRequest::from(
            ApiStreamContext::Timeshift,
            &username,
            &password,
            &stream_id,
            &action_path,
        ), /*&addr*/
    )
        .await
        .into_response()
}

async fn xtream_player_api_timeshift_query_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::Query(api_query_req): axum::extract::Query<UserApiRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_form_req): axum::extract::Form<UserApiRequest>,
) -> impl IntoResponse + Send {
    let username = get_non_empty(&api_query_req.username, &api_form_req.username, "");
    let password = get_non_empty(&api_query_req.password, &api_form_req.password, "");
    let stream_id = get_non_empty(&api_query_req.stream, &api_form_req.stream, "");
    let duration = get_non_empty(&api_query_req.duration, &api_form_req.duration, "");
    let start = get_non_empty(&api_query_req.start, &api_form_req.start, "");
    let action_path = format!("{duration}/{start}");
    if username.is_empty()
        || password.is_empty()
        || stream_id.is_empty()
        || duration.is_empty()
        || start.is_empty()
    {
        // if token.is_empty() {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
        // }
        // xtream_player_api_stream(&req_headers, &api_query_req, &app_state, ApiStreamRequest::from_access_token(ApiStreamContext::Timeshift, token, stream_id, &action_path)/*, &addr*/).await.into_response()
    }
    xtream_player_api_stream(
        &fingerprint,
        &req_headers,
        &app_state,
        &api_query_req,
        ApiStreamRequest::from(
            ApiStreamContext::Timeshift,
            username,
            password,
            stream_id,
            &action_path,
        ),
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

    if let Ok(pli) = xtream_get_item_for_stream_id(
        virtual_id,
        app_state,
        target,
        Some(cluster),
    ).await {
        if pli.item_type.is_local() {
            let Ok(xtream_output) = target.get_xtream_output().ok_or_else(|| info_err!("Unexpected: xtream output required for target {}", target.name)) else {
                return try_unwrap_body!(empty_json_response_as_array());
            };

            let server_info = app_state.app_config.get_user_server_info(user);
            let options = xtream_mapping_option_from_target_options(target, xtream_output, &app_state.app_config, user, Some(server_info.get_base_url().as_str()));
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
                    ).await
                    {
                        return try_unwrap_body!(axum::response::Response::builder()
                            .status(axum::http::StatusCode::OK)
                            .header(
                                axum::http::header::CONTENT_TYPE,
                                mime::APPLICATION_JSON.to_string()
                            )
                            .body(axum::body::Body::from(content)));
                    }
                }
            }
        }

        return match cluster {
            XtreamCluster::Video => {
                let content =
                    create_vod_info_from_item(target, user, &pli);
                try_unwrap_body!(axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header(
                        axum::http::header::CONTENT_TYPE,
                        mime::APPLICATION_JSON.to_string()
                    )
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

        if let Ok(pli) = xtream_get_item_for_stream_id(
            virtual_id,
            app_state,
            target,
            None,
        ).await {
            let config = &app_state.app_config.config.load();
            if let (Some(epg_path), Some(channel_id)) = (get_epg_path_for_target(config, target), &pli.epg_channel_id) {
                if file_exists_async(&epg_path).await {
                    return serve_short_epg(app_state, epg_path.as_path(), user, target, channel_id, stream_id.intern(), limit).await;
                }
            }

            if pli.provider_id > 0 {
                let input_name = &pli.input_name;
                if let Some(input) = app_state.app_config.get_input_by_name(input_name) {
                    if let Some(action_url) = xtream::get_xtream_player_api_action_url(
                        &input,
                        crate::model::XC_ACTION_GET_SHORT_EPG,
                    ) {
                        let mut info_url = format!(
                            "{action_url}&{}={}",
                            crate::model::XC_TAG_STREAM_ID,
                            pli.provider_id
                        );
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
                                [(
                                    axum::http::header::CONTENT_TYPE.to_string(),
                                    mime::APPLICATION_JSON.to_string(),
                                )],
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
                let filter = user_get_bouquet_filter(
                    config,
                    &user.username,
                    category_id,
                    TargetType::Xtream,
                    cluster,
                ).await;

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
    target: &Arc<ConfigTarget>,
    stream_id: &str,
    start: &str,
    end: &str,
) -> impl IntoResponse + Send {
    let req_virtual_id: u32 = if let Ok(id) = stream_id.parse::<u32>() {
        id
    } else {
        return axum::Json(json!(ShortEpgResultDto::default())).into_response();
    };
    let pli = try_result_bad_request!(xtream_get_item_for_stream_id(
        req_virtual_id,
        app_state,
        target,
        Some(XtreamCluster::Live)
    ).await);
    let input = try_option_bad_request!(app_state
        .app_config
        .get_input_by_name(&pli.input_name));
    let info_url = try_option_bad_request!(xtream::get_xtream_player_api_action_url(
        &input,
        crate::model::XC_ACTION_GET_CATCHUP_TABLE
    )
    .map(|action_url| format!(
        "{action_url}&{}={}&start={start}&end={end}",
        crate::model::XC_TAG_STREAM_ID,
        pli.provider_id
    )));
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
    let epg_listings = try_option_bad_request!(doc
        .get_mut(crate::model::XC_TAG_EPG_LISTINGS)
        .and_then(Value::as_array_mut));
    let config = &app_state.app_config.config.load();
    let target_path =
        try_option_bad_request!(get_target_storage_path(config, target.name.as_str()));
    let Ok((mut target_id_mapping, file_lock)) = get_target_id_mapping(&app_state.app_config, &target_path, target.use_memory_cache).await else {
        return internal_server_error!()
    };
    let mut in_memory_updates = Vec::new();
    for epg_list_item in epg_listings.iter_mut().filter_map(Value::as_object_mut) {
        // TODO epg_id
        if let Some(catchup_provider_id) = epg_list_item
            .get(crate::model::XC_TAG_ID)
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<u32>().ok())
        {
            let uuid = generate_playlist_uuid(
                &pli.get_uuid().to_string(),
                &catchup_provider_id.to_string(),
                pli.item_type,
                &pli.input_name,
            );
            let virtual_id = target_id_mapping.get_and_update_virtual_id(
                &uuid,
                catchup_provider_id,
                PlaylistItemType::Catchup,
                pli.virtual_id,
            );

            if target.use_memory_cache {
                in_memory_updates.push(
                    VirtualIdRecord::new(
                        catchup_provider_id,
                        virtual_id,
                        PlaylistItemType::Catchup,
                        pli.virtual_id,
                        uuid,
                    ),
                );
            }

            epg_list_item.insert(
                crate::model::XC_TAG_ID.to_string(),
                Value::String(virtual_id.to_string()),
            );
        }
    }
    if let Err(err) = target_id_mapping.persist() {
        error!("Failed to write catchup id mapping {err}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    drop(file_lock);

    if target.use_memory_cache && !in_memory_updates.is_empty() {
        app_state.playlists.update_target_id_mapping(target, in_memory_updates).await;
    }

    serde_json::to_string(&doc).map_or_else(
        |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
        |result| {
            try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    mime::APPLICATION_JSON.to_string()
                )
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
    api_req: UserApiRequest,
    app_state: &Arc<AppState>,
) -> impl IntoResponse + Send {
    let user_target = get_user_target(&api_req, app_state);
    if let Some((user, target)) = user_target {
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
                    let (l, v, s) = acc;
                    i.options.as_ref().map_or((false, false, false), |o| {
                        (
                            l && o.xtream_skip_live,
                            v && o.xtream_skip_vod,
                            s && o.xtream_skip_series,
                        )
                    })
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
                return xtream_get_short_epg(
                    app_state,
                    &user,
                    &target,
                    api_req.stream_id.trim(),
                    api_req.get_limit(),
                )
                    .await
                    .into_response();
            }
            crate::model::XC_ACTION_GET_CATCHUP_TABLE => {
                skip_json_response_if_flag_set!(
                    skip_live,
                    xtream_get_catchup_response(
                        app_state,
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
        ).await {
            return response.into_response();
        }

        let result = match action {
            crate::model::XC_ACTION_GET_LIVE_STREAMS => skip_flag_optional!(
                skip_live,
                xtream_load_rewrite_playlist(
                    XtreamCluster::Live,
                    &app_state.app_config,
                    &target,
                    category_id,
                    &user
                )
                .await
            ),
            crate::model::XC_ACTION_GET_VOD_STREAMS => skip_flag_optional!(
                skip_vod,
                xtream_load_rewrite_playlist(
                    XtreamCluster::Video,
                    &app_state.app_config,
                    &target,
                    category_id,
                    &user
                )
                .await
            ),
            crate::model::XC_ACTION_GET_SERIES => skip_flag_optional!(
                skip_series,
                xtream_load_rewrite_playlist(
                    XtreamCluster::Series,
                    &app_state.app_config,
                    &target,
                    category_id,
                    &user
                )
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
                            .header(
                                axum::http::header::CONTENT_TYPE,
                                mime::APPLICATION_JSON.to_string()
                            )
                            .body(axum::body::Body::from_stream(content_stream)))
                    }
                    Err(err) => {
                        error!(
                            "Failed response for xtream target: {} action: {} error: {}",
                            &target.name, action, err
                        );
                        // Some players fail on NoContent, so we return an empty array
                        api_utils::empty_json_list_response().into_response()
                    }
                }
            }
            None => {
                // Some players fail on NoContent, so we return an empty array
                api_utils::empty_json_list_response().into_response()
            }
        }
    } else {
        match (user_target.is_none(), api_req.action.is_empty()) {
            (true, _) => debug!("Can't find user!"),
            (_, true) => debug!("Parameter action is empty!"),
            _ => debug!("Bad request!"),
        }
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

fn xtream_create_content_stream(
    xtream_iter: impl Iterator<Item=(String, bool)>,
) -> impl Stream<Item=Result<Bytes, String>> {
    stream::once(async { Ok::<Bytes, String>(Bytes::from("[")) }).chain(
        stream::iter(xtream_iter.map(move |(mut line, has_next)| {
            if has_next {
                line.push(',');
            }
            Ok::<Bytes, String>(Bytes::from(line))
        })).chain(stream::once(async {
            Ok::<Bytes, String>(Bytes::from("]"))
        })),
    )
}

async fn xtream_player_api_get(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
) -> impl IntoResponse + Send {
    xtream_player_api(api_req, &app_state).await
}

async fn xtream_player_api_post(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_req): axum::extract::Form<UserApiRequest>,
) -> impl IntoResponse + Send {
    xtream_player_api(api_req, &app_state).await
}

macro_rules! register_xtream_api {
    ($router:expr, [$($path:expr),*]) => {{
        $router
       $(
          .route($path, axum::routing::get(xtream_player_api_get))
          .route($path, axum::routing::post(xtream_player_api_post))
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
          .route($path, axum::routing::get(xtream_player_api_timeshift_query_stream))
          .route($path, axum::routing::post(xtream_player_api_timeshift_query_stream))
            //$cfg.service(web::resource($path).route(web::get().to(xtream_player_api_timeshift_stream)).route(web::post().to(xtream_player_api_timeshift_stream)));
        )*
    }};
}

async fn xtream_player_token_stream(
    fingerprint: Fingerprint,
    axum::extract::Path((token, target_id, cluster, stream_id)): axum::extract::Path<(
        String,
        u16,
        String,
        String,
    )>,
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
    router = router.route(
        "/token/{token}/{target_id}/{cluster}/{stream_id}",
        axum::routing::get(xtream_player_token_stream),
    );
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
