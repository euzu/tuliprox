use crate::api::endpoints::xtream_api::{get_xtream_player_api_stream_url, ApiStreamContext};
use crate::api::model::{create_channel_unavailable_stream, create_custom_video_stream_response,
                        create_provider_connections_exhausted_stream, create_provider_stream,
                        get_stream_response_with_headers, create_active_client_stream, AppState,
                        CustomVideoStreamType, ProviderStreamFactoryOptions,
                        SharedStreamManager, StreamError, ThrottledStream, UserApiRequest};
use crate::api::model::{tee_stream, UserSession};
use crate::api::model::{ProviderAllocation, ProviderConfig, ProviderStreamState, StreamDetails, StreamingStrategy};
use crate::auth::Fingerprint;
use crate::model::{ConfigInput};
use crate::model::{ConfigTarget, ProxyUserCredentials};
use crate::tools::lru_cache::LRUResourceCache;
use crate::utils::request::{content_type_from_ext, parse_range, send_with_retry};
use crate::utils::{async_file_reader, async_file_writer, create_new_file_for_write, get_file_extension};
use crate::utils::{debug_if_enabled, trace_if_enabled};
use crate::utils::request;
use crate::BUILD_TIMESTAMP;

use arc_swap::ArcSwapOption;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt, TryStreamExt};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use log::{debug, error, info, log_enabled, trace, warn};
use serde::Serialize;
use shared::concat_string;
use shared::model::{Claims, InputFetchMethod, PlaylistEntry, PlaylistItemType, ProxyType, StreamChannel, TargetType, UserConnectionPermission, VirtualId, XtreamCluster};
use shared::utils::{bin_serialize, human_readable_kbps, trim_slash, Internable, CONTENT_TYPE_CBOR};
use shared::utils::{
    extract_extension_from_url, replace_url_extension, sanitize_sensitive_info, DASH_EXT, HLS_EXT,
};
use std::borrow::Cow;
use std::collections::{HashMap};
use std::convert::Infallible;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;
use url::Url;

#[macro_export]
macro_rules! try_option_bad_request {
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Some(value) => value,
            None => {
                if $msg_is_error {
                    error!("{}", $msg);
                } else {
                    debug!("{}", $msg);
                }
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
macro_rules! internal_server_error {
    () => {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
    };
}

#[macro_export]
macro_rules! try_unwrap_body {
    ($body:expr) => {
        $body.map_or_else(
            |_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            |resp| resp.into_response(),
        )
    };
}

#[macro_export]
macro_rules! try_result_or_status {
    ($option:expr, $status:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Ok(value) => value,
            Err(_) => {
                if $msg_is_error {
                    error!("{}", $msg);
                } else {
                    debug!("{}", $msg);
                }
                return $status.into_response();
            }
        }
    };
    ($option:expr, $status:expr) => {
        match $option {
            Ok(value) => value,
            Err(_) => return $status.into_response(),
        }
    };
}

#[macro_export]
macro_rules! try_result_bad_request {
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
       $crate::api::api_utils::try_result_or_status!($option, axum::http::StatusCode::BAD_REQUEST, $msg_is_error, $msg)
    };
    ($option:expr) => {
       $crate::api::api_utils::try_result_or_status!($option, axum::http::StatusCode::BAD_REQUEST)
    };
}

#[macro_export]
macro_rules! try_result_not_found {
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
       $crate::api::api_utils::try_result_or_status!($option, axum::http::StatusCode::NOT_FOUND, $msg_is_error, $msg)
    };
    ($option:expr) => {
       $crate::api::api_utils::try_result_or_status!($option, axum::http::StatusCode::NOT_FOUND)
    };
}

pub use try_result_or_status;
pub use try_option_bad_request;
pub use try_result_bad_request;
pub use try_result_not_found;
pub use try_unwrap_body;
pub use internal_server_error;
use crate::api::panel_api::{can_provision_on_exhausted, create_panel_api_provisioning_stream_details};

pub fn get_server_time() -> String {
    chrono::offset::Local::now()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

pub fn get_build_time() -> Option<String> {
    BUILD_TIMESTAMP
        .to_string()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}

#[allow(clippy::missing_panics_doc)]
pub async fn serve_file(file_path: &Path, mime_type: String, cache_control: Option<&str>) -> impl IntoResponse + Send {
    match tokio::fs::try_exists(file_path).await {
        Ok(exists) => {
            if !exists {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        }
        Err(err) => {
            error!("Failed to open file {}, {err:?}", file_path.display());
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    match tokio::fs::File::open(file_path).await {
        Ok(file) => {
            let last_modified = file.metadata().await.ok()
                .and_then(|m| m.modified().ok())
                .map(|m| {
                    let dt: DateTime<Utc> = m.into();
                    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
                });

            let reader = async_file_reader(file);
            let stream = tokio_util::io::ReaderStream::new(reader);
            let body = axum::body::Body::from_stream(stream);

            let mut builder = axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime_type)
                .header(
                    axum::http::header::CACHE_CONTROL,
                    cache_control.unwrap_or("no-cache")
                );

            if let Some(lm) = last_modified {
                builder = builder.header(axum::http::header::LAST_MODIFIED, lm);
            }

            try_unwrap_body!(builder.body(body))
        }
        Err(_) => internal_server_error!(),
    }
}

pub fn get_user_target_by_username(
    username: &str,
    app_state: &Arc<AppState>,
) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
    if !username.is_empty() {
        return app_state.app_config.get_target_for_username(username);
    }
    None
}

pub fn get_user_target_by_credentials<'a>(
    username: &str,
    password: &str,
    api_req: &'a UserApiRequest,
    app_state: &'a AppState,
) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
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

pub fn get_user_target<'a>(
    api_req: &'a UserApiRequest,
    app_state: &'a AppState,
) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
    let username = api_req.username.as_str().trim();
    let password = api_req.password.as_str().trim();
    get_user_target_by_credentials(username, password, api_req, app_state)
}

