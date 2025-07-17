use crate::api::endpoints::xtream_api::{get_xtream_player_api_stream_url, ApiStreamContext};
use crate::api::model::active_provider_manager::{ProviderAllocation, ProviderConnectionGuard};
use crate::api::model::app_state::AppState;
use crate::api::model::model_utils::get_stream_response_with_headers;
use crate::api::model::request::UserApiRequest;
use crate::api::model::stream::{BoxedProviderStream, ProviderStreamInfo, ProviderStreamResponse};
use crate::api::model::stream_error::StreamError;
use crate::api::model::streams::active_client_stream::ActiveClientStream;
use crate::api::model::streams::persist_pipe_stream::PersistPipeStream;
use crate::api::model::streams::provider_stream::{create_channel_unavailable_stream, create_custom_video_stream_response, create_provider_connections_exhausted_stream, CustomVideoStreamType};
use crate::api::model::streams::provider_stream_factory::{create_provider_stream, ProviderStreamFactoryOptions};
use crate::api::model::streams::shared_stream_manager::SharedStreamManager;
use crate::api::model::streams::throttled_stream::ThrottledStream;
use crate::auth::Claims;
use crate::model::ConfigInput;
use crate::model::{ConfigTarget, ProxyUserCredentials};
use crate::tools::atomic_once_flag::AtomicOnceFlag;
use crate::tools::lru_cache::LRUResourceCache;
use crate::utils::create_new_file_for_write;
use crate::utils::request;
use crate::utils::{debug_if_enabled, trace_if_enabled};
use crate::BUILD_TIMESTAMP;
use arc_swap::ArcSwapOption;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use log::{debug, error, log_enabled, trace};
use shared::model::{InputFetchMethod, PlaylistEntry, PlaylistItemType, TargetType, UserConnectionPermission, XtreamCluster};
use shared::utils::{default_grace_period_millis, human_readable_byte_size, trim_slash};
use shared::utils::{extract_extension_from_url, replace_url_extension, sanitize_sensitive_info, DASH_EXT, HLS_EXT};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

#[macro_export]
macro_rules! try_option_bad_request {
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Some(value) => value,
            None => {
                if $msg_is_error {error!("{}", $msg);} else {debug!("{}", $msg);}
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        }
    };
    ($option:expr) => {
        match $option {
            Some(value) => value,
            None => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        }
    };
}

#[macro_export]
macro_rules! try_result_bad_request {
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Ok(value) => value,
            Err(_) => {
                if $msg_is_error {error!("{}", $msg);} else {debug!("{}", $msg);}
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        }
    };
    ($option:expr) => {
        match $option {
            Ok(value) => value,
            Err(_) => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        }
    };
}

pub use try_option_bad_request;
pub use try_result_bad_request;
use crate::api::model::active_user_manager::UserSession;
use crate::api::model::provider_config::ProviderConfig;

pub fn get_server_time() -> String {
    chrono::offset::Local::now().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

pub fn get_build_time() -> Option<String> {
    BUILD_TIMESTAMP.to_string().parse::<DateTime<Utc>>().ok().map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}

pub fn get_memory_usage() -> String {
    crate::utils::get_memory_usage().map_or(String::from("?"), human_readable_byte_size)
}


#[allow(clippy::missing_panics_doc)]
pub async fn serve_file(file_path: &Path, mime_type: mime::Mime) -> impl IntoResponse + Send {
    if file_path.exists() {
        return match tokio::fs::File::open(file_path).await {
            Ok(file) => {
                let reader = tokio::io::BufReader::new(file);
                let stream = tokio_util::io::ReaderStream::new(reader);
                let body = axum::body::Body::from_stream(stream);

                axum::response::Response::builder()
                    .status(axum::http::StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, mime_type.to_string())
                    .header(axum::http::header::CACHE_CONTROL, axum::http::header::HeaderValue::from_static("no-cache"))
                    .body(body)
                    .unwrap()
                    .into_response()
            }
            Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}

pub fn get_user_target_by_username(username: &str, app_state: &AppState) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
    if !username.is_empty() {
        return app_state.app_config.get_target_for_username(username);
    }
    None
}

pub fn get_user_target_by_credentials<'a>(username: &str, password: &str, api_req: &'a UserApiRequest,
                                          app_state: &'a AppState) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
    if !username.is_empty() && !password.is_empty() {
        app_state.app_config.get_target_for_user(username, password)
    } else {
        let token = api_req.token.as_str().trim();
        if token.is_empty() {
            None
        } else {
            app_state.app_config.get_target_for_user_by_token(token)
        }
    }
}

pub fn get_user_target<'a>(api_req: &'a UserApiRequest, app_state: &'a AppState) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
    let username = api_req.username.as_str().trim();
    let password = api_req.password.as_str().trim();
    get_user_target_by_credentials(username, password, api_req, app_state)
}

