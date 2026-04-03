use crate::{
    api::{
        endpoints::xtream_api::{get_xtream_player_api_stream_url, ApiStreamContext},
        model::{
            create_active_client_stream, create_channel_unavailable_stream, create_custom_video_stream_response,
            create_provider_connections_exhausted_stream, create_provider_stream, get_stream_response_with_headers,
            tee_stream, AppState, CustomVideoStreamType, ProviderAllocation, ProviderConfig, ProviderStreamFactoryOptions,
            ProviderStreamState, SharedStreamManager, StreamDetails, StreamError, StreamingStrategy, ThrottledStream,
            UserApiRequest, UserSession,
        },
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
    repository::{ConnectFailureReason, FailureStage},
    utils::{
        async_file_reader, async_file_writer, create_new_file_for_write, debug_if_enabled, get_file_extension, request,
        request::{content_type_from_ext, parse_range, send_with_retry_and_provider},
        trace_if_enabled,
    },
    BUILD_TIMESTAMP,
};
use arc_swap::ArcSwapOption;
use axum::{
    body::Body,
    http::{header, Extensions, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{stream, Stream, StreamExt, TryStreamExt};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use log::{debug, error, info, log_enabled, trace, warn};
use serde::Serialize;
use shared::{
    concat_string,
    model::{
        Claims, InputFetchMethod, PlaylistEntry, PlaylistItemType, ProxyType, StreamChannel, StreamInfo, TargetType,
        UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        bin_serialize, extract_extension_from_url, human_readable_kbps, is_sanitize_sensitive_info_enabled,
        replace_url_extension, sanitize_sensitive_info,
        trim_slash, Internable, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON, DASH_EXT, HLS_EXT,
    },
};
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::Mutex,
};
use tokio_util::io::ReaderStream;
use url::Url;

pub(crate) fn resolve_request_url_for_logging<'a>(input: &ConfigInput, stream_url: &'a str) -> Cow<'a, str> {
    if is_sanitize_sensitive_info_enabled() {
        return Cow::Borrowed(stream_url);
    }

    let provider = input.get_resolve_provider(stream_url);
    if let Ok(url) = Url::parse(stream_url) {
        return Cow::Owned(request::preview_request_target_for_logging(&url, provider.as_ref()));
    }

    input
        .resolve_url(stream_url)
        .ok()
        .and_then(|resolved| {
            Url::parse(resolved.as_ref())
                .ok()
                .map(|url| Cow::Owned(request::preview_request_target_for_logging(&url, provider.as_ref())))
        })
        .unwrap_or(Cow::Borrowed(stream_url))
}

pub(crate) struct ConnectFailedAttempt<'a> {
    pub app_state: &'a Arc<AppState>,
    pub fingerprint: &'a Fingerprint,
    pub user: &'a ProxyUserCredentials,
    pub stream_channel: StreamChannel,
    pub provider_name: &'a str,
    pub req_headers: &'a HeaderMap,
    pub reason: ConnectFailureReason,
    pub failure_stage: FailureStage,
}

pub(crate) fn record_connect_failed_attempt(attempt: ConnectFailedAttempt<'_>) {
    let user_agent = attempt
        .req_headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let info = StreamInfo::new(
        0,
        0,
        &attempt.user.username,
        &attempt.fingerprint.addr,
        &attempt.fingerprint.client_ip,
        attempt.provider_name,
        attempt.stream_channel,
        user_agent,
        None,
        None,
    );
    attempt
        .app_state
        .connection_manager
        .record_connect_failed(&info, attempt.reason, attempt.failure_stage);
}

fn admission_failure_video_type(reason: ConnectFailureReason) -> Option<CustomVideoStreamType> {
    match reason {
        ConnectFailureReason::UserAccountExpired => Some(CustomVideoStreamType::UserAccountExpired),
        ConnectFailureReason::UserConnectionsExhausted => Some(CustomVideoStreamType::UserConnectionsExhausted),
        ConnectFailureReason::ProviderConnectionsExhausted => Some(CustomVideoStreamType::ProviderConnectionsExhausted),
        _ => None,
    }
}