pub struct StreamOptions {
    pub stream_retry: bool,
    pub buffer_enabled: bool,
    pub buffer_size: usize,
    pub pipe_provider_stream: bool,
}

/// Constructs a `StreamOptions` object based on the application's reverse proxy configuration.
///
/// This function retrieves streaming-related settings from the `AppState`:
/// - `stream_retry`: whether retrying the stream is enabled,
/// - `buffer_enabled`: whether stream buffering is enabled,
/// - `buffer_size`: the size of the stream buffer.
///
/// If the reverse proxy or stream settings are not defined, default values are used:
/// - retry: `false`
/// - buffering: `false`
/// - buffer size: `0`
///
/// Additionally, it computes `pipe_provider_stream`, which is `true` only if
/// both retry and buffering are disabled—indicating that the stream can be piped directly
/// from the provider without additional handling.
///
/// Returns a `StreamOptions` instance with the resolved configuration.
fn get_stream_options(app_state: &Arc<AppState>) -> StreamOptions {
    let (stream_retry, buffer_enabled, buffer_size) = app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or((false, false, 0), |stream| {
            let (buffer_enabled, buffer_size) = stream
                .buffer
                .as_ref()
                .map_or((false, 0), |buffer| (buffer.enabled, buffer.size));
            (
                stream.retry,
                buffer_enabled,
                buffer_size,
            )
        });
    let pipe_provider_stream = !stream_retry && !buffer_enabled;
    StreamOptions {
        stream_retry,
        buffer_enabled,
        buffer_size,
        pipe_provider_stream,
    }
}

pub fn get_stream_alternative_url(
    stream_url: &str,
    input: &ConfigInput,
    alias_input: &Arc<ProviderConfig>,
) -> String {
    let Some(input_user_info) = input.get_user_info() else {
        return stream_url.to_string();
    };
    let Some(alt_input_user_info) = alias_input.get_user_info() else {
        return stream_url.to_string();
    };

    let modified = stream_url.replacen(&input_user_info.base_url, &alt_input_user_info.base_url, 1);
    let modified = modified.replacen(&input_user_info.username, &alt_input_user_info.username, 1);
    modified.replacen(&input_user_info.password, &alt_input_user_info.password, 1)
}

async fn get_redirect_alternative_url(
    app_state: &Arc<AppState>,
    redirect_url: &Arc<str>,
    input: &ConfigInput,
) -> Arc<str> {
    if let Some((base_url, username, password)) = input.get_matched_config_by_url(redirect_url) {
        if let Some(provider_cfg) = app_state
            .active_provider
            .get_next_provider(&input.name)
            .await
        {
            let mut new_url = redirect_url.replacen(base_url, provider_cfg.url.as_str(), 1);
            if let (Some(old_username), Some(old_password)) = (username, password) {
                if let (Some(new_username), Some(new_password)) = (
                    provider_cfg.username.as_ref(),
                    provider_cfg.password.as_ref(),
                ) {
                    new_url = new_url.replacen(old_username, new_username, 1);
                    new_url = new_url.replacen(old_password, new_password, 1);
                    return new_url.into();
                }
                // one has credentials the other not, something not right
                return redirect_url.clone();
            }
            return new_url.into();
        }
    }
    redirect_url.clone()
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
async fn resolve_streaming_strategy(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    input: &ConfigInput,
    force_provider: Option<&Arc<str>>,
) -> StreamingStrategy {
    // allocate a provider connection
    let provider_connection_handle = match force_provider {
        Some(provider) => app_state.active_provider.force_exact_acquire_connection(provider, &fingerprint.addr).await,
        None => app_state.active_provider.acquire_connection(&input.name, &fingerprint.addr).await,
    };

    // panel_api provisioning/loading is handled later in the stream creation flow

    let stream_response_params =
        if let Some(allocation) = provider_connection_handle.as_ref().map(|ph| &ph.allocation) {
            match allocation {
                ProviderAllocation::Exhausted => {
                    debug!("Provider {} is exhausted. No connections allowed.", input.name);
                    let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
                    ProviderStreamState::Custom(stream)
                }
                ProviderAllocation::Available(ref provider_cfg)
                | ProviderAllocation::GracePeriod(ref provider_cfg) => {
                    // force_stream_provider means we keep the url and the provider.
                    // If force_stream_provider or the input is the same as the config we don't need to get new url
                    let (selected_provider_name, url) = if force_provider.is_some() || provider_cfg.id == input.id {
                        (input.name.clone(), stream_url.to_string())
                    } else {
                        (provider_cfg.name.clone(), get_stream_alternative_url(stream_url, input, provider_cfg))
                    };

                    debug_if_enabled!(
                        "provider session: input={} provider_cfg={} user={} allocation={} stream_url={}",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&provider_cfg.name),
                        sanitize_sensitive_info(provider_cfg.get_user_info().as_ref().map_or_else(|| "?", |u| u.username.as_str())),
                        allocation.short_key(),
                        sanitize_sensitive_info(&url)
                    );

                    match allocation {
                        ProviderAllocation::Exhausted => {
                            let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
                            ProviderStreamState::Custom(stream)
                        }
                        ProviderAllocation::Available(_) => ProviderStreamState::Available(Some(selected_provider_name.intern()), url.intern()),
                        ProviderAllocation::GracePeriod(_) => ProviderStreamState::GracePeriod(Some(selected_provider_name.intern()), url.intern()),
                    }
                }
            }
        } else {
            debug!("Provider {} is exhausted. No connections allowed.", input.name);
            let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
            ProviderStreamState::Custom(stream)
        };


    StreamingStrategy {
        provider_handle: provider_connection_handle,
        provider_stream_state: stream_response_params,
        input_headers: Some(input.headers.clone()),
    }
}