pub struct StreamOptions {
    pub stream_retry: bool,
    pub stream_force_retry_secs: u32,
    pub buffer_enabled: bool,
    pub buffer_size: usize,
    pub pipe_provider_stream: bool,
}

/// Constructs a `StreamOptions` object based on the application's reverse proxy configuration.
///
/// This function retrieves streaming-related settings from the `AppState`:
/// - `stream_retry`: whether retrying the stream is enabled,
/// - `stream_force_retry_secs`: the number of seconds to wait before a forced retry,
/// - `buffer_enabled`: whether stream buffering is enabled,
/// - `buffer_size`: the size of the stream buffer.
///
/// If the reverse proxy or stream settings are not defined, default values are used:
/// - retry: `false`
/// - forced retry interval: `0`
/// - buffering: `false`
/// - buffer size: `0`
///
/// Additionally, it computes `pipe_provider_stream`, which is `true` only if
/// both retry and buffering are disabled—indicating that the stream can be piped directly
/// from the provider without additional handling.
///
/// Returns a `StreamOptions` instance with the resolved configuration.
fn get_stream_options(app_state: &AppState) -> StreamOptions {
    let (stream_retry, stream_force_retry_secs, buffer_enabled, buffer_size) = app_state
        .app_config.config.load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or((false, 0, false, 0), |stream| {
            let (buffer_enabled, buffer_size) = stream
                .buffer
                .as_ref()
                .map_or((false, 0), |buffer| (buffer.enabled, buffer.size));
            (stream.retry, stream.forced_retry_interval_secs, buffer_enabled, buffer_size)
        });
    let pipe_provider_stream = !stream_retry && !buffer_enabled;
    StreamOptions { stream_retry, stream_force_retry_secs, buffer_enabled, buffer_size, pipe_provider_stream }
}

// fn get_stream_content_length(provider_response: Option<&(Vec<(String, String)>, StatusCode)>) -> u64 {
//     let content_length = provider_response
//         .as_ref()
//         .and_then(|(headers, _)| headers.iter().find(|(h, _)| h.eq(axum::http::header::CONTENT_LENGTH.as_str())))
//         .and_then(|(_, val)| val.parse::<u64>().ok())
//         .unwrap_or(0);
//     content_length
// }

pub fn get_stream_alternative_url(stream_url: &str, input: &ConfigInput, alias_input: &Arc<ProviderConfig>) -> String {
    let Some(input_user_info) = input.get_user_info() else { return stream_url.to_owned() };
    let Some(alt_input_user_info) = alias_input.get_user_info() else { return stream_url.to_owned() };

    let modified = stream_url.replace(&input_user_info.base_url, &alt_input_user_info.base_url);
    let modified = modified.replace(&input_user_info.username, &alt_input_user_info.username);
    modified.replace(&input_user_info.password, &alt_input_user_info.password)
}

async fn get_redirect_alternative_url<'a>(app_state: &AppState, redirect_url: &'a str, input: &ConfigInput) -> Cow<'a, str> {
    if let Some((base_url, username, password)) = input.get_matched_config_by_url(redirect_url) {
        if let Some(provider_cfg) = app_state.active_provider.get_next_provider(&input.name).await {
            let mut new_url = redirect_url.replacen(base_url, provider_cfg.url.as_str(), 1);
            if let (Some(old_username), Some(old_password)) = (username, password) {
                if let (Some(new_username), Some(new_password)) = (provider_cfg.username.as_ref(), provider_cfg.password.as_ref()) {
                    new_url = new_url.replacen(old_username, new_username, 1);
                    new_url = new_url.replacen(old_password, new_password, 1);
                    return Cow::Owned(new_url);
                }
                // one has credentials the other not, something not right
                return Cow::Borrowed(redirect_url);
            }
            return Cow::Owned(new_url);
        }
    }
    Cow::Borrowed(redirect_url)
}