pub(crate) fn admission_failure_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    stream_channel: StreamChannel,
    provider_name: &str,
    req_headers: &HeaderMap,
    reason: ConnectFailureReason,
) -> axum::response::Response {
    record_connect_failed_attempt(ConnectFailedAttempt {
        app_state,
        fingerprint,
        user,
        stream_channel,
        provider_name,
        req_headers,
        reason,
        failure_stage: FailureStage::Admission,
    });
    let Some(video_type) = admission_failure_video_type(reason) else {
        error!("Unsupported admission failure reason: {reason:?}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    create_custom_video_stream_response(app_state, &fingerprint.addr, video_type).into_response()
}

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
macro_rules! try_option_forbidden {
    ($option:expr, $status:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Some(value) => value,
            None => {
                if $msg_is_error {
                    error!("{}", $msg);
                } else {
                    debug!("{}", $msg);
                }
                return $status.into_response();
            }
        }
    };
    ($option:expr, $msg_is_error:expr, $msg:expr) => {
        match $option {
            Some(value) => value,
            None => {
                if $msg_is_error {
                    error!("{}", $msg);
                } else {
                    debug!("{}", $msg);
                }
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
        }
    };
    ($option:expr) => {
        match $option {
            Some(value) => value,
            None => return axum::http::StatusCode::FORBIDDEN.into_response(),
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
        $body
            .map_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(), |resp| resp.into_response())
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

use crate::api::panel_api::{can_provision_on_exhausted, create_panel_api_provisioning_stream_details};
pub use internal_server_error;
use shared::error::TuliproxError;
use shared::utils::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs};
pub use try_option_bad_request;
pub use try_option_forbidden;
pub use try_result_bad_request;
pub use try_result_not_found;
pub use try_result_or_status;
pub use try_unwrap_body;
use crate::utils::LRUResourceCache;

pub fn get_server_time() -> String {
    chrono::offset::Local::now().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

pub fn get_build_time() -> Option<String> {
    BUILD_TIMESTAMP
        .to_string()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DisableResponseCompression;

pub(crate) fn mark_response_as_uncompressed<B>(response: &mut Response<B>) {
    response.extensions_mut().insert(DisableResponseCompression);
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn should_compress_response<B>(response: &Response<B>) -> bool {
    should_compress_response_extensions(response.extensions())
}

pub(crate) fn should_compress_response_extensions(extensions: &Extensions) -> bool {
    extensions.get::<DisableResponseCompression>().is_none()
}

#[derive(Clone, Copy, Debug, Default)]
struct StreamMeteringConfig {
    meter_uid: u32,
    meter_stream: bool,
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
            let last_modified = file.metadata().await.ok().and_then(|m| m.modified().ok()).map(|m| {
                let dt: DateTime<Utc> = m.into();
                dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
            });

            let reader = async_file_reader(file);
            let stream = tokio_util::io::ReaderStream::new(reader);
            let body = axum::body::Body::from_stream(stream);

            let mut builder = axum::response::Response::builder()
                .status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime_type)
                .header(axum::http::header::CACHE_CONTROL, cache_control.unwrap_or("no-cache"));

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

struct StreamingAcquireOptions<'a> {
    force_provider: Option<&'a Arc<str>>,
    allow_provider_grace: bool,
    user_priority: i8,
    connection_kind: crate::api::model::ConnectionKind,
    session_owner: Option<&'a str>,
}

pub(crate) fn connection_priority_for_kind(user: &ProxyUserCredentials, kind: crate::api::model::ConnectionKind) -> i8 {
    match kind {
        crate::api::model::ConnectionKind::Normal => user.priority,
        crate::api::model::ConnectionKind::Soft => user.soft_priority,
    }
}

pub struct ForceStreamRequestContext<'a> {
    pub req_headers: &'a HeaderMap,
    pub input: &'a Arc<ConfigInput>,
    pub user: &'a ProxyUserCredentials,
    pub session_reservation_ttl_secs: u64,
}

/// Constructs a `StreamOptions` object based on the application's reverse proxy configuration.
///
/// This function retrieves streaming-related settings from the `AppState`:
/// - `stream_retry`: whether retrying the stream is enabled,
/// - `buffer_enabled`: whether stream buffering is enabled,
/// - `buffer_size`: the size of the stream buffer.
///
/// If the reverse proxy or stream settings are not defined, default values are used:
/// - retry: `true`
/// - buffering: `false`
/// - buffer size: `0`
///
/// Additionally, it computes `pipe_provider_stream` as `!stream_retry && !buffer_enabled`.
/// This means direct provider piping is enabled only when retry is disabled and buffering is disabled.
///
/// Returns a `StreamOptions` instance with the resolved configuration.
pub(in crate::api) fn get_stream_options(app_state: &Arc<AppState>) -> StreamOptions {
    let (stream_retry, buffer_enabled, buffer_size) = app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or((true, false, 0), |stream| {
            let (buffer_enabled, buffer_size) =
                stream.buffer.as_ref().map_or((false, 0), |buffer| (buffer.enabled, buffer.size));
            (stream.retry, buffer_enabled, buffer_size)
        });
    let pipe_provider_stream = !stream_retry && !buffer_enabled;
    StreamOptions { stream_retry, buffer_enabled, buffer_size, pipe_provider_stream }
}

pub fn get_stream_alternative_url(stream_url: &str, input: &ConfigInput, alias_input: &Arc<ProviderConfig>) -> String {
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
        if let Some(provider_cfg) = app_state.active_provider.get_next_provider(&input.name).await {
            let mut new_url = redirect_url.replacen(base_url, provider_cfg.url.as_str(), 1);
            if let (Some(old_username), Some(old_password)) = (username, password) {
                if let (Some(new_username), Some(new_password)) =
                    (provider_cfg.username.as_ref(), provider_cfg.password.as_ref())
                {
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
    options: StreamingAcquireOptions<'_>,
) -> StreamingStrategy {
    // allocate a provider connection
    let mut forced_provider_allocated = false;
    let provider_connection_handle = match options.force_provider {
        Some(provider) => {
            // First try to stay on the exact pinned provider account without over-allocating.
            // If that account is no longer available, fall back to any available account in the same lineup.
            if let Some(handle) = app_state
                .active_provider
                .acquire_exact_connection_with_grace_for_session(
                    provider,
                    &fingerprint.addr,
                    options.allow_provider_grace,
                    options.user_priority,
                    options.connection_kind,
                    options.session_owner,
                )
                .await
            {
                forced_provider_allocated = true;
                Some(handle)
            } else {
                debug_if_enabled!(
                    "Pinned provider {} unavailable for {}; falling back to lineup allocation",
                    sanitize_sensitive_info(provider),
                    sanitize_sensitive_info(&fingerprint.addr.to_string())
                );
                app_state
                    .active_provider
                    .acquire_connection_with_grace_for_session(
                        &input.name,
                        &fingerprint.addr,
                        options.allow_provider_grace,
                        options.user_priority,
                        options.connection_kind,
                        options.session_owner,
                    )
                    .await
            }
        }
        None => {
            app_state
                .active_provider
                .acquire_connection_with_grace_for_session(
                    &input.name,
                    &fingerprint.addr,
                    options.allow_provider_grace,
                    options.user_priority,
                    options.connection_kind,
                    options.session_owner,
                )
                .await
        }
    };

    // panel_api provisioning/loading is handled later in the stream creation flow

    let stream_response_params = if let Some(allocation) = provider_connection_handle.as_ref().map(|ph| &ph.allocation)
    {
        match allocation {
            ProviderAllocation::Exhausted => {
                debug!("Provider {} is exhausted. No connections allowed.", input.name);
                let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
                ProviderStreamState::Custom(stream)
            }
            ProviderAllocation::Available(ref provider_cfg) | ProviderAllocation::GracePeriod(ref provider_cfg) => {
                // force_stream_provider means we keep the url and the provider.
                // If force_stream_provider or the input is the same as the config we don't need to get new url
                let (selected_provider_name, url) = if forced_provider_allocated || provider_cfg.id == input.id {
                    (input.name.clone(), stream_url.to_string())
                } else {
                    (provider_cfg.name.clone(), get_stream_alternative_url(stream_url, input, provider_cfg))
                };

                debug_if_enabled!(
                    "provider session: input={} provider_cfg={} user={} allocation={} stream_url={}",
                    sanitize_sensitive_info(&input.name),
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(
                        provider_cfg.get_user_info().as_ref().map_or_else(|| "?", |u| u.username.as_str())
                    ),
                    allocation.short_key(),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, &url).as_ref())
                );

                if matches!(allocation, ProviderAllocation::Available(_)) {
                    ProviderStreamState::Available(Some(selected_provider_name.intern()), url.intern())
                } else {
                    ProviderStreamState::GracePeriod(Some(selected_provider_name.intern()), url.intern())
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
    username: &str,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    stream_channel: &StreamChannel,
    item_type: PlaylistItemType,
    share_stream: bool,
    connection_permission: UserConnectionPermission,
    force_provider: Option<&Arc<str>>,
    allow_provider_grace: bool,
    virtual_id: VirtualId,
    user_priority: i8,
    connection_kind: crate::api::model::ConnectionKind,
    session_owner: Option<&str>,
) -> Result<StreamDetails, TuliproxError> {
    let mut streaming_strategy = resolve_streaming_strategy(
        app_state,
        stream_url,
        fingerprint,
        input,
        StreamingAcquireOptions {
            force_provider,
            allow_provider_grace,
            user_priority,
            connection_kind,
            session_owner,
        },
    )
    .await;
    let mut grace_period_options = app_state.get_grace_options();
    grace_period_options.period_millis = get_grace_period_millis(
        connection_permission,
        &streaming_strategy.provider_stream_state,
        grace_period_options.period_millis,
    );
    let provider_grace_active = matches!(
        streaming_strategy.provider_stream_state,
        ProviderStreamState::GracePeriod(_, _)
    );

    let guard_provider_name =
        streaming_strategy.provider_handle.as_ref().and_then(|guard| guard.allocation.get_provider_name());

    if matches!(streaming_strategy.provider_stream_state, ProviderStreamState::Custom(_))
        && can_provision_on_exhausted(app_state, input)
    {
        if let Some(handle) = streaming_strategy.provider_handle.take() {
            app_state.connection_manager.release_provider_handle(Some(handle)).await;
        }
        debug_if_enabled!(
            "panel_api: provider connections exhausted; sending provisioning stream for input {}",
            sanitize_sensitive_info(&input.name)
        );
        return Ok(create_panel_api_provisioning_stream_details(
            app_state,
            input,
            guard_provider_name.clone(),
            &grace_period_options,
            fingerprint.addr,
            virtual_id,
        ));
    }

    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom(provider_stream) => {
            let (stream, stream_info) = provider_stream;
            Ok(StreamDetails {
                stream,
                stream_info,
                provider_name: guard_provider_name.clone(),
                request_url: None,
                grace_period: grace_period_options,
                provider_grace_active: false,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: streaming_strategy.provider_handle.clone(),
            })
        }
        ProviderStreamState::Available(_provider_name, request_url)
        | ProviderStreamState::GracePeriod(_provider_name, request_url) => {
            debug_if_enabled!(
                "Provider stream selection: allocated_provider={} actual_request_url={}",
                sanitize_sensitive_info(guard_provider_name.as_deref().unwrap_or("?")),
                sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
            );
            let defer_provider_stream_until_grace_check = if provider_grace_active && grace_period_options.hold_stream {
                if let Some(provider_name) = guard_provider_name.as_ref() {
                    app_state.active_provider.is_over_limit(provider_name).await
                } else {
                    false
                }
            } else {
                false
            };
            let (stream, stream_info, reconnect_flag) = if defer_provider_stream_until_grace_check {
                debug_if_enabled!(
                    "Deferring provider stream open until grace check completes for {}",
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
                );
                (None, None, None)
            } else {
                let parsed_url = Url::parse(&request_url);
                let ((stream, stream_info), reconnect_flag) = if let Ok(url) = parsed_url {
                    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
                    let disabled_headers = app_state.get_disabled_headers();
                    let mut provider_stream_factory_options = ProviderStreamFactoryOptions::new(
                        &crate::api::model::ProviderStreamFactoryParams {
                            addr: fingerprint.addr,
                            item_type,
                            share_stream,
                            stream_options,
                            stream_url: &url,
                            req_headers,
                            input_headers: streaming_strategy.input_headers.as_ref(),
                            disabled_headers: disabled_headers.as_ref(),
                            default_user_agent: default_user_agent.as_deref(),
                            username: Some(username),
                            client_ip: Some(&fingerprint.client_ip),
                            stream_channel: Some(stream_channel),
                            connect_failure_stage: Some(FailureStage::ProviderOpen),
                        },
                    );

                    let provider_config = input.get_resolve_provider(url.as_ref());
                    provider_stream_factory_options.set_provider(provider_config);

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
                (stream, stream_info, reconnect_flag)
            };

            if log_enabled!(log::Level::Debug) {
                if let Some((headers, status_code, response_url, _custom_video_type)) = stream_info.as_ref() {
                    debug!(
                        "Responding stream request {} with status {}, headers {:?}",
                        sanitize_sensitive_info(response_url.as_ref().map_or(stream_url, |s| s.as_str())),
                        status_code,
                        headers
                    );
                }
            }

            // If no upstream stream is ready, release the provider unless provider grace
            // intentionally deferred the open until the grace check resolves.
            let provider_handle = if stream.is_none() && !defer_provider_stream_until_grace_check {
                let provider_handle = streaming_strategy.provider_handle.take();
                app_state.connection_manager.release_provider_handle(provider_handle).await;
                error!("Can't open stream {}", sanitize_sensitive_info(&request_url));
                None
            } else {
                streaming_strategy.provider_handle.take()
            };

            Ok(StreamDetails {
                stream,
                stream_info,
                provider_name: guard_provider_name.clone(),
                request_url: Some(request_url.clone()),
                grace_period: grace_period_options,
                provider_grace_active,
                disable_provider_grace: false,
                reconnect_flag,
                provider_handle,
            })
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
        let extension =
            self.stream_ext.map_or_else(|| extract_extension_from_url(url).unwrap_or_default(), ToString::to_string);

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

    let redirect_request = params.user.proxy.is_redirect(item_type) || params.target.is_force_redirect(item_type);
    let is_hls_request = item_type == PlaylistItemType::LiveHls || params.stream_ext == Some(HLS_EXT);
    let is_dash_request =
        (!is_hls_request && item_type == PlaylistItemType::LiveDash) || params.stream_ext == Some(DASH_EXT);

    if params.target_type == TargetType::M3u {
        if redirect_request || is_dash_request {
            let redirect_url: Arc<str> = if is_hls_request {
                replace_url_extension(&provider_url, HLS_EXT).into()
            } else {
                provider_url.clone()
            };
            let redirect_url =
                if is_dash_request { replace_url_extension(&redirect_url, DASH_EXT).into() } else { redirect_url };
            let redirect_url = get_redirect_alternative_url(app_state, &redirect_url, params.input).await;
            debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(&redirect_url));
            return Some(redirect(&redirect_url).into_response());
        }
    } else if params.target_type == TargetType::Xtream {
        let Some(provider_id) = params.provider_id else {
            return Some(StatusCode::BAD_REQUEST.into_response());
        };

        if redirect_request {
            let target_name = params.target.name.as_str();
            let virtual_id = params.item.get_virtual_id();
            let stream_url = match get_xtream_player_api_stream_url(
                params.input,
                params.req_context,
                &params.get_query_path(provider_id, &provider_url),
                &provider_url,
            ) {
                None => {
                    error!(
                        "Can't find stream url for target {target_name}, context {}, stream_id {virtual_id}",
                        params.req_context
                    );
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
                Some(url) => match app_state.active_provider.get_next_provider(&params.input.name).await {
                    Some(provider_cfg) => get_stream_alternative_url(&url, params.input, &provider_cfg),
                    None => url.to_string(),
                },
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
                    sanitize_sensitive_info(resolve_request_url_for_logging(params.input, redirect_url).as_ref())
                );
                return Some(redirect(redirect_url).into_response());
            }

            debug_if_enabled!(
                "Redirecting stream request to {}",
                sanitize_sensitive_info(resolve_request_url_for_logging(params.input, &stream_url).as_ref())
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

fn prepare_body_stream<S>(app_state: &Arc<AppState>, item_type: PlaylistItemType, stream: S) -> axum::body::Body
where
    S: futures::Stream<Item = Result<bytes::Bytes, StreamError>> + Send + 'static,
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
#[allow(clippy::too_many_lines)]
pub async fn force_provider_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    mut stream_channel: StreamChannel,
    ctx: ForceStreamRequestContext<'_>,
) -> impl IntoResponse + Send {
    let stream_options = get_stream_options(app_state);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;
    let item_type = stream_channel.item_type;

    // Release the existing provider connection for this session before acquiring a new one.
    // This is critical for users with a connection limit of 1 to avoid "Provider exhausted" or provider-side 502/509 errors during seeking.
    app_state.connection_manager.release_provider_connection(&user_session.addr).await;

    // Keep seek/range reconnects on the same provider account whenever that exact account is still available.
    // If it is not available anymore, resolve_streaming_strategy will fall back to another free account.
    let preferred_provider = Some(&user_session.provider);
    // Never allow provider-side grace for forced seek/session reacquire.
    // Over-allocation here would break provider-side one-connection limits.
    let allow_provider_grace = false;
    let connection_kind = user_session
        .connection_kind
        .unwrap_or(crate::api::model::ConnectionKind::Normal);

    let stream_details = match create_stream_response_details(
        app_state,
        &stream_options,
        &user_session.stream_url,
        &ctx.user.username,
        fingerprint,
        ctx.req_headers,
        ctx.input,
        &stream_channel,
        item_type,
        share_stream,
        connection_permission,
        preferred_provider,
        allow_provider_grace,
        stream_channel.virtual_id,
        connection_priority_for_kind(ctx.user, connection_kind),
        connection_kind,
        Some(user_session.token.as_str()),
    )
    .await
    {
        Ok(stream_details) => stream_details,
        Err(err) => {
            error!("Failed to stream: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let deferred_grace_hold_stream = stream_details.has_deferred_provider_open();

    if stream_details.has_stream() || deferred_grace_hold_stream {
        let metering = prepare_stream_metering(
            app_state,
            user_session.stream_url.as_ref(),
            share_stream,
            stream_details.stream.is_some(),
            stream_details.has_deferred_provider_open(),
        )
        .await;
        let provider_response =
            stream_details.stream_info.as_ref().map(|(h, sc, url, cvt)| (h.clone(), *sc, url.clone(), *cvt));
        if ctx.session_reservation_ttl_secs > 0 {
            if let Some(provider_name) = stream_details.provider_name.as_ref() {
                app_state
                    .active_provider
                    .refresh_provider_reservation(provider_name, &user_session.token, ctx.session_reservation_ttl_secs)
                    .await;
            }
        }
        app_state
            .active_users
            .update_session_addr(&ctx.user.username, &user_session.token, &fingerprint.addr)
            .await;
        stream_channel.shared = share_stream;
        let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user: ctx.user,
            connection_permission,
            connection_kind: user_session
                .connection_kind
                .unwrap_or(crate::api::model::ConnectionKind::Normal),
            fingerprint,
            stream_channel,
            session_token: Some(&user_session.token),
            req_headers: ctx.req_headers,
            meter_uid: metering.meter_uid,
            meter_stream: metering.meter_stream,
        })
        .await;

        let (status_code, header_map) = get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
        let mut response = axum::response::Response::builder().status(status_code);
        for (key, value) in &header_map {
            response = response.header(key, value);
        }

        let body_stream = prepare_body_stream(app_state, item_type, stream);
        debug_if_enabled!(
            "Streaming provider forced stream request from {}",
            sanitize_sensitive_info(resolve_request_url_for_logging(ctx.input, user_session.stream_url.as_ref()).as_ref())
        );
        let mut response = try_unwrap_body!(response.body(body_stream));
        mark_response_as_uncompressed(&mut response);
        return response;
    }

    app_state.connection_manager.release_provider_handle(stream_details.provider_handle).await;
    if let (Some(stream), _stream_info) =
        create_channel_unavailable_stream(&app_state.app_config, &[], StatusCode::SERVICE_UNAVAILABLE)
    {
        app_state
            .connection_manager
            .update_stream_detail(&fingerprint.addr, CustomVideoStreamType::ChannelUnavailable)
            .await;
        debug!("Streaming custom stream");
        let mut response = try_unwrap_body!(axum::response::Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from_stream(stream)));
        mark_response_as_uncompressed(&mut response);
        response
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
    input: &Arc<ConfigInput>,
    target: &Arc<ConfigTarget>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    allow_exhausted_shared_reconnect: bool,
) -> impl IntoResponse + Send {
    let request_log_stream_url = resolve_request_url_for_logging(input, stream_url);
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(request_log_stream_url.as_ref()));
    }

    let virtual_id = stream_channel.virtual_id;
    let item_type = stream_channel.item_type;
    let allow_shared_reuse = connection_permission != UserConnectionPermission::Exhausted || allow_exhausted_shared_reconnect;

    let share_stream = is_stream_share_enabled(item_type, target);
    let _shared_lock = if share_stream {
        let write_lock = app_state.app_config.file_locks.write_lock_str(stream_url).await;

        if allow_shared_reuse {
            if let Some(value) = try_shared_stream_response_if_any(
                app_state,
                stream_url,
                fingerprint,
                user,
                connection_permission,
                connection_kind,
                stream_channel.clone(),
                session_token,
                req_headers,
            )
            .await
            {
                return value.into_response();
            }
        }
        Some(write_lock)
    } else {
        // Opportunistic cross-target sharing: if another target already runs a shared stream
        // for the same provider URL, subscribe to it instead of opening a separate connection.
        if item_type == PlaylistItemType::Live && allow_shared_reuse {
            if let Some(value) = try_shared_stream_response_if_any(
                app_state,
                stream_url,
                fingerprint,
                user,
                connection_permission,
                connection_kind,
                stream_channel.clone(),
                session_token,
                req_headers,
            )
            .await
            {
                debug_if_enabled!(
                    "Opportunistic shared stream reuse for {}",
                    sanitize_sensitive_info(stream_url)
                );
                return value.into_response();
            }
        }
        None
    };

    if connection_permission == UserConnectionPermission::Exhausted {
        record_connect_failed_attempt(ConnectFailedAttempt {
            app_state,
            fingerprint,
            user,
            stream_channel: stream_channel.clone(),
            provider_name: input.name.as_ref(),
            req_headers,
            reason: ConnectFailureReason::UserConnectionsExhausted,
            failure_stage: FailureStage::Admission,
        });
        return create_custom_video_stream_response(
            app_state,
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        )
        .into_response();
    }

    let stream_options = get_stream_options(app_state);
    let mut stream_details = match create_stream_response_details(
        app_state,
        &stream_options,
        stream_url,
        &user.username,
        fingerprint,
        req_headers,
        input,
        &stream_channel,
        item_type,
        share_stream,
        connection_permission,
        None,
        true,
        stream_channel.virtual_id,
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
        Some(session_token),
    )
    .await
    {
        Ok(stream_details) => stream_details,
        Err(err) => {
            error!("Failed to stream: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // When no provider stream is available, still create an ActiveClientStream if a grace period
    // needs to resolve (provider-grace with hold_stream, or user-grace). The grace task will
    // determine the correct mode (UserExhausted / ProviderExhausted / Inner) and serve the
    // appropriate custom video or terminate cleanly.
    let deferred_grace_hold_stream =
        stream_details.has_deferred_provider_open() || connection_permission == UserConnectionPermission::GracePeriod;

    if stream_details.has_stream() || deferred_grace_hold_stream {
        // let content_length = get_stream_content_length(provider_response.as_ref());
        let provider_response = stream_details
            .stream_info
            .as_ref()
            .map(|(h, sc, response_url, cvt)| (h.clone(), *sc, response_url.clone(), *cvt));
        let provider_name = stream_details.provider_name.clone();
        let actual_request_url = stream_details.request_url.clone().unwrap_or_else(|| Arc::<str>::from(stream_url));
        let log_actual_request_url = resolve_request_url_for_logging(input, actual_request_url.as_ref());

        debug_if_enabled!(
            "Provider request mapping: allocated_provider={} actual_request_url={}",
            sanitize_sensitive_info(provider_name.as_deref().unwrap_or("?")),
            sanitize_sensitive_info(log_actual_request_url.as_ref())
        );

        if let Some((headers, status, _response_url, Some(CustomVideoStreamType::Provisioning))) =
            stream_details.stream_info.as_ref()
        {
            debug_if_enabled!("panel_api provisioning response to client: status={} headers={:?}", status, headers);
        }

        let metering = prepare_stream_metering(
            app_state,
            stream_url,
            share_stream,
            stream_details.stream.is_some(),
            stream_details.has_deferred_provider_open(),
        )
        .await;

        let mut is_stream_shared = share_stream && !deferred_grace_hold_stream;
        if let Some((_header, _status_code, _url, Some(_custom_video))) = stream_details.stream_info.as_ref() {
            if stream_details.stream.is_some() {
                is_stream_shared = false;
            }
        }
        let provider_handle = if is_stream_shared && !stream_details.has_deferred_provider_open() {
            stream_details.provider_handle.take()
        } else {
            None
        };

        stream_channel.shared = is_stream_shared;
        if is_stream_shared {
            stream_channel.shared_joined_existing = Some(false);
            stream_channel.shared_stream_id = Some(u64::from(metering.meter_uid));
        } else {
            stream_channel.shared_joined_existing = None;
            stream_channel.shared_stream_id = None;
        }
        let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user,
            connection_permission,
            connection_kind,
            fingerprint,
            stream_channel,
            session_token: Some(session_token),
            req_headers,
            meter_uid: metering.meter_uid,
            meter_stream: metering.meter_stream,
        })
        .await;
        let stream_resp = if is_stream_shared {
            debug_if_enabled!(
                "Streaming shared stream request from {}",
                sanitize_sensitive_info(log_actual_request_url.as_ref())
            );
            // Shared Stream response
            let shared_headers = provider_response.as_ref().map_or_else(Vec::new, |(h, _, _, _)| h.clone());
            if let Some((broadcast_stream, _shared_provider)) = SharedStreamManager::register_shared_stream(
                app_state,
                stream_url,
                stream,
                &fingerprint.addr,
                shared_headers,
                stream_options.buffer_size,
                provider_handle,
                connection_priority_for_kind(user, connection_kind),
                connection_kind,
            )
            .await
            {
                let (status_code, header_map) =
                    get_stream_response_with_headers(provider_response.map(|(h, s, _, _)| (h, s)));
                let mut response = axum::response::Response::builder().status(status_code);
                for (key, value) in &header_map {
                    response = response.header(key, value);
                }
                let mut response = try_unwrap_body!(response.body(axum::body::Body::from_stream(broadcast_stream)));
                mark_response_as_uncompressed(&mut response);
                response
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
            let session_url: Cow<'_, str> = if matches!(
                item_type,
                PlaylistItemType::Catchup
                    | PlaylistItemType::Video
                    | PlaylistItemType::LocalVideo
                    | PlaylistItemType::Series
                    | PlaylistItemType::LocalSeries
            ) {
                Cow::Owned(actual_request_url.to_string())
            } else {
                provider_response
                    .as_ref()
                    .and_then(|(_, _, u, _)| u.as_ref())
                    .map_or_else(|| Cow::Owned(actual_request_url.to_string()), |url| Cow::Owned(url.to_string()))
            };
            let log_session_url = resolve_request_url_for_logging(input, session_url.as_ref());
            if log_enabled!(log::Level::Debug) {
                if log_session_url.eq(log_actual_request_url.as_ref()) {
                    debug!("Streaming stream request from {}", sanitize_sensitive_info(log_actual_request_url.as_ref()));
                } else {
                    debug!(
                        "Streaming stream request for {} from {}",
                        sanitize_sensitive_info(log_actual_request_url.as_ref()),
                        sanitize_sensitive_info(log_session_url.as_ref())
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
                        .create_user_session(crate::api::model::CreateUserSessionParams {
                            user,
                            session_token,
                            virtual_id,
                            provider: &provider,
                            stream_url: &session_url,
                            addr: &fingerprint.addr,
                            connection_permission,
                            connection_kind: Some(connection_kind),
                        })
                        .await;
                    let reservation_ttl_secs = get_session_reservation_ttl_secs(app_state, item_type);
                    if reservation_ttl_secs > 0 {
                        app_state
                            .active_provider
                            .refresh_provider_reservation(&provider, session_token, reservation_ttl_secs)
                            .await;
                    }
                }
            }

            let body_stream = prepare_body_stream(app_state, item_type, stream);
            let mut response = try_unwrap_body!(response.body(body_stream));
            mark_response_as_uncompressed(&mut response);
            response
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

fn is_stream_metrics_enabled(app_state: &Arc<AppState>) -> bool {
    app_state
        .app_config
        .config
        .load()
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .is_some_and(|stream| stream.metrics_enabled)
}

async fn prepare_stream_metering(
    app_state: &Arc<AppState>,
    stream_url: &str,
    share_stream: bool,
    has_stream: bool,
    has_deferred_provider_open: bool,
) -> StreamMeteringConfig {
    if !is_stream_metrics_enabled(app_state) {
        return StreamMeteringConfig::default();
    }

    if share_stream && !has_stream && !has_deferred_provider_open {
        return StreamMeteringConfig {
            meter_uid: app_state.shared_stream_manager.get_meter_uid(stream_url).await.unwrap_or(0),
            meter_stream: false,
        };
    }

    if has_stream || has_deferred_provider_open {
        let meter_uid = app_state.connection_manager.next_stream_uid();
        if share_stream {
            app_state.shared_stream_manager.register_meter_uid(stream_url, meter_uid).await;
        }
        return StreamMeteringConfig {
            meter_uid,
            meter_stream: true,
        };
    }

    StreamMeteringConfig::default()
}

fn resolve_stream_config_u64(
    stream_config: Option<&crate::model::StreamConfig>,
    selector: impl FnOnce(&crate::model::StreamConfig) -> u64,
    default_value: u64,
) -> u64 {
    stream_config.map_or(default_value, selector)
}

fn get_stream_config_u64(app_state: &Arc<AppState>, selector: impl FnOnce(&crate::model::StreamConfig) -> u64, default_value: u64) -> u64 {
    let config = app_state.app_config.config.load();
    let stream_config = config.reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.stream.as_ref());
    resolve_stream_config_u64(stream_config, selector, default_value)
}

pub(crate) fn get_hls_session_ttl_secs(app_state: &Arc<AppState>) -> u64 {
    get_stream_config_u64(app_state, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs())
}

pub(crate) fn get_catchup_session_ttl_secs(app_state: &Arc<AppState>) -> u64 {
    get_stream_config_u64(app_state, |stream| stream.catchup_session_ttl_secs, default_catchup_session_ttl_secs())
}

pub(crate) fn get_session_reservation_ttl_secs(app_state: &Arc<AppState>, item_type: PlaylistItemType) -> u64 {
    match item_type {
        PlaylistItemType::LiveHls | PlaylistItemType::LiveDash => get_hls_session_ttl_secs(app_state),
        PlaylistItemType::Catchup => get_catchup_session_ttl_secs(app_state),
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_shared_stream_response_if_any(
    app_state: &Arc<AppState>,
    stream_url: &str,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    connect_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    mut stream_channel: StreamChannel,
    session_token: &str,
    req_headers: &HeaderMap,
) -> Option<impl IntoResponse> {
    if connect_permission == UserConnectionPermission::GracePeriod {
        return None;
    }

    if let Some((stream, provider)) = SharedStreamManager::subscribe_shared_stream(
        app_state,
        stream_url,
        &fingerprint.addr,
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
    )
    .await
    {
        debug_if_enabled!("Using shared stream {}", sanitize_sensitive_info(stream_url));
        if let Some(headers) = app_state.shared_stream_manager.get_shared_state_headers(stream_url).await {
            let (status_code, header_map) = get_stream_response_with_headers(Some((headers.clone(), StatusCode::OK)));
            let mut grace_period_options = app_state.get_grace_options();
            if connect_permission != UserConnectionPermission::GracePeriod {
                grace_period_options.period_millis = 0;
            }
            let mut stream_details = StreamDetails::from_stream(stream, grace_period_options);

            stream_details.provider_name = provider;
            if let Some(provider_name) = stream_details.provider_name.as_deref() {
                let _ = app_state
                    .active_users
                    .create_user_session(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token,
                        virtual_id: stream_channel.virtual_id,
                        provider: provider_name,
                        stream_url,
                        addr: &fingerprint.addr,
                        connection_permission: connect_permission,
                        connection_kind: Some(connection_kind),
                    })
                    .await;
            }
            stream_channel.shared = true;
            stream_channel.shared_joined_existing = Some(true);
            stream_channel.shared_stream_id = app_state.shared_stream_manager.get_meter_uid(stream_url).await.map(u64::from);
            let metering = StreamMeteringConfig {
                meter_uid: app_state.shared_stream_manager.get_meter_uid(stream_url).await.unwrap_or(0),
                meter_stream: false,
            };
            let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
                stream_details,
                app_state,
                user,
                connection_permission: connect_permission,
                connection_kind,
                fingerprint,
                stream_channel,
                session_token: Some(session_token),
                req_headers,
                meter_uid: metering.meter_uid,
                meter_stream: metering.meter_stream,
            })
            .await
            .boxed();
            let mut response = axum::response::Response::builder().status(status_code);
            for (key, value) in &header_map {
                response = response.header(key, value);
            }
            let mut response = response.body(axum::body::Body::from_stream(stream)).ok()?;
            mark_response_as_uncompressed(&mut response);
            return Some(response);
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
    input: &ConfigInput,
    _target: &ConfigTarget,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    playback_session_token: Option<&str>,
    check_path: bool,
) -> impl IntoResponse + Send {
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(&pli.url));
    }

    if connection_permission == UserConnectionPermission::Exhausted {
        let allow_session_reopen = if let Some(session_token) = playback_session_token {
            user.max_connections > 0
                && app_state
                    .active_users
                    .connection_permission_for_session(
                        &user.username,
                        user.max_connections,
                        user.soft_connections,
                        session_token,
                    )
                    .await
                    != UserConnectionPermission::Exhausted
        } else {
            false
        };
        if !allow_session_reopen {
            record_connect_failed_attempt(ConnectFailedAttempt {
                app_state,
                fingerprint,
                user,
                stream_channel: pli.clone(),
                provider_name: input.name.as_ref(),
                req_headers,
                reason: ConnectFailureReason::UserConnectionsExhausted,
                failure_stage: FailureStage::Admission,
            });
            return create_custom_video_stream_response(
                app_state,
                &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            )
            .into_response();
        }
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
        let Some(library_paths) = app_state
            .app_config
            .config
            .load()
            .library
            .as_ref()
            .map(|lib| lib.scan_directories.iter().map(|dir| dir.path.clone()).collect::<Vec<_>>())
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

    let range = req_headers.get("range").and_then(|v| v.to_str().ok()).and_then(parse_range);

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

    let stream = ReaderStream::new(file.take(content_length))
        .map_err(|err| StreamError::Stream(err.to_string()))
        .boxed();
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let stream = if is_throttled_stream(pli.item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        ThrottledStream::new(stream, throttle_kbps).boxed()
    } else {
        stream
    };
    let mut grace_period_options = app_state.get_grace_options();
    if connection_permission != UserConnectionPermission::GracePeriod {
        grace_period_options.period_millis = 0;
    }
    let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
        stream_details: StreamDetails::from_stream(stream, grace_period_options),
        app_state,
        user,
        connection_permission,
        connection_kind,
        fingerprint,
        stream_channel: pli.clone(),
        session_token: playback_session_token,
        req_headers,
        meter_uid: 0,
        meter_stream: false,
    })
    .await;

    let mut response = Response::new(axum::body::Body::from_stream(stream));

    *response.status_mut() = if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };

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

    mark_response_as_uncompressed(&mut response);
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
        && target.options.as_ref().is_some_and(|opt| opt.share_live_streams)
}

pub type HeaderFilter = Option<Box<dyn Fn(&str) -> bool + Send>>;
pub fn get_headers_from_request(req_headers: &HeaderMap, filter: &HeaderFilter) -> HashMap<String, Vec<u8>> {
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
        .and_then(|v| v.to_str().ok()) // Option<&str>
        .map(ToString::to_string) // Option<String>
        .or_else(|| {
            // fallback to guess
            mime_guess::from_path(resource_url).first_raw().map(ToString::to_string)
        })
}

async fn build_resource_stream_response(
    app_state: &Arc<AppState>,
    resource_url: &str,
    response: reqwest::Response,
) -> axum::response::Response {
    let sanitized_resource_url = sanitize_sensitive_info(resource_url);
    let status = response.status();
    let mut response_builder = axum::response::Response::builder().status(status);
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

    let byte_stream = response.bytes_stream().map_err(|err| StreamError::reqwest(&err));
    // Cache only complete responses (200 OK without Content-Range)
    let can_cache = status == StatusCode::OK && !has_content_range;
    if can_cache {
        debug!("Caching eligible resource stream {sanitized_resource_url}");
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
                    warn!(
                        "Failed to create cache file {} for {sanitized_resource_url}: {err}",
                        resource_path.display()
                    );
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

    let provider_config = input.and_then(|i| i.get_resolve_provider(url.as_str()));
    let Ok(response) =
        send_with_retry_and_provider(&app_state.app_config, url, provider_config.as_ref(), false, |resolved_url| {
            request::get_client_request(
                &app_state.http_client.load(),
                input.map_or(InputFetchMethod::GET, |i| i.method),
                input.map(|i| &i.headers),
                resolved_url,
                Some(req_headers),
                disabled_headers.as_ref(),
                default_user_agent.as_deref(),
            )
        })
        .await
    else {
        return None;
    };

    let status = response.status();

    if status.is_success() {
        return Some(build_resource_stream_response(app_state, resource_url, response).await);
    }

    // Non-retriable Status → Upstream Response incl. Body
    debug_if_enabled!("Failed to open resource got status {status} for {}", sanitize_sensitive_info(resource_url));

    let mut response_builder = axum::response::Response::builder().status(status);
    for (key, value) in response.headers() {
        response_builder = response_builder.header(key, value);
    }

    let stream = response.bytes_stream().map_err(|err| StreamError::reqwest(&err));

    Some(try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(stream))))
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
    let filter: HeaderFilter = Some(Box::new(|key| key != "if-none-match" && key != "if-modified-since"));
    let req_headers = get_headers_from_request(req_headers, &filter);
    if let Some(cache) = app_state.cache.load().as_ref() {
        let mut guard = cache.lock().await;
        if let Some((resource_path, mime_type)) = guard.get_content(resource_url) {
            trace_if_enabled!("Responding resource from cache {}", sanitize_sensitive_info(resource_url));
            return serve_file(
                &resource_path,
                mime_type.unwrap_or_else(|| mime::APPLICATION_OCTET_STREAM.to_string()),
                Some("public, max-age=14400"),
            )
            .await
            .into_response();
        }
    }
    trace_if_enabled!("Try to fetch resource {}", sanitize_sensitive_info(resource_url));
    if let Ok(url) = Url::parse(resource_url) {
        if let Some(resp) = fetch_resource_with_retry(app_state, &url, resource_url, &req_headers, input).await {
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
    if let Some(web_auth_config) = &app_state.app_config.config.load().web_ui.as_ref().and_then(|c| c.auth.as_ref()) {
        let secret_key: &[u8] = web_auth_config.secret.as_ref();
        if let Ok(token_data) =
            decode::<Claims>(token, &DecodingKey::from_secret(secret_key), &Validation::new(Algorithm::HS256))
        {
            return Some(token_data.claims.username);
        }
    }
    None
}

pub fn redirect(url: &str) -> impl IntoResponse {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .body(Body::empty()))
}

pub async fn is_seek_request(cluster: XtreamCluster, req_headers: &HeaderMap) -> bool {
    // seek only for non-live streams
    if cluster == XtreamCluster::Live {
        return false;
    }

    // seek requests contains range header
    let range = req_headers.get("range").and_then(|h| h.to_str().ok()).map(ToString::to_string);

    if let Some(range) = range {
        if range.starts_with("bytes=") {
            return true;
        }
    }
    false
}

pub fn bin_response<T: Serialize>(data: &T) -> impl IntoResponse + Send {
    match bin_serialize(data) {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE_CBOR)], body).into_response(),
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

pub fn stream_json_or_bin_response<P>(
    accept: Option<&str>,
    data: Box<dyn Iterator<Item = P> + Send>,
) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array(data);
    }
    stream_json_array(data)
}

pub fn stream_json_or_bin_response_stream<P, S>(accept: Option<&str>, data: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array_stream(data);
    }
    stream_json_array_stream(data)
}

pub fn create_session_fingerprint(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    concat_string!(&fingerprint.key, "|", username, "|", &virtual_id.to_string())
}

pub fn create_catchup_session_key(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    concat_string!("catchup|", &fingerprint.key, "|", username, "|", &virtual_id.to_string(), "|session")
}

pub(crate) fn should_allow_exhausted_shared_reconnect(
    share_stream: bool,
    user_session: Option<&UserSession>,
    requested_virtual_id: u32,
    requested_stream_url: &str,
) -> bool {
    share_stream
        && user_session.is_some_and(|session| {
            session.permission != UserConnectionPermission::Exhausted
                && session.virtual_id == requested_virtual_id
                && session.stream_url.as_ref() == requested_stream_url
        })
}

pub fn stream_json_array<P>(iter: Box<dyn Iterator<Item = P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold((iter, true), |(mut iter, first)| async move {
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
    });

    let body = Body::from_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    );

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

pub fn stream_bin_array<P>(iter: Box<dyn Iterator<Item = P> + Send>) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
{
    let stream = stream::unfold(iter, |mut iter| async move {
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
    });

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

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

pub fn stream_json_array_stream<P, S>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    let stream = stream::unfold((stream, true), |(mut stream, first)| async move {
        match stream.next().await {
            Some(item) => {
                let mut json = String::new();
                if !first {
                    json.push(',');
                }
                let element = serde_json::to_string(&item).ok()?;
                json.push_str(&element);
                Some((Ok::<Bytes, Infallible>(Bytes::from(json)), (stream, false)))
            }
            None => None,
        }
    });

    let body = Body::from_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    );

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

pub fn stream_bin_array_stream<P, S>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = P> + Send + Unpin + 'static,
{
    let stream = stream::unfold(stream, |mut stream| async move {
        match stream.next().await {
            Some(item) => match bin_serialize(&item) {
                Ok(buf) => Some((Ok::<Bytes, Infallible>(Bytes::from(buf)), stream)),
                Err(err) => {
                    warn!("CBOR serialization error in stream: {err}");
                    Some((Ok::<Bytes, Infallible>(Bytes::new()), stream))
                }
            },
            None => None,
        }
    });

    let body = Body::from_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0x9f])) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0xff])) })),
    );

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
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
        epg_request_timeshift: None,
        created_at: None,
        exp_date: None,
        max_connections: 0,
        status: None,
        ui_enabled: false,
        comment: None,
        priority: 0,
        soft_connections: 0,
        soft_priority: 0,
        t_is_api_user: true,
    }
}