fn get_grace_period_millis(
    connection_permission: UserConnectionPermission,
    stream_response_params: &ProviderStreamState,
    config_grace_period_millis: u64,
) -> u64 {
    if config_grace_period_millis > 0
        && (
        matches!(stream_response_params, ProviderStreamState::GracePeriod(_, _)) // provider grace period
            || connection_permission == UserConnectionPermission::GracePeriod
        // user grace period
    )
    {
        config_grace_period_millis
    } else {
        0
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn create_stream_response_details(
    app_state: &Arc<AppState>,
    stream_options: &StreamOptions,
    stream_url: &str,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    item_type: PlaylistItemType,
    share_stream: bool,
    connection_permission: UserConnectionPermission,
    force_provider: Option<&Arc<str>>,
    virtual_id: VirtualId,
) -> StreamDetails {
    let mut streaming_strategy = resolve_streaming_strategy(app_state, stream_url, fingerprint, input, force_provider).await;
    let mut grace_period_options = app_state.get_grace_options();
    grace_period_options.period_millis = get_grace_period_millis(
        connection_permission,
        &streaming_strategy.provider_stream_state,
        grace_period_options.period_millis,
    );

    let guard_provider_name = streaming_strategy
        .provider_handle
        .as_ref()
        .and_then(|guard| guard.allocation.get_provider_name());

    if matches!(streaming_strategy.provider_stream_state, ProviderStreamState::Custom(_))
        && can_provision_on_exhausted(app_state, input)
    {
        if let Some(handle) = streaming_strategy.provider_handle.take() {
            app_state
                .connection_manager
                .release_provider_handle(Some(handle))
                .await;
        }
        debug_if_enabled!(
            "panel_api: provider connections exhausted; sending provisioning stream for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return create_panel_api_provisioning_stream_details(
            app_state,
            input,
            guard_provider_name.clone(),
            &grace_period_options,
            fingerprint.addr,
            virtual_id,
        );
    }

    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom(provider_stream) => {
            let (stream, stream_info) = provider_stream;
            StreamDetails {
                stream,
                stream_info,
                provider_name: guard_provider_name.clone(),
                grace_period: grace_period_options,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: streaming_strategy.provider_handle.clone(),
            }
        }
        ProviderStreamState::Available(_provider_name, request_url)
        | ProviderStreamState::GracePeriod(_provider_name, request_url) => {
            let parsed_url = Url::parse(&request_url);
            let ((stream, stream_info), reconnect_flag) = if let Ok(url) = parsed_url {
                let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
                let disabled_headers = app_state.get_disabled_headers();
                let provider_stream_factory_options = ProviderStreamFactoryOptions::new(
                    fingerprint.addr,
                    item_type,
                    share_stream,
                    stream_options,
                    &url,
                    req_headers,
                    streaming_strategy.input_headers.as_ref(),
                    disabled_headers.as_ref(),
                    default_user_agent.as_deref(),
                );
                let reconnect_flag = provider_stream_factory_options.get_reconnect_flag_clone();
                let provider_stream = match create_provider_stream(
                    app_state,
                    &app_state.http_client.load(),
                    provider_stream_factory_options,
                )
                    .await
                {
                    None => (None, None),
                    Some((stream, info)) => (Some(stream), info),
                };
                (provider_stream, Some(reconnect_flag))
            } else {
                ((None, None), None)
            };

            if log_enabled!(log::Level::Debug) {
                if let Some((headers, status_code, response_url, _custom_video_type)) = stream_info.as_ref() {
                    debug!(
                        "Responding stream request {} with status {}, headers {:?}",
                        sanitize_sensitive_info(
                            response_url.as_ref().map_or(stream_url, |s| s.as_str())
                        ),
                        status_code,
                        headers
                    );
                }
            }

            // if we have no stream, we should release the provider
            let provider_handle = if stream.is_none() {
                let provider_handle = streaming_strategy.provider_handle.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
                error!("Can't open stream {}", sanitize_sensitive_info(&request_url));
                None
            } else {
                streaming_strategy.provider_handle.take()
            };

            StreamDetails {
                stream,
                stream_info,
                provider_name: guard_provider_name.clone(),
                grace_period: grace_period_options,
                disable_provider_grace: false,
                reconnect_flag,
                provider_handle,
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
            || {
                extract_extension_from_url(url)
                    .map_or_else(String::new, ToString::to_string)
            },
            ToString::to_string,
        );

        // if there is an action_path (like for timeshift duration/start), it will be added in front of the stream_id
        if self.action_path.is_empty() {
            concat_string!(&provider_id.to_string(), &extension)
        } else {
            concat_string!(&trim_slash(self.action_path), "/", &provider_id.to_string(), &extension)
        }
    }
}

pub async fn redirect_response<'a, P>(
    app_state: &Arc<AppState>,
    params: &'a RedirectParams<'a, P>,
) -> Option<impl IntoResponse + Send>
where
    P: PlaylistEntry,
{
    let item_type = params.item.get_item_type();
    let provider_url = params.item.get_provider_url();

    let redirect_request =
        params.user.proxy.is_redirect(item_type) || params.target.is_force_redirect(item_type);
    let is_hls_request =
        item_type == PlaylistItemType::LiveHls || params.stream_ext == Some(HLS_EXT);
    let is_dash_request = (!is_hls_request && item_type == PlaylistItemType::LiveDash)
        || params.stream_ext == Some(DASH_EXT);

    if params.target_type == TargetType::M3u {
        if redirect_request || is_dash_request {
            let redirect_url: Arc<str> = if is_hls_request {
                replace_url_extension(&provider_url, HLS_EXT).into()
            } else {
                provider_url.clone()
            };
            let redirect_url = if is_dash_request {
                replace_url_extension(&redirect_url, DASH_EXT).into()
            } else {
                redirect_url
            };
            let redirect_url =
                get_redirect_alternative_url(app_state, &redirect_url, params.input).await;
            debug_if_enabled!(
                "Redirecting stream request to {}",
                sanitize_sensitive_info(&redirect_url)
            );
            return Some(redirect(&redirect_url).into_response());
        }
    } else if params.target_type == TargetType::Xtream {
        let Some(provider_id) = params.provider_id else {
            return Some(StatusCode::BAD_REQUEST.into_response());
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
                debug_if_enabled!(
                    "Redirecting stream request to {}",
                    sanitize_sensitive_info(&stream_url)
                );
                return Some(redirect(&stream_url).into_response());
            }

            let target_name = params.target.name.as_str();
            let virtual_id = params.item.get_virtual_id();
            let stream_url = match get_xtream_player_api_stream_url(
                params.input,
                params.req_context,
                &params.get_query_path(provider_id, &provider_url),
                &provider_url,
            ) {
                None => {
                    error!("Can't find stream url for target {target_name}, context {}, stream_id {virtual_id}", params.req_context);
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
                Some(url) => {
                    match app_state
                        .active_provider
                        .get_next_provider(&params.input.name)
                        .await
                    {
                        Some(provider_cfg) => {
                            get_stream_alternative_url(&url, params.input, &provider_cfg)
                        }
                        None => url.to_string(),
                    }
                }
            };

            // hls or dash redirect
            if is_dash_request {
                let redirect_url = if is_hls_request {
                    &replace_url_extension(&stream_url, HLS_EXT)
                } else {
                    &replace_url_extension(&stream_url, DASH_EXT)
                };
                debug_if_enabled!(
                    "Redirecting stream request to {}",
                    sanitize_sensitive_info(redirect_url)
                );
                return Some(redirect(redirect_url).into_response());
            }

            debug_if_enabled!(
                "Redirecting stream request to {}",
                sanitize_sensitive_info(&stream_url)
            );
            return Some(redirect(&stream_url).into_response());
        }
    }

    None
}

fn is_throttled_stream(item_type: PlaylistItemType, throttle_kbps: usize) -> bool {
    throttle_kbps > 0
        && matches!(
            item_type,
            PlaylistItemType::Video
                | PlaylistItemType::Series
                | PlaylistItemType::SeriesInfo
                | PlaylistItemType::Catchup
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::LocalSeries
                | PlaylistItemType::LocalSeriesInfo
        )
}

fn prepare_body_stream<S>(
    app_state: &Arc<AppState>,
    item_type: PlaylistItemType,
    stream: S,
) -> axum::body::Body
where
    S: futures::Stream<Item=Result<bytes::Bytes, StreamError>> + Send + 'static,
{
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let body_stream = if is_throttled_stream(item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        axum::body::Body::from_stream(ThrottledStream::new(stream.boxed(), throttle_kbps))
    } else {
        axum::body::Body::from_stream(stream)
    };
    body_stream
}

/// # Panics
pub async fn force_provider_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    mut stream_channel: StreamChannel,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
) -> impl IntoResponse + Send {
    let stream_options = get_stream_options(app_state);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;
    let item_type = stream_channel.item_type;

    // Release the existing provider connection for this session before acquiring a new one.
    // This is critical for users with a connection limit of 1 to avoid "Provider exhausted" or provider-side 502/509 errors during seeking.
    app_state.connection_manager.release_provider_connection(&user_session.addr).await;

    let stream_details = create_stream_response_details(
        app_state,
        &stream_options,
        &user_session.stream_url,
        fingerprint,
        req_headers,
        input,
        item_type,
        share_stream,
        connection_permission,
        Some(&user_session.provider),
        stream_channel.virtual_id,
    )
        .await;

    if stream_details.has_stream() {
        let provider_response = stream_details
            .stream_info
            .as_ref()
            .map(|(h, sc, url, cvt)| (h.clone(), *sc, url.clone(), *cvt));
        app_state
            .active_users
            .update_session_addr(&user.username, &user_session.token, &fingerprint.addr)
            .await;
        stream_channel.shared = share_stream;
        let stream =
            create_active_client_stream(stream_details, app_state, user, connection_permission, fingerprint, stream_channel, Some(&user_session.token), req_headers)
                .await;

        let (status_code, header_map) =
            get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
        let mut response = axum::response::Response::builder().status(status_code);
        for (key, value) in &header_map {
            response = response.header(key, value);
        }

        let body_stream = prepare_body_stream(app_state, item_type, stream);
        debug_if_enabled!(
            "Streaming provider forced stream request from {}",
            sanitize_sensitive_info(&user_session.stream_url)
        );
        return try_unwrap_body!(response.body(body_stream));
    }

    app_state.connection_manager.release_provider_handle(stream_details.provider_handle).await;
    if let (Some(stream), _stream_info) = create_channel_unavailable_stream(
        &app_state.app_config,
        &[],
        StatusCode::SERVICE_UNAVAILABLE,
    ) {
        app_state.connection_manager.update_stream_detail(&fingerprint.addr, CustomVideoStreamType::ChannelUnavailable).await;
        debug!("Streaming custom stream");
        try_unwrap_body!(axum::response::Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from_stream(stream)))
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

/// # Panics
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    session_token: &str,
    mut stream_channel: StreamChannel,
    stream_url: &str,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
) -> impl IntoResponse + Send {
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(stream_url));
    }

    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        ).await
            .into_response();
    }

    let virtual_id = stream_channel.virtual_id;
    let item_type = stream_channel.item_type;

    let share_stream = is_stream_share_enabled(item_type, target);
    let _shared_lock = if share_stream {
        let write_lock = app_state.app_config.file_locks.write_lock_str(stream_url).await;

        if let Some(value) =
            try_shared_stream_response_if_any(app_state, stream_url, fingerprint, user, connection_permission, stream_channel.clone(), session_token, req_headers).await
        {
            return value.into_response();
        }
        Some(write_lock)
    } else {
        None
    };

    let stream_options = get_stream_options(app_state);
    let mut stream_details = create_stream_response_details(
        app_state,
        &stream_options,
        stream_url,
        fingerprint,
        req_headers,
        input,
        item_type,
        share_stream,
        connection_permission,
        None,
        stream_channel.virtual_id,
    ).await;

    if stream_details.has_stream() {
        // let content_length = get_stream_content_length(provider_response.as_ref());
        let provider_response = stream_details
            .stream_info
            .as_ref()
            .map(|(h, sc, response_url, cvt)| (h.clone(), *sc, response_url.clone(), *cvt));
        let provider_name = stream_details.provider_name.clone();

        if let Some((headers, status, _response_url, Some(CustomVideoStreamType::Provisioning))) =
            stream_details.stream_info.as_ref()
        {
            debug_if_enabled!(
                "panel_api provisioning response to client: status={} headers={:?}",
                status,
                headers
            );
        }

        let provider_handle = if share_stream {
            stream_details.provider_handle.take()
        } else {
            None
        };

        let mut is_stream_shared = share_stream;
        if let Some((_header, _status_code, _url, Some(_custom_video))) = stream_details.stream_info.as_ref() {
            if stream_details.stream.is_some() {
                is_stream_shared = false;
            }
        }

        stream_channel.shared = is_stream_shared;
        let stream =
            create_active_client_stream(stream_details, app_state, user, connection_permission, fingerprint, stream_channel, Some(session_token), req_headers)
                .await;
        let stream_resp = if is_stream_shared {
            debug_if_enabled!("Streaming shared stream request from {}",sanitize_sensitive_info(stream_url));
            // Shared Stream response
            let shared_headers = provider_response
                .as_ref()
                .map_or_else(Vec::new, |(h, _, _, _)| h.clone());

            if let Some((broadcast_stream, _shared_provider)) = SharedStreamManager::register_shared_stream(
                app_state,
                stream_url,
                stream,
                &fingerprint.addr,
                shared_headers,
                stream_options.buffer_size,
                provider_handle,
            )
                .await
            {
                let (status_code, header_map) =
                    get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
                let mut response = axum::response::Response::builder().status(status_code);
                for (key, value) in &header_map {
                    response = response.header(key, value);
                }
                try_unwrap_body!(response.body(axum::body::Body::from_stream(broadcast_stream)))
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        } else {
            // Previously, we would always check if the provider redirected the request.
            // If the provider redirected a movie from /movie/... to a temporary /live/... URL,
            // We would save that redirected URL in your session.
            // When we tried to seek or pause/resume, we would use that saved /live/ URL.
            // However, providers often make these redirect links ephemeral or restricted—they
            // might not support seeking, or they might trigger a 509 error if accessed again.
            // For Movies/Series: We now ignore the redirect and always save the original,
            // canonical URL (the one starting with /movie/) in your session.
            // This ensures that every time you seek, we start "fresh" with the correct provider handshake,
            // preventing the session from being "poisoned" by a temporary redirect.
            // For everything else (Live): It continues to work as before, using the redirected URL if available,
            // which is often desirable for live streams to stay on the same edge server.
            let session_url = if matches!(item_type, PlaylistItemType::Catchup | PlaylistItemType::Video | PlaylistItemType::LocalVideo | PlaylistItemType::Series | PlaylistItemType::LocalSeries) {
                Cow::Borrowed(stream_url)
            } else {
                provider_response
                    .as_ref()
                    .and_then(|(_, _, u, _)| u.as_ref())
                    .map_or_else(
                        || Cow::Borrowed(stream_url),
                        |url| Cow::Owned(url.to_string()),
                    )
            };
            if log_enabled!(log::Level::Debug) {
                if session_url.eq(&stream_url) {
                    debug!("Streaming stream request from {}", sanitize_sensitive_info(stream_url)
                    );
                } else {
                    debug!(
                        "Streaming stream request for {} from {}",
                        sanitize_sensitive_info(stream_url),
                        sanitize_sensitive_info(&session_url)
                    );
                }
            }
            let (status_code, header_map) =
                get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
            let mut response = axum::response::Response::builder().status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }

            if let Some(provider) = provider_name {
                if matches!(
                    item_type,
                    PlaylistItemType::LiveHls
                        | PlaylistItemType::LiveDash
                        | PlaylistItemType::Video
                        | PlaylistItemType::Series
                        | PlaylistItemType::LocalSeries
                        | PlaylistItemType::Catchup
                ) {
                    let _ = app_state
                        .active_users
                        .create_user_session(
                            user,
                            session_token,
                            virtual_id,
                            &provider,
                            &session_url,
                            &fingerprint.addr,
                            connection_permission,
                        )
                        .await;
                }
            }

            let body_stream = prepare_body_stream(app_state, item_type, stream);
            try_unwrap_body!(response.body(body_stream))
        };

        return stream_resp.into_response();
    }
    app_state.connection_manager.release_provider_handle(stream_details.provider_handle).await;
    StatusCode::BAD_REQUEST.into_response()
}