type StreamUrl = String;
type ProviderName = String;

enum ProviderStreamState {
    Custom(ProviderStreamResponse),
    Available(Option<ProviderName>, StreamUrl),
    GracePeriod(Option<ProviderName>, StreamUrl),
}

pub struct StreamDetails {
    pub stream: Option<BoxedProviderStream>,
    stream_info: ProviderStreamInfo,
    pub input_name: Option<String>,
    pub grace_period_millis: u64,
    pub reconnect_flag: Option<Arc<AtomicOnceFlag>>,
    pub provider_connection_guard: Option<ProviderConnectionGuard>,
}

impl StreamDetails {
    pub fn from_stream(stream: BoxedProviderStream) -> Self {
        Self {
            stream: Some(stream),
            stream_info: None,
            input_name: None,
            grace_period_millis: default_grace_period_millis(),
            reconnect_flag: None,
            provider_connection_guard: None,
        }
    }
    #[inline]
    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    #[inline]
    pub fn has_grace_period(&self) -> bool {
        self.grace_period_millis > 0
    }
}

struct StreamingStrategy {
    provider_connection_guard: Option<ProviderConnectionGuard>,
    provider_stream_state: ProviderStreamState,
    input_headers: Option<HashMap<String, String>>,
}

/// Determines the appropriate streaming strategy for the given input and stream URL.
///
/// This function attempts to acquire a connection to a streaming provider, either using a forced provider
/// (if specified), or based on the input name. It then selects a corresponding `StreamingOption`:
///
/// - If no connections are available (`Exhausted`), it returns a custom stream indicating exhaustion.
/// - If a connection is available or in a grace period, it constructs a streaming URL accordingly:
///   - If the provider was forced or matches the input, the original URL is reused.
///   - Otherwise, an alternative URL is generated based on the provider and input.
///
/// The function returns:
/// - an optional `ProviderConnectionGuard` to manage the connection's lifecycle,
/// - a `ProviderStreamState` describing how the stream state is,
/// - and optional HTTP headers to include in the request.
///
/// This logic helps abstract the decision-making behind provider selection and stream URL resolution.
async fn resolve_streaming_strategy(app_state: &AppState, stream_url: &str, input: &ConfigInput, force_provider: Option<&str>)
                                    -> StreamingStrategy {
    // allocate a provider connection
    let provider_connection_guard = match force_provider {
        Some(provider) => app_state.active_provider.force_exact_acquire_connection(provider).await,
        None => app_state.active_provider.acquire_connection(&input.name).await
    };
    let stream_response_params = match &*provider_connection_guard {
        ProviderAllocation::Exhausted => {
            debug!("Input {} is exhausted. No connections allowed.", input.name);
            let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
            ProviderStreamState::Custom(stream)
        }
        ProviderAllocation::Available(ref provider)
        | ProviderAllocation::GracePeriod(ref provider) => {
            // force_stream_provider means we keep the url and the provider.
            // If force_stream_provider or the input is the same as the config we don't need to get new url
            let (provider, url) = if force_provider.is_some() || provider.id == input.id {
                (input.name.to_string(), stream_url.to_string())
            } else {
                (provider.name.to_string(), get_stream_alternative_url(stream_url, input, provider))
            };

            if matches!(&*provider_connection_guard, ProviderAllocation::Available(_)) {
                ProviderStreamState::Available(Some(provider), url)
            } else {
                ProviderStreamState::GracePeriod(Some(provider), url)
            }
        }
    };
    StreamingStrategy {
        provider_connection_guard: Some(provider_connection_guard),
        provider_stream_state: stream_response_params,
        input_headers: Some(input.headers.clone()),
    }
}


