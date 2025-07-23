// https://github.com/tellytv/go.xtream-codes/blob/master/structs.go

use crate::api::api_utils;
use crate::api::api_utils::{force_provider_stream_response, get_user_target, get_user_target_by_credentials, is_seek_request, redirect_response, resource_response, separate_number_and_remainder, serve_file, stream_response, RedirectParams};
use crate::api::api_utils::{redirect, try_option_bad_request, try_result_bad_request};
use crate::api::endpoints::hls_api::handle_hls_stream_request;
use crate::api::endpoints::xmltv_api::get_empty_epg_response;
use crate::api::model::app_state::AppState;
use crate::api::model::request::UserApiRequest;
use crate::api::model::streams::provider_stream::{create_custom_video_stream_response, CustomVideoStreamType};
use crate::api::model::xtream::XtreamAuthorizationResponse;
use crate::model::{AppConfig, ConfigTarget};
use crate::model::{Config, ConfigInput};
use crate::model::{ProxyUserCredentials};
use crate::repository::playlist_repository::get_target_id_mapping;
use crate::repository::storage::{get_target_storage_path};
use crate::repository::{storage_const, user_repository, xtream_repository};
use shared::error::create_tuliprox_error_result;
use shared::error::info_err;
use shared::error::{str_to_io_error, TuliproxError, TuliproxErrorKind};
use crate::utils::trace_if_enabled;
use crate::utils::xtream::create_vod_info_from_item;
use shared::utils::{extract_extension_from_url, generate_playlist_uuid, get_u32_from_serde_value, hex_encode, sanitize_sensitive_info, trim_slash, HLS_EXT};
use crate::utils::{request, xtream};
use crate::auth::Fingerprint;
use axum::http::{HeaderMap};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use futures::Stream;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use shared::model::{PlaylistItemType, XtreamCluster, FieldGetAccessor, PlaylistEntry, TargetType, UserConnectionPermission, ProxyType, get_backdrop_path_value, XtreamPlaylistItem};
use crate::api::api_utils::try_unwrap_body;

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
        write!(f, "{}", match self {
            Self::Live | Self::LiveAlt => Self::LIVE,
            Self::Movie => Self::MOVIE,
            Self::Series => Self::SERIES,
            Self::Timeshift => Self::TIMESHIFT,
        })
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
            _ => create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown CounterModifier: {}", s)
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
    pub const fn from(context: ApiStreamContext,
                      username: &'a str,
                      password: &'a str,
                      stream_id: &'a str,
                      action_path: &'a str) -> Self {
        Self {
            context,
            access_token: false,
            username,
            password,
            stream_id,
            action_path,
        }
    }
    pub const fn from_access_token(context: ApiStreamContext,
                                   password: &'a str,
                                   stream_id: &'a str,
                                   action_path: &'a str) -> Self {
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

pub fn serve_query(file_path: &Path, filter: &HashMap<&str, HashSet<String>>) -> impl IntoResponse + Send {
    let filtered = crate::utils::json_filter_file(file_path, filter);
    axum::Json(filtered)
}

pub(in crate::api) fn get_xtream_player_api_stream_url(
    input: &ConfigInput, context: ApiStreamContext, action_path: &str, fallback_url: &str,
) -> Option<String> {
    if let Some(input_user_info) = input.get_user_info() {
        let ctx = match context {
            ApiStreamContext::LiveAlt |
            ApiStreamContext::Live => {
                let use_prefix = input.options.as_ref().is_none_or(|o| o.xtream_live_stream_use_prefix);
                String::from(if use_prefix { "live" } else { "" })
            }
            ApiStreamContext::Movie
            | ApiStreamContext::Series
            | ApiStreamContext::Timeshift => context.to_string()
        };
        let mut parts = vec![
            trim_slash(&input_user_info.base_url),
            trim_slash(&ctx),
            trim_slash(&input_user_info.username),
            trim_slash(&input_user_info.password),
            trim_slash(action_path),
        ];
        parts.retain(|s| !s.is_empty());
        Some(parts.join("/"))

    } else if !fallback_url.is_empty() {
        Some(String::from(fallback_url))
    } else {
        None
    }
}

fn get_user_info(user: &ProxyUserCredentials, app_state: &AppState) -> XtreamAuthorizationResponse {
    let server_info = app_state.app_config.get_user_server_info(user);
    let active_connections = app_state.get_active_connections_for_user(&user.username);
    XtreamAuthorizationResponse::new(&server_info, user, active_connections, app_state.app_config.config.load().user_access_control)
}

async fn xtream_player_api_stream(
    fingerprint: &str,
    addr: &str,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    api_req: &UserApiRequest,
    stream_req: ApiStreamRequest<'_>,
) -> impl IntoResponse + Send {
    let (user, target) = try_option_bad_request!(get_user_target_by_credentials(stream_req.username, stream_req.password, api_req, app_state), false, format!("Could not find any user for xc stream {}", stream_req.username));
    if user.permission_denied(app_state) {
        return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserAccountExpired).into_response();
    }

    let target_name = &target.name;
    if !target.has_output(&TargetType::Xtream) {
        debug!("Target has no xtream codes playlist {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
    let virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
    let (pli, mapping) = try_result_bad_request!(xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &target, None), true, format!("Failed to read xtream item for stream id {}", virtual_id));
    let input = try_option_bad_request!(app_state.app_config.get_input_by_name(pli.input_name.as_str()), true, format!("Cant find input for target {target_name}, context {}, stream_id {virtual_id}", stream_req.context));
    let cluster = pli.xtream_cluster;

    let item_type = if stream_req.context == ApiStreamContext::Timeshift { PlaylistItemType::Catchup } else  { pli.item_type };

    let session_key = format!("{fingerprint}{virtual_id}");
    let user_session = app_state.active_users.get_user_session(&user.username, &session_key);

    let session_url = if let Some(session) = &user_session {
        if session.permission == UserConnectionPermission::Exhausted {
            return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserConnectionsExhausted).into_response();
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::ProviderConnectionsExhausted).into_response();
        }

        if session.virtual_id == virtual_id && is_seek_request(cluster, req_headers).await {
            // partial request means we are in reverse proxy mode, seek happened
            return force_provider_stream_response(addr, app_state, session, item_type, req_headers, &input, &user).await.into_response();
        }

        session.stream_url.as_str()
    } else {
        pli.url.as_str()
    };

    let connection_permission = user.connection_permission(app_state);
    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserConnectionsExhausted).into_response();
    }

    let context = stream_req.context;

    let redirect_params = RedirectParams {
        item: &pli,
        provider_id: Some(mapping.provider_id),
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

    let extension = stream_ext.unwrap_or_else(
        || extract_extension_from_url(&pli.url).map_or_else(String::new, std::string::ToString::to_string));

    let query_path = if stream_req.action_path.is_empty() {
        format!("{}{extension}", pli.provider_id)
    } else {
        format!("{}/{}{extension}", stream_req.action_path, pli.provider_id)
    };

    let stream_url = try_option_bad_request!(get_xtream_player_api_stream_url(&input, stream_req.context, &query_path, session_url),
        true, format!("Cant find stream url for target {target_name}, context {}, stream_id {virtual_id}", stream_req.context));

    let is_hls_request = item_type == PlaylistItemType::LiveHls || item_type == PlaylistItemType::LiveDash || extension == HLS_EXT;
    // Reverse proxy mode
    if is_hls_request {
        return handle_hls_stream_request(fingerprint, addr, app_state, &user, user_session.as_ref(), &stream_url, pli.virtual_id, &input, connection_permission).await.into_response();
    }

    stream_response(addr, app_state, session_key.as_str(), pli.virtual_id, item_type, &stream_url, req_headers, &input, &target, &user, connection_permission).await.into_response()
}