fn get_stream_throttle(app_state: &Arc<AppState>) -> u64 {
    app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map(|stream| stream.throttle_kbps)
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
async fn try_shared_stream_response_if_any(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    connect_permission: UserConnectionPermission,
    mut stream_channel: StreamChannel,
    session_token: &str,
    req_headers: &HeaderMap,
) -> Option<impl IntoResponse> {
    if let Some((stream, provider)) =
        SharedStreamManager::subscribe_shared_stream(app_state, stream_url, &fingerprint.addr).await
    {
        debug_if_enabled!("Using shared stream {}", sanitize_sensitive_info(stream_url)
        );
        if let Some(headers) = app_state
            .shared_stream_manager
            .get_shared_state_headers(stream_url)
            .await
        {
            let (status_code, header_map) = get_stream_response_with_headers(Some((
                headers.clone(),
                StatusCode::OK,
            )));
            let mut grace_period_options = app_state.get_grace_options();
            if connect_permission != UserConnectionPermission::GracePeriod {
                grace_period_options.period_millis = 0;
            }
            let mut stream_details = StreamDetails::from_stream(stream, grace_period_options);

            stream_details.provider_name = provider;
            stream_channel.shared = true;
            let stream =
                create_active_client_stream(stream_details, app_state, user, connect_permission, fingerprint, stream_channel, Some(session_token), req_headers)
                    .await
                    .boxed();
            let mut response = axum::response::Response::builder().status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }
            return response.body(axum::body::Body::from_stream(stream)).ok();
        }
    }
    None
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn local_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    pli: StreamChannel,
    req_headers: &HeaderMap,
    _input: &ConfigInput,
    _target: &ConfigTarget,
    _user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    check_path: bool,
) -> impl IntoResponse + Send {
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(&pli.url));
    }

    if connection_permission == UserConnectionPermission::Exhausted {
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        ).await.into_response();
    }

    let path = PathBuf::from(pli.url.strip_prefix("file://").unwrap_or(&pli.url));

    // Canonicalize and validate the path
    let path = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(err) => {
            error!("Local file path is corrupt {}: {err}", path.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    if check_path {
        let Some(library_paths) = app_state.app_config.config.load()
        .library.as_ref()
        .map(|lib| {
            lib.scan_directories
                .iter()
                .map(|dir| dir.path.clone())
                .collect::<Vec<_>>()
        })
        else {
            return StatusCode::NOT_FOUND.into_response();
        };

        // Verify path is within allowed media directories
        // (requires configuration of allowed base paths)
        if !is_path_within_allowed_directories(&path, &library_paths) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    let Ok(mut file) = tokio::fs::File::open(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(metadata) = file.metadata().await else { return internal_server_error!() };
    let file_size = metadata.len();

    let range = req_headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);

    let (start, end) = if let Some((req_start, req_end)) = range {
        if file_size == 0 || req_start >= file_size {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        let end = req_end.unwrap_or(file_size - 1).min(file_size - 1);
        if end < req_start {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        (req_start, end)
    } else {
        if file_size == 0 {
            // Serve empty file
            let body = axum::body::Body::empty();
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            if let Some(ext) = get_file_extension(&pli.url) {
                let ct = content_type_from_ext(&ext);
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            } else {
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            }
            headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            return response.into_response();
        }
        (0, file_size - 1)
    };

    let content_length = end - start + 1;

    if start > 0 {
        if let Err(_err) = file.seek(SeekFrom::Start(start)).await {
            return internal_server_error!();
        }
    }

    let stream = ReaderStream::new(file.take(content_length));
    let body_stream = prepare_body_stream::<_>(app_state, pli.item_type, stream.map_err(|err| StreamError::Stream(err.to_string())));

    let mut response = Response::new(body_stream);

    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let headers = response.headers_mut();
    if let Some(ext) = get_file_extension(&pli.url) {
        let ct = content_type_from_ext(&ext);
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    } else {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    }
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    if let Ok(header_value) = HeaderValue::from_str(&content_length.to_string()) {
        headers.insert(header::CONTENT_LENGTH, header_value);
    }

    if range.is_some() {
        if let Ok(header_value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{file_size}")) {
            headers.insert(header::CONTENT_RANGE, header_value);
        }
    }

    response
}

fn is_path_within_allowed_directories(sub_path: &Path, root_paths: &[String]) -> bool {
    for root_path in root_paths {
        if sub_path.starts_with(PathBuf::from(root_path)) {
            return true;
        }
    }
    false
}

pub fn is_stream_share_enabled(item_type: PlaylistItemType, target: &ConfigTarget) -> bool {
    (item_type == PlaylistItemType::Live/* || item_type == PlaylistItemType::LiveHls */)
        && target
        .options
        .as_ref()
        .is_some_and(|opt| opt.share_live_streams)
}

pub type HeaderFilter = Option<Box<dyn Fn(&str) -> bool + Send>>;
pub fn get_headers_from_request(
    req_headers: &HeaderMap,
    filter: &HeaderFilter,
) -> HashMap<String, Vec<u8>> {
    req_headers
        .iter()
        .filter(|(k, _)| match &filter {
            None => true,
            Some(predicate) => predicate(k.as_str()),
        })
        .map(|(k, v)| (k.as_str().to_string(), v.as_bytes().to_vec()))
        .collect()
}

fn get_add_cache_content(
    res_url: &str,
    mime_type: Option<String>,
    cache: &Arc<ArcSwapOption<Mutex<LRUResourceCache>>>,
) -> Arc<dyn Fn(usize) + Send + Sync> {
    let resource_url = String::from(res_url);
    let cache = Arc::clone(cache);
    let add_cache_content: Arc<dyn Fn(usize) + Send + Sync> = Arc::new(move |size| {
        let res_url = resource_url.clone();
        let mime_type = mime_type.clone();
        // todo spawn, replace with unboundchannel
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            if let Some(cache) = cache.load().as_ref() {
                let _ = cache.lock().await.add_content(&res_url, mime_type, size);
            }
        });
    });
    add_cache_content
}

fn get_mime_type(headers: &HeaderMap, resource_url: &str) -> Option<String> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())   // Option<&str>
        .map(ToString::to_string)        // Option<String>
        .or_else(|| {
            // fallback to guess
            mime_guess::from_path(resource_url)
                .first_raw()
                .map(ToString::to_string)
        })
}