fn get_grace_period_millis(connection_permission: UserConnectionPermission, stream_response_params: &ProviderStreamState, config_grace_period_millis: u64) -> u64 {
    if config_grace_period_millis > 0 &&
        (matches!(stream_response_params, ProviderStreamState::GracePeriod(_, _)) // provider grace period
            || connection_permission == UserConnectionPermission::GracePeriod // user grace period
        ) { config_grace_period_millis } else { 0 }
}

#[allow(clippy::too_many_arguments)]
async fn create_stream_response_details(app_state: &AppState,
                                        stream_options: &StreamOptions,
                                        stream_url: &str,
                                        req_headers: &HeaderMap,
                                        input: &ConfigInput,
                                        item_type: PlaylistItemType,
                                        share_stream: bool,
                                        connection_permission: UserConnectionPermission,
                                        force_provider: Option<&str>) -> StreamDetails {
    let mut streaming_strategy =
        resolve_streaming_strategy(app_state, stream_url, input, force_provider).await;
    let config_grace_period_millis = app_state.app_config.config.load().reverse_proxy.as_ref()
        .and_then(|r| r.stream.as_ref()).map_or_else(default_grace_period_millis, |s| s.grace_period_millis);
    let grace_period_millis = get_grace_period_millis(connection_permission, &streaming_strategy.provider_stream_state, config_grace_period_millis);
    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom(provider_stream) => {
            let (stream, stream_info) = provider_stream;
            StreamDetails {
                stream,
                stream_info,
                input_name: None,
                grace_period_millis,
                reconnect_flag: None,
                provider_connection_guard: streaming_strategy.provider_connection_guard.take(),
            }
        }
        ProviderStreamState::Available(provider_name, request_url) |
        ProviderStreamState::GracePeriod(provider_name, request_url) => {
            let parsed_url = Url::parse(&request_url);
            let ((stream, stream_info), reconnect_flag) = if let Ok(url) = parsed_url {
                let provider_stream_factory_options = ProviderStreamFactoryOptions::new(item_type, share_stream, stream_options, &url, req_headers, streaming_strategy.input_headers.as_ref());
                let reconnect_flag = provider_stream_factory_options.get_reconnect_flag_clone();
                let provider_stream = match create_provider_stream(Arc::clone(&app_state.app_config), Arc::clone(&app_state.http_client.load()), provider_stream_factory_options).await {
                    None => (None, None),
                    Some((stream, info)) => {
                        (Some(stream), info)
                    }
                };
                (provider_stream, Some(reconnect_flag))
            } else {
                ((None, None), None)
            };

            // if we have no stream, we should release the provider
            if stream.is_none() {
                if let Some(guard) = streaming_strategy.provider_connection_guard.take() {
                    drop(guard);
                }
                error!("Cant open stream {}", sanitize_sensitive_info(&request_url));
            }

            if log_enabled!(log::Level::Debug) {
                if let Some((headers, status_code, response_url)) = stream_info.as_ref() {
                    debug!(
                        "Responding stream request {} with status {}, headers {:?}",
                        sanitize_sensitive_info(response_url.as_ref().map_or(stream_url, |s| s.as_str())),
                        status_code,
                        headers
                    );
                }
            }

            StreamDetails {
                stream,
                stream_info,
                input_name: provider_name,
                grace_period_millis,
                reconnect_flag,
                provider_connection_guard: streaming_strategy.provider_connection_guard.take(),
            }
        }
    }
}

pub struct RedirectParams<'a, P>
where
    P: PlaylistEntry,
{
    pub item: &'a P,
    pub provider_id: Option<u32>,
    pub cluster: XtreamCluster,
    pub target_type: TargetType,
    pub target: &'a ConfigTarget,
    pub input: &'a ConfigInput,
    pub user: &'a ProxyUserCredentials,
    pub stream_ext: Option<&'a str>,
    pub req_context: ApiStreamContext,
    pub action_path: &'a str,
}