pub fn empty_json_response_as_object() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
        .body(axum::body::Body::from("{}".as_bytes()))
}

pub fn empty_json_response_as_array() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
        .body(axum::body::Body::from("[]".as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Response, StatusCode};
    use crate::{
        api::model::{
            AppState, CancelTokens, ActiveProviderManager, ActiveUserManager, ConnectionManager, EventManager, MetadataUpdateManager,
            PlaylistStorageState, SharedStreamManager,
        },
        auth::Fingerprint,
        model::{AppConfig, Config, ConfigInput, ConfigTarget, MediaToolCapabilities, ProcessTargets, ProxyUserCredentials, SourcesConfig},
        utils::{GeoIp, FileLockManager},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use bytes::Bytes;
    use futures::stream;
    use shared::{
        foundation::Filter,
        model::{ConfigPaths, ConfigTargetOptions, InputFetchMethod, InputType, PlaylistItemType, ProcessingOrder, StreamChannel, XtreamCluster},
        utils::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs, Internable},
    };
    use std::{borrow::Cow, collections::HashMap, sync::Arc};
    use tokio::sync::mpsc;
    use crate::model::StreamHistoryConfig;

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

    #[test]
    fn test_streaming_response_extension_disables_compression() {
        let mut response = Response::new(());
        mark_response_as_uncompressed(&mut response);

        assert!(!should_compress_response(&response));
    }

    #[test]
    fn test_regular_response_keeps_compression_enabled() {
        let response = Response::new(());

        assert!(should_compress_response(&response));
    }

    #[test]
    fn test_get_stream_config_u64_uses_default_when_stream_config_missing() {
        assert_eq!(
            resolve_stream_config_u64(
                None,
                |stream| stream.hls_session_ttl_secs,
                default_hls_session_ttl_secs()
            ),
            default_hls_session_ttl_secs()
        );
        assert_eq!(
            resolve_stream_config_u64(
                None,
                |stream| stream.catchup_session_ttl_secs,
                default_catchup_session_ttl_secs()
            ),
            default_catchup_session_ttl_secs()
        );
    }

    #[tokio::test]
    async fn test_get_session_reservation_ttl_secs_uses_hls_ttl_for_live_dash() {
        let app_state = create_test_app_state();
        assert_eq!(
            get_session_reservation_ttl_secs(&app_state, PlaylistItemType::LiveDash),
            default_hls_session_ttl_secs()
        );
    }

    #[test]
    fn test_should_allow_exhausted_shared_reconnect_only_for_matching_shared_session() {
        let session = UserSession {
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            token: "tok".to_string(),
            virtual_id: 282,
            provider: Arc::<str>::from("provider"),
            stream_url: Arc::<str>::from("http://provider/live/449924.ts"),
            addr: "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!()),
            ts: 1,
            permission: UserConnectionPermission::Allowed,
        };

        assert!(should_allow_exhausted_shared_reconnect(
            true,
            Some(&session),
            282,
            "http://provider/live/449924.ts"
        ));
        assert!(!should_allow_exhausted_shared_reconnect(
            false,
            Some(&session),
            282,
            "http://provider/live/449924.ts"
        ));
        assert!(!should_allow_exhausted_shared_reconnect(
            true,
            Some(&session),
            999,
            "http://provider/live/449924.ts"
        ));
        assert!(!should_allow_exhausted_shared_reconnect(
            true,
            Some(&session),
            282,
            "http://provider/live/other.ts"
        ));
    }

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "local_media".intern(),
            input_type: InputType::Library,
            headers: HashMap::default(),
            url: "file:///tmp".to_string(),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_provider_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_app_state() -> Arc<AppState> {
        create_test_app_state_for_config(Arc::new(create_test_app_config()))
    }

    fn create_test_provider_app_state() -> Arc<AppState> {
        create_test_app_state_for_config(Arc::new(create_test_provider_app_config()))
    }

    fn create_test_app_state_for_config(app_cfg: Arc<AppConfig>) -> Arc<AppState> {
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        let history_config = Some(StreamHistoryConfig::default());
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager =
            Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, history_config.as_ref()));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<Arc<ProcessTargets>>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: crate::api::model::UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    fn create_test_fingerprint(addr: std::net::SocketAddr) -> Fingerprint {
        Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr)
    }

    fn create_test_local_channel(url: &str) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id: 41,
            provider_id: 0,
            input_name: "library".intern(),
            item_type: PlaylistItemType::LocalVideo,
            cluster: XtreamCluster::Video,
            group: "Local Movies".intern(),
            title: "Local Test".intern(),
            url: url.into(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
        }
    }

    fn create_test_live_channel(url: &str) -> StreamChannel {
        StreamChannel {
            target_id: 1,
            virtual_id: 42,
            provider_id: 1,
            input_name: "provider_1".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "Live".intern(),
            title: "Shared Live".intern(),
            url: url.into(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
        }
    }

    fn create_test_shared_target() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "shared".to_string(),
            options: Some(ConfigTargetOptions {
                share_live_streams: true,
                ..ConfigTargetOptions::default()
            }),
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        }
    }

    #[test]
    fn admission_failure_reason_maps_to_custom_video_type() {
        assert!(matches!(
            admission_failure_video_type(ConnectFailureReason::UserAccountExpired),
            Some(CustomVideoStreamType::UserAccountExpired)
        ));
        assert!(matches!(
            admission_failure_video_type(ConnectFailureReason::UserConnectionsExhausted),
            Some(CustomVideoStreamType::UserConnectionsExhausted)
        ));
        assert!(matches!(
            admission_failure_video_type(ConnectFailureReason::ProviderConnectionsExhausted),
            Some(CustomVideoStreamType::ProviderConnectionsExhausted)
        ));
        assert!(admission_failure_video_type(ConnectFailureReason::ProviderError).is_none());
    }

    #[tokio::test]
    async fn local_stream_response_registers_active_local_stream() {
        let app_state = create_test_app_state();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let addr = "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let user = ProxyUserCredentials::default();
        let target = ConfigTarget {
            id: 1,
            enabled: true,
            name: "test".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        };

        let _response = local_stream_response(
            &fingerprint,
            &app_state,
            channel,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            None,
            false,
        )
        .await
        .into_response();

        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1, "local file streaming should register an active stream");
        assert_eq!(active_streams[0].channel.item_type, PlaylistItemType::LocalVideo);
    }

    #[tokio::test]
    async fn stream_response_preserves_soft_kind_for_shared_reuse() {
        let app_state = create_test_provider_app_state();
        let stream_url = "http://provider-1.example/live/shared.ts";
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .get_input_by_name(&input_name)
            .expect("provider input should exist");
        let target = Arc::new(create_test_shared_target());

        let owner_addr = "127.0.0.1:55140".parse().unwrap_or_else(|_| unreachable!());
        let owner_handle = app_state
            .active_provider
            .acquire_connection(&input.name, &owner_addr, 0, crate::api::model::ConnectionKind::Normal)
            .await
            .expect("owner allocation should exist");
        let shared_stream = stream::pending::<Result<Bytes, std::io::Error>>();
        let registered = SharedStreamManager::register_shared_stream(
            app_state.as_ref(),
            stream_url,
            shared_stream,
            &owner_addr,
            Vec::new(),
            1,
            Some(owner_handle),
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .await;
        assert!(registered.is_some(), "shared stream should register");

        let mut user = ProxyUserCredentials::default();
        user.username = "soft-user".to_string();
        user.max_connections = 1;
        user.soft_connections = 1;
        user.priority = 0;
        user.soft_priority = 9;

        let normal_addr = "127.0.0.1:55141".parse().unwrap_or_else(|_| unreachable!());
        let normal_fingerprint = create_test_fingerprint(normal_addr);
        let normal_channel = create_test_live_channel("http://provider-1.example/live/normal.ts");
        app_state.active_users.add_connection(&normal_addr).await;
        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 1001,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &normal_fingerprint,
                provider: input.name.as_ref(),
                stream_channel: &normal_channel,
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("normal-session"),
            })
            .await
            .expect("normal stream should register");

        let admission = app_state
            .get_connection_admission(&user.username, user.max_connections, user.soft_connections)
            .await;
        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(admission.kind, Some(crate::api::model::ConnectionKind::Soft));

        let soft_addr = "127.0.0.1:55142".parse().unwrap_or_else(|_| unreachable!());
        let soft_fingerprint = create_test_fingerprint(soft_addr);
        let response = stream_response(
            &soft_fingerprint,
            &app_state,
            "soft-session",
            create_test_live_channel(stream_url),
            stream_url,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            admission.permission,
            admission.kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            false,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let session_admission = app_state
            .active_users
            .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, "soft-session")
            .await;
        assert_eq!(session_admission.kind, Some(crate::api::model::ConnectionKind::Soft));
    }

    #[tokio::test]
    async fn local_stream_response_disables_response_compression() {
        let app_state = create_test_app_state();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let addr = "127.0.0.1:55124".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let user = ProxyUserCredentials::default();
        let target = ConfigTarget {
            id: 1,
            enabled: true,
            name: "test".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        };

        let response = local_stream_response(
            &fingerprint,
            &app_state,
            channel,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            None,
            false,
        )
        .await
        .into_response();

        assert!(!should_compress_response(&response));
    }

    #[tokio::test]
    async fn local_stream_response_reuses_stable_playback_session_token_across_reopens() {
        let app_state = create_test_app_state();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let user = ProxyUserCredentials::default();
        let target = ConfigTarget {
            id: 1,
            enabled: true,
            name: "test".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        };
        let playback_session_token = "local-playback-token";

        let first_fingerprint = create_test_fingerprint("127.0.0.1:55125".parse().unwrap_or_else(|_| unreachable!()));
        let second_fingerprint = create_test_fingerprint("127.0.0.1:55126".parse().unwrap_or_else(|_| unreachable!()));

        let _first_response = local_stream_response(
            &first_fingerprint,
            &app_state,
            channel.clone(),
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            Some(playback_session_token),
            false,
        )
        .await
        .into_response();

        let _second_response = local_stream_response(
            &second_fingerprint,
            &app_state,
            channel,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            Some(playback_session_token),
            false,
        )
        .await
        .into_response();

        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1, "stable playback token should reuse the tracked local connection");
        assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
        assert_eq!(active_streams[0].addr, second_fingerprint.addr);
    }

    #[tokio::test]
    async fn local_stream_response_allows_exhausted_reopen_for_same_playback_session_token() {
        let app_state = create_test_app_state();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let mut user = ProxyUserCredentials::default();
        user.username = "user1".to_string();
        user.max_connections = 1;
        let target = ConfigTarget {
            id: 1,
            enabled: true,
            name: "test".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        };
        let playback_session_token = "local-playback-token";

        let first_fingerprint = create_test_fingerprint("127.0.0.1:55127".parse().unwrap_or_else(|_| unreachable!()));
        let second_fingerprint = create_test_fingerprint("127.0.0.1:55128".parse().unwrap_or_else(|_| unreachable!()));

        let _first_response = local_stream_response(
            &first_fingerprint,
            &app_state,
            channel.clone(),
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            Some(playback_session_token),
            false,
        )
        .await
        .into_response();

        let second_response = local_stream_response(
            &second_fingerprint,
            &app_state,
            channel,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Exhausted,
            crate::api::model::ConnectionKind::Normal,
            Some(playback_session_token),
            false,
        )
        .await
        .into_response();

        assert_eq!(second_response.status(), StatusCode::OK);

        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1);
        assert_eq!(active_streams[0].session_token.as_deref(), Some(playback_session_token));
        assert_eq!(active_streams[0].addr, second_fingerprint.addr);
    }
}