async fn build_resource_stream_response(
    app_state: &Arc<AppState>,
    resource_url: &str,
    response: reqwest::Response,
) -> axum::response::Response {
    let sanitized_resource_url = sanitize_sensitive_info(resource_url);
    let status = response.status();
    let mut response_builder =
        axum::response::Response::builder().status(status);
    let mime_type = get_mime_type(response.headers(), resource_url);
    let has_content_range = response.headers().contains_key(header::CONTENT_RANGE);
    for (key, value) in response.headers() {
        let name = key.as_str();
        let is_hop_by_hop = matches!(
            name.to_ascii_lowercase().as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        );
        if !is_hop_by_hop {
            response_builder = response_builder.header(key, value);
        }
    }

    if !response_builder.headers_ref().is_some_and(|h| h.contains_key(header::CACHE_CONTROL)) {
        response_builder = response_builder.header(header::CACHE_CONTROL, "public, max-age=14400");
    }

    let byte_stream = response
        .bytes_stream()
        .map_err(|err| StreamError::reqwest(&err));
    // Cache only complete responses (200 OK without Content-Range)
    let can_cache = status == StatusCode::OK && !has_content_range;
    if can_cache {
        debug!( "Caching eligible resource stream {sanitized_resource_url}");
        let cache_resource_path = if let Some(cache) = app_state.cache.load().as_ref() {
            Some(cache.lock().await.store_path(resource_url, mime_type.as_deref()))
        } else {
            None
        };
        if let Some(resource_path) = cache_resource_path {
            match create_new_file_for_write(&resource_path).await {
                Ok(file) => {
                    debug!("Persisting resource stream {sanitized_resource_url} to {}", resource_path.display());
                    let writer = async_file_writer(file);
                    let add_cache_content = get_add_cache_content(resource_url, mime_type, &app_state.cache);
                    let tee = tee_stream(byte_stream, writer, &resource_path, add_cache_content);
                    return try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(tee)));
                }
                Err(err) => {
                    warn!("Failed to create cache file {} for {sanitized_resource_url}: {err}", resource_path.display());
                }
            }
        } else {
            debug!("Resource cache unavailable; streaming response for {sanitized_resource_url} without persistence");
        }
    }

    try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(byte_stream)))
}