impl<P> RedirectParams<'_, P>
where
    P: PlaylistEntry,
{
    pub fn get_query_path(&self, provider_id: u32, url: &str) -> String {
        let extension = self.stream_ext.map_or_else(
            || extract_extension_from_url(url).map_or_else(String::new, std::string::ToString::to_string),
            std::string::ToString::to_string);

        // if there is an action_path (like for timeshift duration/start), it will be added in front of the stream_id
        if self.action_path.is_empty() {
            format!("{provider_id}{extension}")
        } else {
            format!("{}/{provider_id}{extension}", trim_slash(self.action_path))
        }
    }
}

pub async fn redirect_response<'a, P>(app_state: &AppState, params: &'a RedirectParams<'a, P>) -> Option<impl IntoResponse + Send>
where
    P: PlaylistEntry,
{
    let item_type = params.item.get_item_type();
    let provider_url = &params.item.get_provider_url();

    let redirect_request = params.user.proxy.is_redirect(item_type) || params.target.is_force_redirect(item_type);
    let is_hls_request = item_type == PlaylistItemType::LiveHls || params.stream_ext == Some(HLS_EXT);
    let is_dash_request = !is_hls_request && item_type == PlaylistItemType::LiveDash || params.stream_ext == Some(DASH_EXT);

    if params.target_type == TargetType::M3u {
        if redirect_request || is_dash_request {
            let redirect_url = if is_hls_request { &replace_url_extension(provider_url, HLS_EXT) } else { provider_url };
            let redirect_url = if is_dash_request { &replace_url_extension(redirect_url, DASH_EXT) } else { redirect_url };
            let redirect_url = get_redirect_alternative_url(app_state, redirect_url, params.input).await;
            debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(&redirect_url));
            return Some(redirect(&redirect_url).into_response());
        }
    } else if params.target_type == TargetType::Xtream {
        let Some(provider_id) = params.provider_id else {
            return Some(axum::http::StatusCode::BAD_REQUEST.into_response());
        };

        if redirect_request {

            // handle redirect for series but why?
            if params.cluster == XtreamCluster::Series {
                let ext = params.stream_ext.unwrap_or_default();
                let url = params.input.url.as_str();
                let username = params.input.username.as_ref().map_or("", |v| v);
                let password = params.input.password.as_ref().map_or("", |v| v);
                // TODO do i need action_path like for timeshift ?
                let stream_url = format!("{url}/series/{username}/{password}/{provider_id}{ext}");
                debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(&stream_url));
                return Some(redirect(&stream_url).into_response());
            }

            let target_name = params.target.name.as_str();
            let virtual_id = params.item.get_virtual_id();
            let stream_url = match get_xtream_player_api_stream_url(params.input, params.req_context, &params.get_query_path(provider_id, provider_url), provider_url) {
                None => {
                    error!("Cant find stream url for target {target_name}, context {}, stream_id {virtual_id}", params.req_context);
                    return Some(axum::http::StatusCode::BAD_REQUEST.into_response());
                }
                Some(url) => {
                    match app_state.active_provider.get_next_provider(&params.input.name).await {
                        Some(provider_cfg) => get_stream_alternative_url(&url, params.input, &provider_cfg),
                        None => url,
                    }
                }
            };

            // hls or dash redirect
            if is_dash_request {
                let redirect_url = if is_hls_request { &replace_url_extension(&stream_url, HLS_EXT) } else { &replace_url_extension(&stream_url, DASH_EXT) };
                debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(redirect_url));
                return Some(redirect(redirect_url).into_response());
            }

            debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(&stream_url));
            return Some(redirect(&stream_url).into_response());
        }
    }

    None
}

fn is_throttled_stream(item_type: PlaylistItemType, throttle_kbps: usize) -> bool {
    throttle_kbps > 0 && matches!(item_type, PlaylistItemType::Video | PlaylistItemType::Series  | PlaylistItemType::SeriesInfo | PlaylistItemType::Catchup)
}

fn prepare_body_stream(app_state: &AppState, item_type: PlaylistItemType, stream: ActiveClientStream) -> axum::body::Body {
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let body_stream = if is_throttled_stream(item_type, throttle_kbps) {
        axum::body::Body::from_stream(ThrottledStream::new(stream.boxed(), throttle_kbps))
    } else {
        axum::body::Body::from_stream(stream)
    };
    body_stream
}