// Used by webui
async fn xtream_player_api_stream_with_token(
    fingerprint: &str,
    addr: &str,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    target_id: u16,
    stream_req: ApiStreamRequest<'_>,
) -> impl IntoResponse + Send {
    if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
        let target_name = &target.name;
        if !target.has_output(&TargetType::Xtream) {
            debug!("Target has no xtream output {target_name}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        let (action_stream_id, stream_ext) = separate_number_and_remainder(stream_req.stream_id);
        let virtual_id: u32 = try_result_bad_request!(action_stream_id.trim().parse());
        let (pli, _mapping) = try_result_bad_request!(xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &target, None), true, format!("Failed to read xtream item for stream id {}", virtual_id));
        let input = try_option_bad_request!(app_state.app_config.get_input_by_name(pli.input_name.as_str()), true, format!("Cant find input for target {target_name}, context {}, stream_id {virtual_id}", stream_req.context));

        let session_key = format!("{fingerprint}{virtual_id}");

        let is_hls_request = pli.item_type == PlaylistItemType::LiveHls || stream_ext.as_deref() == Some(HLS_EXT);

        let config = app_state.app_config.config.load();
        let server = config.web_ui.as_ref().and_then(|web_ui| web_ui.player_server.as_ref()).map_or("default", |server_name| server_name.as_str());

        let user = ProxyUserCredentials {
            username: "api_user".to_string(),
            password: "api_user".to_string(),
            token: None,
            proxy: ProxyType::Reverse(None),
            server: Some(server.to_string()),
            epg_timeshift: None,
            created_at: None,
            exp_date: None,
            max_connections: 0,
            status: None,
            ui_enabled: false,
            comment: None,
        };

        // TODO how should we use fixed provider for hls in multi provider config?

        // Reverse proxy mode
        if is_hls_request {
            return handle_hls_stream_request(fingerprint, addr, app_state, &user, None, &pli.url, pli.virtual_id, &input, UserConnectionPermission::Allowed).await.into_response();
        }

        let extension = stream_ext.unwrap_or_else(
            || extract_extension_from_url(&pli.url).map_or_else(String::new, std::string::ToString::to_string));

        let query_path = if stream_req.action_path.is_empty() {
            format!("{}{extension}", pli.provider_id)
        } else {
            format!("{}/{}{extension}", stream_req.action_path, pli.provider_id)
        };

        let stream_url = try_option_bad_request!(get_xtream_player_api_stream_url(&input,
        stream_req.context, &query_path, pli.url.as_str()),
        true, format!("Cant find stream url for target {target_name}, context {}, stream_id {virtual_id}",
        stream_req.context));

        trace_if_enabled!("Streaming stream request from {}", sanitize_sensitive_info(&stream_url));
        stream_response(addr, app_state, session_key.as_str(), pli.virtual_id, pli.item_type, &stream_url, req_headers, &input, &target, &user, UserConnectionPermission::Allowed).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}


fn get_doc_id_and_field_name(input: &str) -> Option<(u32, &str)> {
    if let Some(pos) = input.find('_') {
        let (number_part, rest) = input.split_at(pos);
        let field = &rest[1..]; // cut _
        if let Ok(number) = number_part.parse::<u32>() {
            return Some((number, field));
        }
    }
    None
}

fn get_doc_resource_field_value<'a>(field: &'a str, doc: Option<&'a Value>) -> Option<Cow<'a, str>> {
    if let Some(Value::Object(info_data)) = doc {
        if field.starts_with(crate::model::XC_PROP_BACKDROP_PATH) {
            return get_backdrop_path_value(field, info_data.get(crate::model::XC_PROP_BACKDROP_PATH));
        } else if let Some(Value::String(url)) = info_data.get(field) {
            return Some(Cow::Borrowed(url));
        }
    }
    None
}

fn xtream_get_info_resource_url<'a>(config: &'a AppConfig, pli: &'a XtreamPlaylistItem, target: &'a ConfigTarget, resource: &'a str) -> Result<Option<Cow<'a, str>>, serde_json::Error> {
    let info_content = match pli.xtream_cluster {
        XtreamCluster::Video => {
            xtream_repository::xtream_load_vod_info(config, target.name.as_str(), pli.get_virtual_id())
        }
        XtreamCluster::Series => {
            xtream_repository::xtream_load_series_info(config, target.name.as_str(), pli.get_virtual_id())
        }
        XtreamCluster::Live => None,
    };
    if let Some(content) = info_content {
        let doc: Map<String, Value> = serde_json::from_str(&content)?;
        let (field, possible_episode_id) = if let Some(field_name_with_episode_id) = resource.strip_prefix(crate::model::XC_INFO_RESOURCE_PREFIX_EPISODE) {
            if let Some((episode_id, field_name)) = get_doc_id_and_field_name(field_name_with_episode_id) {
                (field_name, Some(episode_id))
            } else {
                return Ok(None);
            }
        } else {
            (&resource[crate::model::XC_INFO_RESOURCE_PREFIX.len()..], None)
        };
        let info_doc = match pli.xtream_cluster {
            XtreamCluster::Video | XtreamCluster::Series => {
                if let Some(episode_id) = possible_episode_id {
                    get_episode_info_doc(&doc, episode_id)
                } else {
                    doc.get(crate::model::XC_TAG_INFO_DATA)
                }
            }
            XtreamCluster::Live => None,
        };

        if let Some(value) = get_doc_resource_field_value(field, info_doc) {
            return Ok(Some(Cow::Owned(value.into_owned())));
        }
    }
    Ok(None)
}

fn get_episode_info_doc(doc: &Map<String, Value>, episode_id: u32) -> Option<&Value> {
    let episodes = doc.get(crate::model::XC_TAG_EPISODES)?.as_object()?;
    for season_episodes in episodes.values() {
        if let Value::Array(episode_list) = season_episodes {
            for episode in episode_list {
                if let Value::Object(episode_doc) = episode {
                    if let Some(episode_id_value) = episode_doc.get(crate::model::XC_TAG_ID) {
                        if let Some(doc_episode_id) = get_u32_from_serde_value(episode_id_value) {
                            if doc_episode_id == episode_id {
                                return episode_doc.get(crate::model::XC_TAG_INFO_DATA);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn get_season_info_doc(doc: &Vec<Value>, season_id: u32) -> Option<&Value> {
    for season in doc {
        if let Value::Object(season_doc) = season {
            if let Some(season_id_value) = season_doc.get(crate::model::XC_TAG_ID) {
                if let Some(doc_season_id) = get_u32_from_serde_value(season_id_value) {
                    if doc_season_id == season_id {
                        return Some(season);
                    }
                }
            }
        }
    }
    None
}


fn xtream_get_season_resource_url<'a>(config: &'a AppConfig, pli: &'a XtreamPlaylistItem, target: &'a ConfigTarget, resource: &'a str) -> Result<Option<Cow<'a, str>>, serde_json::Error> {
    let info_content = match pli.xtream_cluster {
        XtreamCluster::Series => {
            xtream_repository::xtream_load_series_info(config, target.name.as_str(), pli.get_virtual_id())
        }
        XtreamCluster::Video | XtreamCluster::Live => None,
    };
    if let Some(content) = info_content {
        let doc: Map<String, Value> = serde_json::from_str(&content)?;

        if let Some(field_name_with_season_id) = resource.strip_prefix(crate::model::XC_SEASON_RESOURCE_PREFIX) {
            if let Some((season_id, field)) = get_doc_id_and_field_name(field_name_with_season_id) {
                let seasons_doc = match pli.xtream_cluster {
                    XtreamCluster::Series => doc.get(crate::model::XC_TAG_SEASONS_DATA),
                    XtreamCluster::Video | XtreamCluster::Live => None,
                };

                if let Some(Value::Array(seasons)) = seasons_doc {
                    if let Some(value) = get_doc_resource_field_value(field, get_season_info_doc(seasons, season_id)) {
                        return Ok(Some(Cow::Owned(value.into_owned())));
                    }
                }
            }
        }
    }
    Ok(None)
}

async fn xtream_player_api_resource(
    req_headers: &HeaderMap,
    api_req: &UserApiRequest,
    app_state: &Arc<AppState>,
    resource_req: ApiStreamRequest<'_>,
) -> impl IntoResponse {
    let (user, target) = try_option_bad_request!(get_user_target_by_credentials(resource_req.username, resource_req.password, api_req, app_state), false, format!("Could not find any user xc resource {}", resource_req.username));
    if user.permission_denied(app_state) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let target_name = &target.name;
    if !target.has_output(&TargetType::Xtream) {
        debug!("Target has no xtream output {target_name}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let virtual_id: u32 = try_result_bad_request!(resource_req.stream_id.trim().parse());
    let resource = resource_req.action_path.trim();
    let (pli, _) = try_result_bad_request!(xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &target, None), true, format!("Failed to read xtream item for stream id {}", virtual_id));
    let stream_url = if resource.starts_with(crate::model::XC_INFO_RESOURCE_PREFIX) {
        try_result_bad_request!(xtream_get_info_resource_url(&app_state.app_config, &pli, &target, resource))
    } else if resource.starts_with(crate::model::XC_SEASON_RESOURCE_PREFIX) {
        try_result_bad_request!(xtream_get_season_resource_url(&app_state.app_config, &pli, &target, resource))
    } else {
        pli.get_field(resource)
    };

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
            Fingerprint(fingerprint, addr): Fingerprint,
            req_headers: HeaderMap,
            axum::extract::Path((username, password, stream_id)): axum::extract::Path<(String, String, String)>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
        ) ->  impl IntoResponse + Send {
            xtream_player_api_stream(
                &fingerprint,
                &addr,
                &req_headers,
                &app_state,
                &api_req,
                ApiStreamRequest::from($context, &username, &password, &stream_id, ""),
            ).await.into_response()
        }
    }
}

macro_rules! create_xtream_player_api_resource {
    ($fn_name:ident, $context:expr) => {
        async fn $fn_name(
            axum::extract::Path((username, password, stream_id, resource)): axum::extract::Path<(String, String, String, String)>,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Query(api_req): axum::extract::Query<UserApiRequest>,
            req_headers: HeaderMap,
        ) ->  impl IntoResponse {
            xtream_player_api_resource(&req_headers, &api_req, &app_state, ApiStreamRequest::from($context, &username, &password, &stream_id, &resource)).await.into_response()
        }
    }
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
    Fingerprint(fingerprint, addr): Fingerprint,
    req_headers: HeaderMap,
    axum::extract::Query(mut api_req): axum::extract::Query<UserApiRequest>,
    axum::extract::Path(timeshift_request): axum::extract::Path<XtreamApiTimeShiftRequest>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Form(api_form_req): axum::extract::Form<UserApiRequest>,
) -> impl IntoResponse + Send {
    let username = get_non_empty(&timeshift_request.username, &api_form_req.username, &api_req.username).to_string();
    let password = get_non_empty(&timeshift_request.password, &api_form_req.password, &api_req.password).to_string();
    let stream_id = get_non_empty(&timeshift_request.stream_id, &api_req.stream_id, &api_form_req.stream_id).to_string();
    let duration = get_non_empty(&timeshift_request.duration, &timeshift_request.duration, &api_form_req.duration);
    let start = get_non_empty(&timeshift_request.start, &timeshift_request.start, &api_form_req.start);

    let action_path = format!("{duration}/{start}");
    api_req.username = username.to_string();
    api_req.password = password.to_string();
    api_req.stream_id = stream_id.to_string();

    xtream_player_api_stream(&fingerprint, &addr, &req_headers, &app_state, &api_req,  ApiStreamRequest::from(ApiStreamContext::Timeshift, &username, &password, &stream_id, &action_path), /*&addr*/).await.into_response()
}

async fn xtream_player_api_timeshift_query_stream(
    Fingerprint(fingerprint, addr): Fingerprint,
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
    if username.is_empty() || password.is_empty() || stream_id.is_empty() || duration.is_empty() || start.is_empty() {
        // if token.is_empty() {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
        // }
        // xtream_player_api_stream(&req_headers, &api_query_req, &app_state, ApiStreamRequest::from_access_token(ApiStreamContext::Timeshift, token, stream_id, &action_path)/*, &addr*/).await.into_response()
    }
    xtream_player_api_stream(&fingerprint, &addr, &req_headers, &app_state, &api_query_req, ApiStreamRequest::from(ApiStreamContext::Timeshift, username, password, stream_id, &action_path)).await.into_response()
}


async fn xtream_get_stream_info_response(app_state: &AppState, user: &ProxyUserCredentials,
                                         target: &ConfigTarget, stream_id: &str,
                                         cluster: XtreamCluster) -> impl IntoResponse + Send {
    let virtual_id: u32 = match FromStr::from_str(stream_id) {
        Ok(id) => id,
        Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response()
    };

    if let Ok((pli, virtual_record)) = xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, target, Some(cluster)) {
        if pli.provider_id > 0 {
            let input_name = &pli.input_name;
            if let Some(input) = app_state.app_config.get_input_by_name(input_name.as_str()) {
                if let Some(info_url) = xtream::get_xtream_player_api_info_url(&input, cluster, pli.provider_id) {
                    // Redirect is only possible for live streams, vod and series info needs to be modified
                    if user.proxy == ProxyType::Redirect && cluster == XtreamCluster::Live {
                        return redirect(&info_url).into_response();
                    } else if let Ok(content) = xtream::get_xtream_stream_info(Arc::clone(&app_state.http_client.load()), &app_state.app_config, user, &input, target, &pli, info_url.as_str(), cluster).await {
                        return try_unwrap_body!(axum::response::Response::builder()
                            .status(axum::http::StatusCode::OK)
                            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                            .body(axum::body::Body::from(content)))
                    }
                }
            }
        }

        return match cluster {
            XtreamCluster::Video => {
                let content = create_vod_info_from_item(target, user, &pli, virtual_record.last_updated);
                try_unwrap_body!(axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                    .body(axum::body::Body::from(content)))
            }
            XtreamCluster::Live | XtreamCluster::Series => try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(axum::body::Body::from("{}".as_bytes()))),
        };
    }
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
        .body(axum::body::Body::from("{}".as_bytes())))
}

async fn xtream_get_short_epg(app_state: &AppState, user: &ProxyUserCredentials, target: &ConfigTarget, stream_id: &str, limit: &str) -> impl IntoResponse + Send {
    let target_name = &target.name;
    if target.has_output(&TargetType::Xtream) {
        let virtual_id: u32 = match FromStr::from_str(stream_id.trim()) {
            Ok(id) => id,
            Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response()
        };

        if let Ok((pli, _)) = xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, target, None) {
            if pli.provider_id > 0 {
                let input_name = &pli.input_name;
                if let Some(input) = app_state.app_config.get_input_by_name(input_name.as_str()) {
                    if let Some(action_url) = xtream::get_xtream_player_api_action_url(&input, crate::model::XC_ACTION_GET_SHORT_EPG) {
                        let mut info_url = format!("{action_url}&{}={}", crate::model::XC_TAG_STREAM_ID, pli.provider_id);
                        if !(limit.is_empty() || limit.eq("0")) {
                            info_url = format!("{info_url}&limit={limit}");
                        }
                        if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) {
                            return redirect(&info_url).into_response();
                        }

                        // TODO serve epg from own db
                        return match request::download_text_content(Arc::clone(&app_state.http_client.load()), &input, info_url.as_str(), None).await {
                            Ok((content, _)) => (axum::http::StatusCode::OK, [(axum::http::header::CONTENT_TYPE.to_string(), mime::APPLICATION_JSON.to_string())], content).into_response(),
                            Err(err) => {
                                error!("Failed to download epg {}", sanitize_sensitive_info(err.to_string().as_str()));
                                get_empty_epg_response().into_response()
                            }
                        };
                    }
                }
            }
        }
    }
    warn!("Cant find short epg with id: {target_name}/{stream_id}");
    get_empty_epg_response().into_response()
}

async fn xtream_player_api_handle_content_action(config: &Config, target_name: &str, action: &str, category_id: Option<u32>, user: &ProxyUserCredentials) -> Option<impl IntoResponse> {
    if let Ok((path, content)) = match action {
        crate::model::XC_ACTION_GET_LIVE_CATEGORIES => xtream_repository::xtream_get_collection_path(config, target_name, storage_const::COL_CAT_LIVE),
        crate::model::XC_ACTION_GET_VOD_CATEGORIES => xtream_repository::xtream_get_collection_path(config, target_name, storage_const::COL_CAT_VOD),
        crate::model::XC_ACTION_GET_SERIES_CATEGORIES => xtream_repository::xtream_get_collection_path(config, target_name, storage_const::COL_CAT_SERIES),
        _ => Err(str_to_io_error(""))
    } {
        if let Some(file_path) = path {
            // load user bouquet
            let filter = match action {
                crate::model::XC_ACTION_GET_LIVE_CATEGORIES => user_repository::user_get_bouquet_filter(config, &user.username, category_id, TargetType::Xtream, XtreamCluster::Live).await,
                crate::model::XC_ACTION_GET_VOD_CATEGORIES => user_repository::user_get_bouquet_filter(config, &user.username, category_id, TargetType::Xtream, XtreamCluster::Video).await,
                crate::model::XC_ACTION_GET_SERIES_CATEGORIES => user_repository::user_get_bouquet_filter(config, &user.username, category_id, TargetType::Xtream, XtreamCluster::Series).await,
                _ => None
            };
            if let Some(flt) = filter {
                return Some(serve_query(&file_path, &HashMap::from([(crate::model::XC_TAG_CATEGORY_ID, flt)])).into_response());
            }
            return Some(serve_file(&file_path, mime::APPLICATION_JSON).await.into_response());
        } else if let Some(payload) = content {
            return Some(try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .body(payload)));
        }
        return Some(api_utils::empty_json_list_response().into_response());
    }
    None
}

async fn xtream_get_catchup_response(app_state: &AppState, target: &ConfigTarget, stream_id: &str, start: &str, end: &str) -> impl IntoResponse + Send {
    let virtual_id: u32 = try_result_bad_request!(FromStr::from_str(stream_id));
    let (pli, _) = try_result_bad_request!(xtream_repository::xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, target, Some(XtreamCluster::Live)));
    let input = try_option_bad_request!(app_state.app_config.get_input_by_name(pli.input_name.as_str()));
    let info_url = try_option_bad_request!(xtream::get_xtream_player_api_action_url(&input, crate::model::XC_ACTION_GET_CATCHUP_TABLE)
        .map(|action_url| format!("{action_url}&{}={}&start={start}&end={end}", crate::model::XC_TAG_STREAM_ID, pli.provider_id)));
    let content = try_result_bad_request!(xtream::get_xtream_stream_info_content(Arc::clone(&app_state.http_client.load()), info_url.as_str(), &input).await);
    let mut doc: Map<String, Value> = try_result_bad_request!(serde_json::from_str(&content));
    let epg_listings = try_option_bad_request!(doc.get_mut(crate::model::XC_TAG_EPG_LISTINGS).and_then(Value::as_array_mut));
    let config = &app_state.app_config.config.load();
    let target_path = try_option_bad_request!(get_target_storage_path(config, target.name.as_str()));
    let (mut target_id_mapping, file_lock) = get_target_id_mapping(&app_state.app_config, &target_path).await;
    for epg_list_item in epg_listings.iter_mut().filter_map(Value::as_object_mut) {
        // TODO epg_id
        if let Some(catchup_provider_id) = epg_list_item.get(crate::model::XC_TAG_ID).and_then(Value::as_str).and_then(|id| id.parse::<u32>().ok()) {
            let uuid = generate_playlist_uuid(&hex_encode(&pli.get_uuid()), &catchup_provider_id.to_string(), pli.item_type, &pli.url);
            let virtual_id = target_id_mapping.get_and_update_virtual_id(&uuid, catchup_provider_id, PlaylistItemType::Catchup, pli.provider_id);
            epg_list_item.insert(crate::model::XC_TAG_ID.to_string(), Value::String(virtual_id.to_string()));
        }
    }
    if let Err(err) = target_id_mapping.persist() {
        error!("Failed to write catchup id mapping {err}");
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    drop(file_lock);
    serde_json::to_string(&doc)
        .map_or_else(
            |_| axum::http::StatusCode::BAD_REQUEST.into_response(),
            |result| try_unwrap_body!(axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                .body(result)))
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

async fn xtream_player_api(
    api_req: UserApiRequest,
    app_state: &Arc<AppState>,
) -> impl IntoResponse + Send {
    let user_target = get_user_target(&api_req, app_state);
    if let Some((user, target)) = user_target {
        if !target.has_output(&TargetType::Xtream) {
            return axum::response::Json(get_user_info(&user, app_state)).into_response();
        }

        let action = api_req.action.trim();
        if action.is_empty() {
            return axum::response::Json(get_user_info(&user, app_state)).into_response();
        }

        if user.permission_denied(app_state) {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }

        // Process specific playlist actions
        let (skip_live, skip_vod, skip_series) = if let Some(inputs) = app_state.app_config.get_inputs_for_target(&target.name) {
            inputs.iter().fold((true, true, true), |acc, i| {
                let (l, v, s) = acc;
                i.options.as_ref().map_or((false, false, false), |o| (l && o.xtream_skip_live, v && o.xtream_skip_vod, s && o.xtream_skip_series))
            })
        } else {
            (false, false, false)
        };

        match action {
            crate::model::XC_ACTION_GET_ACCOUNT_INFO => {
                return axum::response::Json(get_user_info(&user, app_state)).into_response();
            }
            crate::model::XC_ACTION_GET_SERIES_INFO => {
                skip_json_response_if_flag_set!(skip_series, xtream_get_stream_info_response(app_state, &user, &target, api_req.series_id.trim(), XtreamCluster::Series).await);
            }
            crate::model::XC_ACTION_GET_VOD_INFO => {
                skip_json_response_if_flag_set!(skip_vod,  xtream_get_stream_info_response(app_state, &user, &target, api_req.vod_id.trim(), XtreamCluster::Video).await);
            }
            crate::model::XC_ACTION_GET_EPG | crate::model::XC_ACTION_GET_SHORT_EPG => {
                return xtream_get_short_epg(
                    app_state, &user, &target, api_req.stream_id.trim(), api_req.limit.trim(),
                ).await.into_response();
            }
            crate::model::XC_ACTION_GET_CATCHUP_TABLE => {
                skip_json_response_if_flag_set!(skip_live, xtream_get_catchup_response(app_state, &target, api_req.stream_id.trim(), api_req.start.trim(), api_req.end.trim()).await);
            }
            _ => {}
        }

        let category_id = api_req.category_id.trim().parse::<u32>().ok();
        // Handle general content actions
        if let Some(response) = xtream_player_api_handle_content_action(
            &app_state.app_config.config.load(), &target.name, action, category_id, &user,
        ).await {
            return response.into_response();
        }

        let result = match action {
            crate::model::XC_ACTION_GET_LIVE_STREAMS =>
                skip_flag_optional!(skip_live, xtream_repository::xtream_load_rewrite_playlist(XtreamCluster::Live, &app_state.app_config, &target, category_id, &user).await),
            crate::model::XC_ACTION_GET_VOD_STREAMS =>
                skip_flag_optional!(skip_vod, xtream_repository::xtream_load_rewrite_playlist(XtreamCluster::Video, &app_state.app_config, &target, category_id, &user).await),
            crate::model::XC_ACTION_GET_SERIES =>
                skip_flag_optional!(skip_series, xtream_repository::xtream_load_rewrite_playlist(XtreamCluster::Series, &app_state.app_config, &target, category_id, &user).await),
            _ => Some(Err(info_err!(format!("Cant find content: {action} for target: {}", &target.name)))),
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
            (true, _) => debug!("Cant find user!"),
            (_, true) => debug!("Parameter action is empty!"),
            _ => debug!("Bad request!"),
        }
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

fn xtream_create_content_stream(xtream_iter: impl Iterator<Item=(String, bool)>) -> impl Stream<Item=Result<Bytes, String>> {
    stream::once(async { Ok::<Bytes, String>(Bytes::from("[")) }).chain(
        stream::iter(xtream_iter.map(move |(line, has_next)| {
            Ok::<Bytes, String>(Bytes::from(if has_next {
                format!("{line},")
            } else {
                line.to_string()
            }))
        })).chain(stream::once(async { Ok::<Bytes, String>(Bytes::from("]")) })))
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
    Fingerprint(fingerprint, addr): Fingerprint,
    axum::extract::Path((token, target_id, cluster, stream_id)): axum::extract::Path<(String, u16, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: HeaderMap,
) -> impl IntoResponse + Send {
    let ctxt = try_result_bad_request!(ApiStreamContext::from_str(cluster.as_str()));
    xtream_player_api_stream_with_token(&fingerprint, &addr, &req_headers, &app_state, target_id, ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, "")).await.into_response()
}

pub fn xtream_api_register() -> axum::Router<Arc<AppState>> {
    let router = axum::Router::new();
    let mut router = register_xtream_api!(router, ["/player_api.php", "/panel_api.php", "/xtream"]);
    router = router.route("/token/{token}/{target_id}/{cluster}/{stream_id}", axum::routing::get(xtream_player_token_stream));
    router = register_xtream_api_stream!(router, [
        ("", xtream_player_api_live_stream_alt),
        ("/live", xtream_player_api_live_stream),
        ("/movie", xtream_player_api_movie_stream),
        ("/series", xtream_player_api_series_stream)]);
    router = router.route("/timeshift/{username}/{password}/{duration}/{start}/{stream_id}", axum::routing::get(xtream_player_api_timeshift_stream));
    router = register_xtream_api_timeshift!(router, [
        "/timeshift.php",
        "/streaming/timeshift.php"]);
    register_xtream_api_resource!(router, [
        ("live", xtream_player_api_live_resource),
        ("movie", xtream_player_api_movie_resource),
        ("series", xtream_player_api_series_resource)])
}