async fn fetch_resource_with_retry(
    app_state: &Arc<AppState>,
    url: &Url,
    resource_url: &str,
    req_headers: &HashMap<String, Vec<u8>>,
    input: Option<&ConfigInput>,
) -> Option<axum::response::Response> {
    let config = app_state.app_config.config.load();
    let default_user_agent = config.default_user_agent.clone();
    drop(config);

    let disabled_headers = app_state.get_disabled_headers();

    let Ok(response) = send_with_retry(
        &app_state.app_config,
        url,
        || {
            request::get_client_request(
                &app_state.http_client.load(),
                input.map_or(InputFetchMethod::GET, |i| i.method),
                input.map(|i| &i.headers),
                url,
                Some(req_headers),
                disabled_headers.as_ref(),
                default_user_agent.as_deref(),
            )
        },
    ).await else { return None };

    let status = response.status();

    if status.is_success() {
        return Some(
            build_resource_stream_response(app_state, resource_url, response).await,
        );
    }

    // Non-retriable Status → Upstream Response incl. Body
    debug_if_enabled!(
        "Failed to open resource got status {status} for {}",
        sanitize_sensitive_info(resource_url)
    );

    let mut response_builder = axum::response::Response::builder().status(status);
    for (key, value) in response.headers() {
        response_builder = response_builder.header(key, value);
    }

    let stream = response
        .bytes_stream()
        .map_err(|err| StreamError::reqwest(&err));

    Some(try_unwrap_body!(
        response_builder.body(axum::body::Body::from_stream(stream))
    ))
}