/// # Panics
pub async fn force_provider_stream_response(addr: &str,
                                            app_state: &AppState,
                                            user_session: &UserSession,
                                            item_type: PlaylistItemType,
                                            req_headers: &HeaderMap,
                                            input: &ConfigInput,
                                            user: &ProxyUserCredentials) -> impl IntoResponse + Send {
    let stream_options = get_stream_options(app_state);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;

    let mut stream_details =
        create_stream_response_details(app_state, &stream_options, &user_session.stream_url, req_headers, input, item_type, share_stream, connection_permission, Some(&user_session.provider)).await;

    if stream_details.has_stream() {
        let provider_response = stream_details.stream_info.as_ref().map(|(h, sc, url)| (h.clone(), *sc, url.clone()));
        app_state.active_users.update_session_addr(&user.username, &user_session.token, addr).await;
        let stream = ActiveClientStream::new(stream_details, app_state, user, connection_permission, addr).await;

        let (status_code, header_map) = get_stream_response_with_headers(provider_response.map(|(h, s, _)| (h, s)));
        let mut response = axum::response::Response::builder().status(status_code);
        for (key, value) in &header_map {
            response = response.header(key, value);
        }

        let body_stream = prepare_body_stream(app_state, item_type, stream);
        debug_if_enabled!("Streaming provider forced stream request from {}", sanitize_sensitive_info(&user_session.stream_url));
        return response.body(body_stream).unwrap().into_response();
    }
    drop(stream_details.provider_connection_guard.take());
    if let (Some(stream), _stream_info) =
        create_channel_unavailable_stream(&app_state.app_config, &[], axum::http::StatusCode::BAD_GATEWAY)
    {
        debug!("Streaming custom stream");
        axum::response::Response::builder().status(axum::http::StatusCode::OK).body(axum::body::Body::from_stream(stream)).unwrap().into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

/// # Panics
#[allow(clippy::too_many_arguments)]
pub async fn stream_response(addr: &str,
                             app_state: &AppState, session_token: &str, virtual_id: u32,
                             item_type: PlaylistItemType, stream_url: &str, req_headers: &HeaderMap,
                             input: &ConfigInput, target: &ConfigTarget, user: &ProxyUserCredentials,
                             connection_permission: UserConnectionPermission) -> impl IntoResponse + Send {
    if log_enabled!(log::Level::Trace) { trace!("Try to open stream {}", sanitize_sensitive_info(stream_url)); }

    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(&app_state.app_config, CustomVideoStreamType::UserConnectionsExhausted).into_response();
    }

    let share_stream = is_stream_share_enabled(item_type, target);
    if share_stream {
        if let Some(value) = shared_stream_response(app_state, stream_url, user, connection_permission, addr).await {
            return value.into_response();
        }
    }

    let stream_options = get_stream_options(app_state);
    let mut stream_details =
        create_stream_response_details(app_state, &stream_options, stream_url, req_headers, input, item_type, share_stream, connection_permission, None).await;
    if stream_details.has_stream() {
        // let content_length = get_stream_content_length(provider_response.as_ref());
        let provider_response = stream_details.stream_info.as_ref().map(|(h, sc, response_url)| (h.clone(), *sc, response_url.clone()));
        let provider_name = stream_details.provider_connection_guard.as_ref().and_then(ProviderConnectionGuard::get_provider_name);

        let stream = ActiveClientStream::new(stream_details, app_state, user, connection_permission, addr).await;
        let stream_resp = if share_stream {
            debug_if_enabled!("Streaming shared stream request from {}", sanitize_sensitive_info(stream_url));
            // Shared Stream response
            let shared_headers = provider_response.as_ref().map_or_else(Vec::new, |(h, _, _)| h.clone());
            SharedStreamManager::subscribe(app_state, stream_url, stream, shared_headers, stream_options.buffer_size).await;
            if let Some(broadcast_stream) = SharedStreamManager::subscribe_shared_stream(app_state, stream_url).await {
                let (status_code, header_map) = get_stream_response_with_headers(provider_response.map(|(h, s, _)| (h, s)));
                let mut response = axum::response::Response::builder()
                    .status(status_code);
                for (key, value) in &header_map {
                    response = response.header(key, value);
                }
                response.body(axum::body::Body::from_stream(broadcast_stream)).unwrap().into_response()
            } else {
                axum::http::StatusCode::BAD_REQUEST.into_response()
            }
        } else {
            let session_url = provider_response.as_ref().and_then(|(_, _, u)| u.as_ref()).map_or_else(|| Cow::Borrowed(stream_url), |url| Cow::Owned(url.to_string()));
            if log_enabled!(log::Level::Debug) {
                if session_url.eq(&stream_url) {
                    debug!("Streaming stream request from {}", sanitize_sensitive_info(stream_url));
                } else {
                    debug!("Streaming stream request for {} from {}", sanitize_sensitive_info(stream_url), sanitize_sensitive_info(&session_url));
                }
            }
            let (status_code, header_map) = get_stream_response_with_headers(provider_response.map(|(h, s, _)| (h, s)));
            let mut response = axum::response::Response::builder().status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }

            if let Some(provider) = provider_name {
                if matches!(item_type, PlaylistItemType::LiveHls  | PlaylistItemType::LiveDash | PlaylistItemType::Video | PlaylistItemType::Series | PlaylistItemType::Catchup) {
                    let _ = app_state.active_users.create_user_session(user, session_token, virtual_id, &provider, &session_url, addr, connection_permission).await;
                }
            }

            let body_stream = prepare_body_stream(app_state, item_type, stream);
            response.body(body_stream).unwrap().into_response()
        };

        return stream_resp.into_response();
    }
    drop(stream_details.provider_connection_guard.take());
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

fn get_stream_throttle(app_state: &AppState) -> u64 {
    app_state.app_config.config.load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map(|stream| stream.throttle_kbps).unwrap_or_default()
}

async fn shared_stream_response(app_state: &AppState, stream_url: &str, user: &ProxyUserCredentials, connect_permission: UserConnectionPermission, addr: &str) -> Option<impl IntoResponse> {
    if let Some(stream) = SharedStreamManager::subscribe_shared_stream(app_state, stream_url).await {
        debug_if_enabled!("Using shared stream {}", sanitize_sensitive_info(stream_url));
        if let Some(headers) = app_state.shared_stream_manager.get_shared_state_headers(stream_url).await {
            let (status_code, header_map) = get_stream_response_with_headers(Some((headers.clone(), axum::http::StatusCode::OK)));
            let stream_details = StreamDetails::from_stream(stream);
            let stream = ActiveClientStream::new(stream_details, app_state, user, connect_permission, addr).await.boxed();
            let mut response = axum::response::Response::builder()
                .status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }
            return Some(response.body(axum::body::Body::from_stream(stream)).unwrap());
        }
    }
    None
}

pub fn is_stream_share_enabled(item_type: PlaylistItemType, target: &ConfigTarget) -> bool {
    (item_type == PlaylistItemType::Live  /* || item_type == PlaylistItemType::LiveHls */) && target.options.as_ref().is_some_and(|opt| opt.share_live_streams)
}

pub type HeaderFilter = Option<Box<dyn Fn(&str) -> bool + Send>>;
pub fn get_headers_from_request(req_headers: &HeaderMap, filter: &HeaderFilter) -> HashMap<String, Vec<u8>> {
    req_headers
        .iter()
        .filter(|(k, _)| match &filter {
            None => true,
            Some(predicate) => predicate(k.as_str())
        })
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect()
}

fn get_add_cache_content(res_url: &str, cache: &Arc<ArcSwapOption<Mutex<LRUResourceCache>>>) -> Arc<dyn Fn(usize) + Send + Sync> {
    let resource_url = String::from(res_url);
    let cache = Arc::clone(cache);
    let add_cache_content: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |size| {
        let res_url = resource_url.clone();
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            if let Some(cache) = cache.load().as_ref() {
                let _ = cache.lock().await.add_content(&res_url, size);
            }
        });
    });
    add_cache_content
}

/// # Panics
pub async fn resource_response(app_state: &AppState, resource_url: &str, req_headers: &HeaderMap, input: Option<&ConfigInput>) -> impl IntoResponse + Send {
    if resource_url.is_empty() {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    let filter: HeaderFilter = Some(Box::new(|key| key != "if-none-match" && key != "if-modified-since"));
    let req_headers = get_headers_from_request(req_headers, &filter);
    if let Some(cache) = app_state.cache.load().as_ref() {
        let mut guard = cache.lock().await;
        if let Some(resource_path) = guard.get_content(resource_url) {
            trace_if_enabled!("Responding resource from cache {}", sanitize_sensitive_info(resource_url));
            return serve_file(&resource_path, mime::APPLICATION_OCTET_STREAM).await.into_response();
        }
    }
    trace_if_enabled!("Try to fetch resource {}", sanitize_sensitive_info(resource_url));
    if let Ok(url) = Url::parse(resource_url) {
        let client = request::get_client_request(&app_state.http_client.load(), input.map_or(InputFetchMethod::GET, |i| i.method), input.map(|i| &i.headers), &url, Some(&req_headers));
        match client.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    let mut response_builder = axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK);
                    for (key, value) in response.headers() {
                        response_builder = response_builder.header(key, value);
                    }

                    let byte_stream = response.bytes_stream().map_err(|err| StreamError::reqwest(&err));
                    let cache_resource_path = {
                        if let Some(cache) = app_state.cache.load().as_ref() {
                            Some(cache.lock().await.store_path(resource_url))
                        } else {
                            None
                        }
                    };
                    if let Some(resource_path) = cache_resource_path {
                        if let Ok(file) = create_new_file_for_write(&resource_path) {
                            let writer = BufWriter::new(file);
                            let add_cache_content = get_add_cache_content(resource_url, &app_state.cache);
                            let stream = PersistPipeStream::new(byte_stream, writer, add_cache_content);
                            return response_builder.body(axum::body::Body::from_stream(stream)).unwrap().into_response();
                        }
                    }
                    return response_builder.body(axum::body::Body::from_stream(byte_stream)).unwrap().into_response();
                }
                debug_if_enabled!("Failed to open resource got status {} for {}", status, sanitize_sensitive_info(resource_url));
            }
            Err(err) => {
                error!("Received failure from server {}:  {}", sanitize_sensitive_info(resource_url), err);
            }
        }
    } else {
        error!("Url is malformed {}", sanitize_sensitive_info(resource_url));
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}