/// # Panics
pub async fn resource_response(
    app_state: &Arc<AppState>,
    resource_url: &str,
    req_headers: &HeaderMap,
    input: Option<&ConfigInput>,
) -> impl IntoResponse + Send {
    if resource_url.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }
    let filter: HeaderFilter = Some(Box::new(|key| {
        key != "if-none-match" && key != "if-modified-since"
    }));
    let req_headers = get_headers_from_request(req_headers, &filter);
    if let Some(cache) = app_state.cache.load().as_ref() {
        let mut guard = cache.lock().await;
        if let Some((resource_path, mime_type)) = guard.get_content(resource_url) {
            trace_if_enabled!("Responding resource from cache {}", sanitize_sensitive_info(resource_url));
            return serve_file(&resource_path, mime_type.unwrap_or_else(|| mime::APPLICATION_OCTET_STREAM.to_string()), Some("public, max-age=14400"))
                .await
                .into_response();
        }
    }
    trace_if_enabled!(
        "Try to fetch resource {}",
        sanitize_sensitive_info(resource_url)
    );
    if let Ok(url) = Url::parse(resource_url) {
        if let Some(resp) =
            fetch_resource_with_retry(app_state, &url, resource_url, &req_headers, input).await {
            return resp;
        }
        // Upstream failure after retries
        return StatusCode::BAD_GATEWAY.into_response();
    }
    error!("Url is malformed {}", sanitize_sensitive_info(resource_url));
    StatusCode::BAD_REQUEST.into_response()
}