pub fn separate_number_and_remainder(input: &str) -> (String, Option<String>) {
    input.rfind('.').map_or_else(|| (input.to_string(), None), |dot_index| {
        let number_part = input[..dot_index].to_string();
        let rest = input[dot_index..].to_string();
        (number_part, if rest.len() < 2 { None } else { Some(rest) })
    })
}

/// # Panics
pub fn empty_json_list_response() -> impl IntoResponse + Send {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", mime::APPLICATION_JSON.to_string())
        .body("[]".to_string())
        .unwrap()
        .into_response()
}

pub fn get_username_from_auth_header(
    token: &str,
    app_state: &Arc<AppState>,
) -> Option<String> {
    if let Some(web_auth_config) = &app_state.app_config.config.load().web_ui.as_ref().and_then(|c| c.auth.as_ref()) {
        let secret_key: &str = web_auth_config.secret.as_ref();
        if let Ok(token_data) = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret_key.as_bytes()),
            &Validation::new(Algorithm::HS256),
        ) {
            return Some(token_data.claims.username);
        }
    }
    None
}

/// # Panics
pub fn redirect(url: &str) -> impl IntoResponse {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header("Location", url)
        .body(axum::body::Body::empty())
        .unwrap()
}

pub async fn is_seek_request(
    cluster: XtreamCluster,
    req_headers: &HeaderMap,
) -> bool {
    // seek only for non-live streams
    if cluster == XtreamCluster::Live {
        return false;
    }

    // seek requests contains range header
    let range = req_headers
        .get("range")
        .and_then(|h| h.to_str().ok())
        .map(ToString::to_string);

    if let Some(range) = range {
        if range.starts_with("bytes=0-") {
            return false;
        }
        if range.starts_with("bytes=") {
            return true;
        }
    }
    false
}