pub fn separate_number_and_remainder(input: &str) -> (String, Option<String>) {
    input.rfind('.').map_or_else(
        || (input.to_string(), None),
        |dot_index| {
            let number_part = input[..dot_index].to_string();
            let rest = input[dot_index..].to_string();
            (number_part, if rest.len() < 2 { None } else { Some(rest) })
        },
    )
}

/// # Panics
pub fn empty_json_list_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
        .body("[]".to_owned()))
}

pub fn get_username_from_auth_header(token: &str, app_state: &Arc<AppState>) -> Option<String> {
    if let Some(web_auth_config) = &app_state
        .app_config
        .config
        .load()
        .web_ui
        .as_ref()
        .and_then(|c| c.auth.as_ref())
    {
        let secret_key: &[u8] = web_auth_config.secret.as_ref();
        if let Ok(token_data) = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret_key),
            &Validation::new(Algorithm::HS256),
        ) {
            return Some(token_data.claims.username);
        }
    }
    None
}

/// # Panics
pub fn redirect(url: &str) -> impl IntoResponse {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .body(axum::body::Body::empty()))
}

pub async fn is_seek_request(cluster: XtreamCluster, req_headers: &HeaderMap) -> bool {
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
        if range.starts_with("bytes=") {
            return true;
        }
    }
    false
}

pub fn bin_response<T: Serialize>(data: &T) -> impl IntoResponse + Send {
    match bin_serialize(data) {
        Ok(body) => (
            [(header::CONTENT_TYPE, CONTENT_TYPE_CBOR)],
            body,
        ).into_response(),
        Err(_) => internal_server_error!(),
    }
}

pub fn json_response<T: Serialize>(data: &T) -> impl IntoResponse + Send {
    (StatusCode::OK, axum::Json(data)).into_response()
}

pub fn json_or_bin_response<T: Serialize>(accept: Option<&str>, data: &T) -> impl IntoResponse + Send {
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return bin_response(data).into_response();
    }
    json_response(data).into_response()
}

pub fn stream_json_or_bin_response<P>(accept: Option<&str>, data: Box<dyn Iterator<Item=P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array(data);
    }
    stream_json_array(data)
}

pub fn create_session_fingerprint(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    concat_string!(&fingerprint.key, "|", username, "|", &virtual_id.to_string())
}

pub fn stream_json_array<P>(iter: Box<dyn Iterator<Item=P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold(
        (iter, true),
        |(mut iter, first)| async move {
            match iter.next() {
                Some(item) => {
                    let mut json = String::new();
                    if !first {
                        json.push(',');
                    }
                    let element = serde_json::to_string(&item).ok()?;
                    json.push_str(&element);
                    Some((Ok::<Bytes, Infallible>(Bytes::from(json)), (iter, false)))
                }
                None => None,
            }
        },
    );

    let body = Body::from_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async {
                Ok::<_, Infallible>(Bytes::from_static(b"]"))
            })),
    );

    try_unwrap_body!(Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(body))
}

pub fn stream_bin_array<P>(iter: Box<dyn Iterator<Item=P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold(
        iter,
        |mut iter| async move {
            match iter.next() {
                Some(item) => {
                    match bin_serialize(&item) {
                        Ok(buf) => Some((Ok::<Bytes, Infallible>(Bytes::from(buf)), iter)),
                        Err(err) => {
                            warn!("CBOR serialization error in stream: {err}");
                            Some((Ok::<Bytes, Infallible>(Bytes::new()), iter)) // skip errors, continue
                        }
                    }
                }
                None => None,
            }
        },
    );

    let body = Body::from_stream(
        stream::once(async {
            // CBOR: start indefinite-length array
            Ok::<_, Infallible>(Bytes::from_static(&[0x9f]))
        })
            .chain(stream)
            .chain(stream::once(async {
                // CBOR: end indefinite-length array
                Ok::<_, Infallible>(Bytes::from_static(&[0xff]))
            })),
    );

    try_unwrap_body!(Response::builder()
        .header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR)
        .body(body))
}

pub fn create_api_proxy_user(app_state: &Arc<AppState>) -> ProxyUserCredentials {
    let config = app_state.app_config.config.load();

    let server = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());

    ProxyUserCredentials {
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
    }
}

pub fn empty_json_response_as_object() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            mime::APPLICATION_JSON.to_string(),
        )
        .body(axum::body::Body::from("{}".as_bytes()))
}

pub fn empty_json_response_as_array() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            mime::APPLICATION_JSON.to_string(),
        )
        .body(axum::body::Body::from("[]".as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use shared::model::XtreamCluster;

    #[tokio::test]
    async fn test_is_seek_request() {
        let mut headers = HeaderMap::new();

        // No range header
        assert!(!is_seek_request(XtreamCluster::Video, &headers).await);

        // Range: bytes=0- (Should be true now to allow session takeover on restart)
        headers.insert("range", "bytes=0-".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers).await);

        // Range: bytes=100- (Should be true)
        headers.insert("range", "bytes=100-".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers).await);

        // Range: bytes=100-200 (Should be true)
        headers.insert("range", "bytes=100-200".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers).await);

        // Live cluster should always return false
        headers.insert("range", "bytes=100-".parse().unwrap());
        assert!(!is_seek_request(XtreamCluster::Live, &headers).await);
    }
}
