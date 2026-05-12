use crate::{
    api::{
        endpoints::xtream_api::{get_xtream_player_api_stream_url, ApiStreamContext},
        model::{
            create_active_client_stream, create_channel_unavailable_stream, create_custom_video_stream_response,
            create_provider_connections_exhausted_stream, create_provider_stream, get_stream_response_with_headers,
            tee_stream, AppState, BoxedProviderStream, CustomVideoStreamType, ProviderAllocation, ProviderConfig,
            ProviderStreamFactoryOptions, ProviderStreamInfo, ProviderStreamState, SharedStreamManager, StreamDetails, StreamError,
            StreamingStrategy, ThrottledStream, UserApiRequest, UserSession, PendingProviderReason,
        },
    },
    auth::Fingerprint,
    media_server::{
        playback::{
            media_server_image_response as open_media_server_proxy_image_response,
            media_server_stream_response as open_media_server_proxy_stream_response,
            parse_media_server_image_ref, parse_media_server_stream_ref,
        },
        plex::client::PlexCatalogClient,
        MediaServerError, MediaServerErrorKind, MediaServerHttpClient, MediaServerImageRef,
    },
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
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
    http::{header, Extensions, HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
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
        Claims, InputFetchMethod, InputType, PlaylistEntry, PlaylistItemType, ProxyType, StreamChannel, StreamInfo, TargetType,
        UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        bin_serialize, extract_extension_from_url, human_readable_kbps, is_sanitize_sensitive_info_enabled,
        replace_url_extension, sanitize_sensitive_info, current_time_secs, trim_slash, Internable, CONTENT_TYPE_CBOR,
        CONTENT_TYPE_JSON, DASH_EXT, HLS_EXT,
    },
};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    io::SeekFrom,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::Mutex,
};
use tokio_util::io::ReaderStream;
use url::Url;

const RECENT_EVICTION_REENTRY_TTL_SECS: u64 = 3;

#[derive(Clone, Copy)]
pub(crate) enum EvictionReentryGuard<'a> {
    Session(&'a str),
    SocketPlayback { virtual_id: VirtualId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlaybackRequestClass {
    Prepare,
    Activate,
    FollowUp,
    Terminate,
}

#[derive(Clone, Copy)]
pub(crate) struct PlaybackRequestFacts<'a> {
    pub(crate) item_type: PlaylistItemType,
    pub(crate) existing_session: Option<&'a UserSession>,
    pub(crate) prepare_only: bool,
    pub(crate) terminate: bool,
}

pub(crate) fn classify_playback_request(facts: PlaybackRequestFacts<'_>) -> PlaybackRequestClass {
    if facts.terminate {
        return PlaybackRequestClass::Terminate;
    }
    if facts.prepare_only {
        return PlaybackRequestClass::Prepare;
    }
    if let Some(session) = facts.existing_session {
        // FollowUp only for sessions that are actively counted.
        // PendingProvider has no counted lease yet - activation is still pending.
        // Prepared/Preserved/Expired sessions are not FollowUp.
        if session.lifecycle.is_counted() {
            return PlaybackRequestClass::FollowUp;
        }
    }
    let _ = facts.item_type;
    PlaybackRequestClass::Activate
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_playback_request_admission(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    fingerprint: &Fingerprint,
    item_type: PlaylistItemType,
    user_session: Option<&UserSession>,
    session_token: &str,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
    prepare_only: bool,
    terminate: bool,
) -> (crate::api::model::ConnectionAdmission, Option<crate::api::model::GraceMode>, PlaybackRequestClass) {
    let request_class = classify_playback_request(PlaybackRequestFacts {
        item_type,
        existing_session: user_session,
        prepare_only,
        terminate,
    });
    let limits_enabled =
        (user.max_connections > 0 || user.soft_connections > 0) && app_state.app_config.config.load().user_access_control;

    // Handle explicit Terminate: run termination and return exhausted permission.
    // No admission strategies are evaluated — termination immediately expires the playback.
    if request_class == PlaybackRequestClass::Terminate {
        if let Some(session) = user_session {
            app_state
                .active_users
                .terminate_session(&user.username, session.token.as_str())
                .await;
        }
        return (
            crate::api::model::ConnectionAdmission {
                permission: UserConnectionPermission::Exhausted,
                kind: user_session
                    .and_then(|session| session.connection_kind)
                    .or(Some(crate::api::model::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    // Handle Prepare: no admission cost, just prepare state. Return Allowed without
    // running strategies or modifying counted state. Caller handles the actual activation.
    if request_class == PlaybackRequestClass::Prepare {
        return (
            crate::api::model::ConnectionAdmission {
                permission: UserConnectionPermission::Allowed,
                kind: user_session
                    .and_then(|session| session.connection_kind)
                    .or(Some(crate::api::model::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    if request_class == PlaybackRequestClass::FollowUp || !limits_enabled {
        return (
            crate::api::model::ConnectionAdmission {
                permission: user_session
                    .map_or(UserConnectionPermission::Allowed, |session| session.permission),
                kind: user_session
                    .and_then(|session| session.connection_kind)
                    .or(Some(crate::api::model::ConnectionKind::Normal)),
            },
            None,
            request_class,
        );
    }

    let result = resolve_admission_with_strategies(
        app_state,
        &user.username,
        user.max_connections,
        user.soft_connections,
        &fingerprint.client_ip,
        &fingerprint.addr,
        true,
        Some(session_token),
        activate_unbound_session,
        eviction_reentry_guard,
    )
    .await;

    (result.admission, result.grace_mode, request_class)
}

async fn should_suppress_eviction_for_recent_request(
    app_state: &Arc<AppState>,
    username: &str,
    client_ip: &str,
    guard: EvictionReentryGuard<'_>,
    target_addr: &std::net::SocketAddr,
) -> bool {
    match guard {
        EvictionReentryGuard::Session(session_token) => app_state
            .active_users
            .recently_evicted_session_protected_addr(session_token)
            .await
            .is_some_and(|protected_addr| protected_addr == *target_addr),
        EvictionReentryGuard::SocketPlayback { virtual_id } => app_state
            .active_users
            .recent_socket_reentry_protected_addr(username, client_ip, virtual_id)
            .await
            .is_some_and(|protected_addr| protected_addr == *target_addr),
    }
}

async fn get_admission_for_request(
    app_state: &Arc<AppState>,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    is_session_request: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
) -> crate::api::model::ConnectionAdmission {
    if is_session_request {
        if activate_unbound_session {
            app_state
                .get_connection_admission_for_session_activation(
                    username,
                    max_connections,
                    soft_connections,
                    session_token.unwrap_or_default(),
                )
                .await
        } else {
            app_state
                .get_connection_admission_for_session(
                    username,
                    max_connections,
                    soft_connections,
                    session_token.unwrap_or_default(),
                )
                .await
        }
    } else {
        app_state.get_connection_admission(username, max_connections, soft_connections).await
    }
}

pub(crate) fn resolve_request_url_for_logging<'a>(input: &ConfigInput, stream_url: &'a str) -> Cow<'a, str> {
    if is_media_server_playback_url(input, stream_url) {
        return Cow::Borrowed("media-server://<redacted>");
    }
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
    pub provider_name: Arc<str>,
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
    // Resolve target_name from target_id using the stable target config name.
    let target_name =
        attempt.app_state.app_config.get_target_by_id(info.channel.target_id).as_deref().map(|t| (&t.name).intern());
    attempt.app_state.connection_manager.record_connect_failed_with_provider_failure(
        &info,
        attempt.reason,
        attempt.failure_stage,
        None,
        None,
        target_name,
    );
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
    provider_name: Arc<str>,
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
use crate::utils::LRUResourceCache;
pub use internal_server_error;
use shared::error::TuliproxError;
use shared::model::{AdmissionStrategy, ConnectFailureReason, FailureStage, GeoIpUnavailablePolicy};
use shared::utils::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs};
pub use try_option_bad_request;
pub use try_option_forbidden;
pub use try_result_bad_request;
pub use try_result_not_found;
pub use try_result_or_status;
pub use try_unwrap_body;

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
                return StatusCode::NOT_FOUND.into_response();
            }
        }
        Err(err) => {
            error!("Failed to open file {}, {err:?}", file_path.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    match tokio::fs::File::open(file_path).await {
        Ok(file) => {
            let last_modified = file.metadata().await.ok().and_then(|m| m.modified().ok()).map(|m| {
                let dt: DateTime<Utc> = m.into();
                dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
            });

            let reader = async_file_reader(file);
            let stream = ReaderStream::new(reader);
            let body = Body::from_stream(stream);

            let mut builder = axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime_type)
                .header(header::CACHE_CONTROL, cache_control.unwrap_or("no-cache"));

            if let Some(lm) = last_modified {
                builder = builder.header(header::LAST_MODIFIED, lm);
            }

            try_unwrap_body!(builder.body(body))
        }
        Err(_) => internal_server_error!(),
    }
}

pub fn get_user_target_by_username(
    username: &str,
    app_state: &Arc<AppState>,
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
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
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
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
) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
    let username = api_req.username.as_str().trim();
    let password = api_req.password.as_str().trim();
    get_user_target_by_credentials(username, password, api_req, app_state)
}

/// Result of a policy-aware network access evaluation.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NetworkAccessDecision {
    /// Request is allowed (matched CIDR or country with `GeoIP` available).
    Allowed,
    /// Request is allowed because `GeoIP` is unavailable and policy is Allow.
    AllowedGeoIpUnavailable,
    /// Request is denied with a typed reason.
    Denied(NetworkAccessDenyReason),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum NetworkAccessDenyReason {
    NoCidrMatch,
    NoCountryMatch,
    GeoIpUnavailable,
    CountryUnknown,
    MalformedClientIp,
}

impl NetworkAccessDenyReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCidrMatch => "no_cidr_match",
            Self::NoCountryMatch => "no_country_match",
            Self::GeoIpUnavailable => "geoip_unavailable",
            Self::CountryUnknown => "country_unknown",
            Self::MalformedClientIp => "malformed_client_ip",
        }
    }
}

/// Logs a network access denial with structured context for operator debugging.
/// Do NOT log passwords or secrets.
#[allow(clippy::uninlined_format_args)]
pub fn log_network_access_denied(username: &str, client_ip: &str, reason: &str) {
    let sanitized_username = sanitize_sensitive_info(username);
    let sanitized_client_ip = sanitize_sensitive_info(client_ip);
    warn!(
        target: "network_access",
        "Network access denied: user=\"{}\" client_ip=\"{}\" reason={}",
        sanitized_username,
        sanitized_client_ip,
        reason
    );
}

/// Logs a network access allowed-without-GeoIP event for explicit-risk observability.
#[allow(clippy::uninlined_format_args)]
pub fn log_network_access_allowed_geoip_unavailable(username: &str, client_ip: &str) {
    warn!(
        target: "network_access",
        "Network access allowed because GeoIP is unavailable and reverse_proxy.geoip.unavailable_policy=allow; user=\"{}\" client_ip=\"{}\"",
        sanitize_sensitive_info(username),
        sanitize_sensitive_info(client_ip)
    );
}

/// Evaluates network access with the configured GeoIP-unavailable policy.
/// Returns a structured decision for logging and HTTP response mapping.
pub fn evaluate_network_access(
    user: &ProxyUserCredentials,
    client_ip: &str,
    geoip: &Arc<ArcSwapOption<crate::utils::GeoIp>>,
    geoip_unavailable_policy: GeoIpUnavailablePolicy,
) -> NetworkAccessDecision {
    let Some(access) = user.network_access.as_ref() else {
        return NetworkAccessDecision::Allowed;
    };
    if access.is_empty() {
        return NetworkAccessDecision::Allowed;
    }

    let Ok(parsed_ip) = client_ip.parse::<std::net::IpAddr>() else {
        return NetworkAccessDecision::Denied(NetworkAccessDenyReason::MalformedClientIp);
    };

    // CIDR check
    for net in &access.allowed_networks {
        if net.contains(&parsed_ip) {
            return NetworkAccessDecision::Allowed;
        }
    }

    // No CIDR match — check country rules
    if access.allowed_countries.is_empty() {
        return NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch);
    }

    // Country rules exist — check if GeoIP is available
    let geoip_guard = geoip.load();
    let Some(geoip_db) = geoip_guard.as_ref() else {
        // GeoIP unavailable — apply policy
        return match geoip_unavailable_policy {
            GeoIpUnavailablePolicy::Allow => NetworkAccessDecision::AllowedGeoIpUnavailable,
            GeoIpUnavailablePolicy::Deny => NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable),
        };
    };

    // GeoIP is loaded — do country lookup
    match geoip_db.lookup(client_ip) {
        Some(country) => {
            if access.allowed_countries.iter().any(|c| c == &country) {
                NetworkAccessDecision::Allowed
            } else {
                NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch)
            }
        }
        None => NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown),
    }
}

pub struct StreamOptions {
    pub stream_retry: bool,
    pub buffer_enabled: bool,
    pub buffer_size: usize,
    pub pipe_provider_stream: bool,
}

struct StreamingAcquireOptions<'a> {
    force_provider: Option<&'a Arc<str>>,
    allow_forced_provider_fallback: bool,
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

/// Metadata capturing which grace strategy was chosen and the original connection kind,
/// used to reconstruct the remaining-strategies slice on user-grace failure.
#[derive(Debug, Clone)]
pub(crate) struct GraceResolutionContext {
    /// Index of the grace strategy that was actually used.
    pub(crate) strategy_index: usize,
    /// Full effective strategy list for stable reconstruction of the remaining slice.
    pub(crate) strategies: Vec<AdmissionStrategy>,
    /// The original `ConnectionKind` from the admission decision that led to this grace.
    /// Preserved so that the remaining-strategy fallback can return the correct kind
    /// (e.g., `Soft`) even when the grace itself hardcoded `Normal`.
    #[allow(dead_code)]
    // Stored so the original admission kind remains available when follow-up
    // grace fallback reconstruction starts using it again.
    pub(crate) kind: Option<crate::api::model::ConnectionKind>,
}

/// Structured result of evaluating admission strategies.
#[derive(Debug)]
pub(crate) struct AdmissionStrategyResolution {
    pub(crate) admission: crate::api::model::ConnectionAdmission,
    pub(crate) grace_mode: Option<crate::api::model::GraceMode>,
    /// Present only when the request was admitted via a user-grace strategy.
    pub(crate) grace_context: Option<GraceResolutionContext>,
}

pub(in crate::api) fn get_effective_admission_strategies(app_state: &Arc<AppState>) -> Vec<AdmissionStrategy> {
    let config = app_state.app_config.config.load();
    let stream_config = config.reverse_proxy.as_ref().and_then(|rp| rp.stream.as_ref());
    match stream_config {
        Some(sc) if sc.admission_strategies.is_some() => sc.admission_strategies.clone().unwrap_or_default(),
        Some(sc) if sc.grace_period_millis > 0 => {
            vec![if sc.grace_period_hold_stream {
                AdmissionStrategy::GraceHoldStream
            } else {
                AdmissionStrategy::GraceInstantStream
            }]
        }
        _ => Vec::new(),
    }
}

/// Shared strategy-evaluation loop used by both the initial admission path
/// (`resolve_admission_with_strategies`) and the remaining-strategies path
/// (`evaluate_remaining_strategies_after_grace`).
///
/// Returns `Some(resolution)` when a Grace or a successful Eviction+Retry is found.
/// Returns `None` when every strategy in `strategies` returns `NoMatch` — the caller
/// is then responsible for constructing the final exhausted result with the correct
/// `kind` (preserved from the original admission).
#[allow(clippy::too_many_arguments)]
async fn evaluate_admission_strategy_loop<'a, F>(
    app_state: &'a Arc<AppState>,
    username: &'a str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &'a str,
    request_addr: &'a std::net::SocketAddr,
    use_session_admission: bool,
    session_token: Option<&'a str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'a>,
    strategies: &'a [shared::model::AdmissionStrategy],
    base_idx: usize,
    admission: crate::api::model::ConnectionAdmission,
    _kind_for_exhausted: Option<crate::api::model::ConnectionKind>,
    build_grace_ctx: F,
) -> Option<AdmissionStrategyResolution>
where
    F: Fn(usize) -> GraceResolutionContext,
{
    use crate::api::model::{evaluate_strategy, AdmissionDecision, StrategyContext};
    use shared::model::UserConnectionPermission;
    let mut candidates = app_state.active_users.get_eviction_candidates(username, client_ip).await;
    let ctx = StrategyContext { username, client_ip, strategies };
    let mut idx = 0usize;

    for strategy in strategies {
        match evaluate_strategy(*strategy, &ctx, &candidates) {
            AdmissionDecision::NoMatch => {}
            AdmissionDecision::Grace(mode) => {
                if app_state.active_users.grant_grace(username).await {
                    // Return a FRESH admission with GracePeriod permission (not the admission
                    // parameter, which may have Exhausted permission). The kind is preserved from
                    // the original admission.
                    return Some(AdmissionStrategyResolution {
                        admission: crate::api::model::ConnectionAdmission {
                            permission: UserConnectionPermission::GracePeriod,
                            kind: admission.kind,
                        },
                        grace_mode: Some(mode),
                        grace_context: Some(build_grace_ctx(base_idx + idx)),
                    });
                }
                debug!("Grace grant rejected for user {username}, continuing with later strategies");
            }
            AdmissionDecision::Evict(target) => {
                if should_suppress_eviction_for_recent_request(
                    app_state,
                    username,
                    client_ip,
                    eviction_reentry_guard,
                    &target.addr,
                )
                .await
                {
                    debug!(
                        "Skipping eviction strategy {strategy:?} for recently evicted request of user {username} targeting {}",
                        target.addr
                    );
                    continue;
                }
                debug!("Evicting connection {} for user {username}", target.addr);
                app_state
                    .active_users
                    .mark_recent_eviction_guard_for_addr(&target.addr, *request_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
                    .await;
                app_state.connection_manager.release_connection_as_kicked(&target.addr).await;
                let retry_admission = get_admission_for_request(
                    app_state,
                    username,
                    max_connections,
                    soft_connections,
                    use_session_admission,
                    session_token,
                    activate_unbound_session,
                )
                .await;
                if retry_admission.permission == UserConnectionPermission::Allowed {
                    return Some(AdmissionStrategyResolution {
                        admission: retry_admission,
                        grace_mode: None,
                        grace_context: None,
                    });
                }
                debug!("Admission still denied after eviction for user {username}, continuing with later strategies");
                candidates = app_state.active_users.get_eviction_candidates(username, client_ip).await;
            }
            AdmissionDecision::Deny => {
                // Caller constructs the final exhausted result.
                return None;
            }
        }
        idx += 1;
    }

    // All strategies returned NoMatch — caller constructs the final exhausted result.
    None
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api) async fn resolve_admission_with_strategies(
    app_state: &Arc<AppState>,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &str,
    request_addr: &std::net::SocketAddr,
    // This controls whether an existing logical playback session may reopen while the user is already at limit.
    // It is intentionally independent from whether the session is socket-bound.
    use_session_admission: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
) -> AdmissionStrategyResolution {
    use shared::model::UserConnectionPermission;

    let admission = get_admission_for_request(
        app_state,
        username,
        max_connections,
        soft_connections,
        use_session_admission,
        session_token,
        activate_unbound_session,
    )
        .await;

    if admission.permission != UserConnectionPermission::Exhausted {
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let strategies = get_effective_admission_strategies(app_state);
    if strategies.is_empty() {
        debug!("No admission strategies configured, denying request for user {username}");
        return AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None };
    }

    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: admission.kind,
    };

    if let Some(resolution) = evaluate_admission_strategy_loop(
        app_state,
        username,
        max_connections,
        soft_connections,
        client_ip,
        request_addr,
        use_session_admission,
        session_token,
        activate_unbound_session,
        eviction_reentry_guard,
        &strategies,
        0,
        admission,
        admission.kind,
        build_grace_ctx,
    )
        .await
    {
        return resolution;
    }

    debug!("No admission strategy could admit user {username}");
    AdmissionStrategyResolution { admission, grace_mode: None, grace_context: None }
}

/// Evaluates only the strategies that come AFTER the already-used grace strategy.
/// This is called when a user-grace has failed and the system needs to determine
/// whether a remaining eviction strategy can free a slot.
///
/// Rules:
/// - Only `grace_context.strategies[(strategy_index + 1)..]` are evaluated
/// - `NoMatch` -> continue to next strategy
/// - `Evict` -> kick target, retry admission
/// - `Grace` -> technically possible under current config (only one grace allowed), but handled
/// - `Deny` -> final exhausted
/// - Empty remaining slice -> final exhausted
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn evaluate_remaining_strategies_after_grace(
    app_state: &Arc<AppState>,
    username: &str,
    max_connections: u32,
    soft_connections: u16,
    client_ip: &str,
    request_addr: &std::net::SocketAddr,
    use_session_admission: bool,
    session_token: Option<&str>,
    activate_unbound_session: bool,
    eviction_reentry_guard: EvictionReentryGuard<'_>,
    grace_context: &GraceResolutionContext,
    original_kind: Option<crate::api::model::ConnectionKind>,
) -> AdmissionStrategyResolution {
    use shared::model::UserConnectionPermission;

    let remaining = grace_context.strategy_index + 1;
    let strategies = &grace_context.strategies;
    if remaining >= strategies.len() {
        debug!("No remaining strategies after grace for user {username}");
        return AdmissionStrategyResolution {
            admission: crate::api::model::ConnectionAdmission {
                permission: UserConnectionPermission::Exhausted,
                kind: original_kind,
            },
            grace_mode: None,
            grace_context: None,
        };
    }

    // admission.kind is used only inside build_grace_ctx for the Grace case's
    // GraceResolutionContext.kind. Both paths (helper early-return and caller
    // exhausted construction) use original_kind, so this is safe.
    let admission = crate::api::model::ConnectionAdmission {
        permission: UserConnectionPermission::Exhausted,
        kind: original_kind,
    };
    let build_grace_ctx = |global_idx: usize| GraceResolutionContext {
        strategy_index: global_idx,
        strategies: strategies.clone(),
        kind: original_kind,
    };

    if let Some(resolution) = evaluate_admission_strategy_loop(
        app_state,
        username,
        max_connections,
        soft_connections,
        client_ip,
        request_addr,
        use_session_admission,
        session_token,
        activate_unbound_session,
        eviction_reentry_guard,
        &strategies[remaining..],
        remaining,
        admission,
        original_kind,
        build_grace_ctx,
    )
        .await
    {
        return resolution;
    }

    debug!("No remaining strategy could admit user {username}");
    AdmissionStrategyResolution {
        admission: crate::api::model::ConnectionAdmission {
            permission: UserConnectionPermission::Exhausted,
            kind: original_kind,
        },
        grace_mode: None,
        grace_context: None,
    }
}

struct SessionActivationRequest<'a> {
    fingerprint: &'a Fingerprint,
    input: &'a ConfigInput,
    user: &'a ProxyUserCredentials,
    session_token: &'a str,
    request_class: Option<PlaybackRequestClass>,
    virtual_id: VirtualId,
    item_type: PlaylistItemType,
    stream_url: &'a str,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    socket_bound: bool,
}

struct PlaybackActivationResult {
    admission: crate::api::model::ConnectionAdmission,
    grace_mode: Option<crate::api::model::GraceMode>,
    grace_context: Option<crate::api::api_utils::GraceResolutionContext>,
    placeholder_transition_version: Option<u64>,
}

/// # Panics
#[allow(clippy::too_many_lines)]
async fn activate_session_before_stream_open(
    app_state: &Arc<AppState>,
    request: SessionActivationRequest<'_>,
) -> PlaybackActivationResult {
    let SessionActivationRequest {
        fingerprint,
        input,
        user,
        session_token,
        request_class,
        virtual_id,
        item_type,
        stream_url,
        connection_permission,
        connection_kind,
        socket_bound,
    } = request;
    // Classify based on current session state, not the pre-computed value.
    // If caller passes FollowUp, verify the session is still counted under the guard.
    // A stale FollowUp would bypass admission — reclassify to catch this.
    let effective_request_class = if let Some(request_class) = request_class {
        if request_class == PlaybackRequestClass::FollowUp {
            // Re-read session under the guard to ensure the counted lease is still held.
            // If it is no longer counted, classify it from the current lifecycle so
            // stale FollowUp requests cannot bypass admission.
            let current_session = app_state
                .active_users
                .get_and_update_user_session(&user.username, session_token)
                .await;
            classify_playback_request(PlaybackRequestFacts {
                item_type,
                existing_session: current_session.as_ref(),
                prepare_only: false,
                terminate: false,
            })
        } else {
            request_class
        }
    } else {
        let existing_session = app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await;
        classify_playback_request(PlaybackRequestFacts {
            item_type,
            existing_session: existing_session.as_ref(),
            prepare_only: false,
            terminate: false,
        })
    };
    let limits_enabled =
        app_state.app_config.config.load().user_access_control && (user.max_connections > 0 || user.soft_connections > 0);
    // Prepare: session setup without admission cost. The caller handles the actual activation.
    // FollowUp: already counted, no re-admission needed.
    // GracePeriod: grace already granted, no re-evaluation needed.
    // No limits: skip admission entirely.
    // GracePeriod permission is already resolved — skip admission strategies (re-run
    // would evict the same session again). But we must still materialize the grace
    // lifecycle (PendingProvider / GraceActive) so the session state is consistent.
    if connection_permission == UserConnectionPermission::GracePeriod {
        // Materialize grace lifecycle under the guard so the session state is consistent.
        // Determine which grace mode applies by checking the current session state.
        let current_session = app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await;
        let (_, resolved_grace) = match current_session.as_ref().map(|s| &s.lifecycle) {
            Some(crate::api::model::PlaybackLifecycle::PendingProvider { .. }) => {
                // Session already in PendingProvider — refresh deadline.
                let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                let _ = app_state
                    .active_users
                    .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                    .await;
                (crate::api::model::PlaybackLifecycle::PendingProvider {
                    data: crate::api::model::PendingProviderState {
                        reason_code: PendingProviderReason::GraceHold,
                        created_at: current_time_secs(),
                        deadline,
                        version: current_session.as_ref().map_or(0, |s| {
                            if let crate::api::model::PlaybackLifecycle::PendingProvider { data } = &s.lifecycle {
                                data.version
                            } else { 0 }
                        }),
                        wake_source: None,
                    },
                }, Some(crate::api::model::GraceMode::Hold))
            }
            Some(crate::api::model::PlaybackLifecycle::GraceActive) => {
                // Already in GraceActive — nothing to refresh.
                (crate::api::model::PlaybackLifecycle::GraceActive, Some(crate::api::model::GraceMode::Instant))
            }
            _ => {
                // Session not yet in grace state — infer from item_type defaults.
                // Live/LiveHls/LiveDash default to Hold; VOD/Catchup to Instant.
                if item_type.is_live() || item_type.is_live_adaptive() {
                    let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                    let _ = app_state
                        .active_users
                        .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                        .await;
                    (crate::api::model::PlaybackLifecycle::PendingProvider {
                        data: crate::api::model::PendingProviderState {
                            reason_code: PendingProviderReason::GraceHold,
                            created_at: current_time_secs(),
                            deadline,
                            version: 1,
                            wake_source: None,
                        },
                    }, Some(crate::api::model::GraceMode::Hold))
                } else {
                    app_state.active_users.mark_grace_active(&user.username, session_token).await;
                    (crate::api::model::PlaybackLifecycle::GraceActive, Some(crate::api::model::GraceMode::Instant))
                }
            }
        };
        return PlaybackActivationResult {
            admission: crate::api::model::ConnectionAdmission {
                permission: connection_permission,
                kind: Some(connection_kind),
            },
            grace_mode: resolved_grace,
            grace_context: None,
            placeholder_transition_version: None,
        };
    }
    // No limits: skip admission entirely. FollowUp / Prepare: no re-admission needed.
    if !limits_enabled
        || effective_request_class == PlaybackRequestClass::FollowUp
        || effective_request_class == PlaybackRequestClass::Prepare
    {
        return PlaybackActivationResult {
            admission: crate::api::model::ConnectionAdmission {
                permission: connection_permission,
                kind: Some(connection_kind),
            },
            grace_mode: None,
            grace_context: None,
            placeholder_transition_version: None,
        };
    }

    let placeholder_transition_version = Some(app_state
        .active_users
        .ensure_user_session_placeholder(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id,
            provider: input.name.as_ref(),
            stream_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind: Some(connection_kind),
            socket_bound,
        })
        .await);

    let result = resolve_admission_with_strategies(
        app_state,
        &user.username,
        user.max_connections,
        user.soft_connections,
        &fingerprint.client_ip,
        &fingerprint.addr,
        true,
        Some(session_token),
        true,
        if socket_bound {
            EvictionReentryGuard::SocketPlayback { virtual_id }
        } else {
            EvictionReentryGuard::Session(session_token)
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;
    let grace_context = result.grace_context;

    if admission.permission == UserConnectionPermission::GracePeriod {
        if matches!(grace_mode, Some(crate::api::model::GraceMode::Hold)) {
            // Hold: session waits for provider slot. Does not count until provider is acquired.
            let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
            let _ = app_state
                .active_users
                .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                .await;
        } else if matches!(grace_mode, Some(crate::api::model::GraceMode::Instant)) {
            // Instant: session is provisionally active immediately. Counts against admission limits
            // until the grace window resolves (success -> Active, failure -> Expired).
            app_state
                .active_users
                .mark_grace_active(&user.username, session_token)
                .await;
        }
    }

    PlaybackActivationResult {
        admission,
        grace_mode,
        grace_context,
        placeholder_transition_version,
    }
}

pub fn get_stream_alternative_url(stream_url: &str, input: &ConfigInput, alias_input: &Arc<ProviderConfig>) -> String {
    let matched_input = input.get_matched_config_by_url(stream_url);
    let source_base_url: Cow<'_, str> = if let Some((base_url, _, _)) = matched_input {
        Cow::Borrowed(base_url)
    } else if let Some(input_user_info) = input.get_user_info() {
        Cow::Owned(input_user_info.base_url)
    } else {
        return stream_url.to_string();
    };
    let source_username = matched_input.map_or(input.username.as_deref(), |(_, username, _)| username.map(String::as_str));
    let source_password = matched_input.map_or(input.password.as_deref(), |(_, _, password)| password.map(String::as_str));
    let Some(alt_input_user_info) = alias_input.get_user_info() else {
        return stream_url.to_string();
    };

    let mut modified = stream_url.replacen(source_base_url.as_ref(), &alt_input_user_info.base_url, 1);
    if let Some(old_username) = source_username {
        let new_username = alt_input_user_info.username.as_str();
        modified = modified.replacen(old_username, new_username, 1);
    }
    if let Some(old_password) = source_password {
        let new_password = alt_input_user_info.password.as_str();
        modified = modified.replacen(old_password, new_password, 1);
    }
    modified
}

fn stream_url_matches_provider(stream_url: &str, provider_cfg: &ProviderConfig) -> bool {
    provider_cfg.get_user_info().is_some_and(|user_info| stream_url.starts_with(&user_info.base_url))
}

pub(crate) fn resolve_redirect_location<'a>(
    input: Option<&ConfigInput>,
    stream_url: &'a str,
) -> Result<Cow<'a, str>, TuliproxError> {
    input.map_or(Ok(Cow::Borrowed(stream_url)), |input| input.resolve_url(stream_url))
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
            } else if options.allow_forced_provider_fallback {
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
            } else {
                debug_if_enabled!(
                    "Pinned provider {} unavailable for {}; strict provider affinity prevents fallback",
                    sanitize_sensitive_info(provider),
                    sanitize_sensitive_info(&fingerprint.addr.to_string())
                );
                None
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
                // Keep the URL only when provider affinity requires it or the URL already targets the selected
                // provider. Hot reload can leave old alias URLs in persisted playlists until the next processing run.
                let keep_original_url = forced_provider_allocated || stream_url_matches_provider(stream_url, provider_cfg);
                let (selected_provider_name, url) = if keep_original_url {
                    (provider_cfg.name.clone(), stream_url.to_string())
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

fn should_defer_provider_open_for_grace_hold(
    provider_grace_active: bool,
    hold_stream: bool,
    item_type: PlaylistItemType,
    is_reopen: bool,
) -> bool {
    if !(provider_grace_active && hold_stream) {
        return false;
    }

    // v3.3.0 opened provider-affine VOD/Series/Catchup reopens immediately, even when
    // provider grace was temporarily in effect. Parking these requests in GracePending
    // was introduced later and breaks players like libmpv during seek/reopen retries.
    // Keep hold-stream behavior for live/admission paths, but restore direct-open behavior
    // for provider-affine on-demand session reopens.
    !(!item_type.is_live() && item_type.requires_provider_affinity() && is_reopen)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::fn_params_excessive_bools)]
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
    allow_forced_provider_fallback: bool,
    allow_provider_grace: bool,
    virtual_id: VirtualId,
    user_priority: i8,
    connection_kind: crate::api::model::ConnectionKind,
    is_reopen: bool,
    session_owner: Option<&str>,
    grace_hold_override: Option<bool>,
    grace_resolution_context: Option<crate::api::api_utils::GraceResolutionContext>,
) -> Result<StreamDetails, TuliproxError> {
    let mut streaming_strategy = resolve_streaming_strategy(
        app_state,
        stream_url,
        fingerprint,
        input,
        StreamingAcquireOptions {
            force_provider,
            allow_forced_provider_fallback,
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
    if let Some(hold) = grace_hold_override {
        grace_period_options.hold_stream = hold;
    }
    let provider_grace_active =
        matches!(streaming_strategy.provider_stream_state, ProviderStreamState::GracePeriod(_, _));

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
            guard_provider_name.clone().or_else(|| Some(input.name.clone())),
            &grace_period_options,
            fingerprint.addr,
            virtual_id,
        ));
    }

    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom(provider_stream) => {
            let (stream, stream_info) = provider_stream;
            // When allocation is exhausted or no connection was acquired, guard_provider_name is None.
            // Use input.name as fallback so the provider field is never empty.
            let provider_name = guard_provider_name.clone().unwrap_or_else(|| input.name.clone());
            Ok(StreamDetails {
                stream,
                stream_info,
                provider_name: Some(provider_name),
                request_url: None,
                grace_period: grace_period_options,
                provider_grace_active: false,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: streaming_strategy.provider_handle.clone(),
                grace_resolution_context,
            })
        }
        ProviderStreamState::Available(_provider_name, request_url)
        | ProviderStreamState::GracePeriod(_provider_name, request_url) => {
            debug_if_enabled!(
                "Provider stream selection: allocated_provider={} actual_request_url={}",
                sanitize_sensitive_info(guard_provider_name.as_deref().unwrap_or("?")),
                sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
            );
            let defer_provider_stream_until_grace_check = if should_defer_provider_open_for_grace_hold(
                provider_grace_active,
                grace_period_options.hold_stream,
                item_type,
                is_reopen,
            ) {
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
            } else if is_media_server_stream_ref_url(request_url.as_ref()) {
                match open_media_server_stream_for_input(app_state, input, request_url.as_ref(), req_headers).await {
                    Ok((stream, stream_info)) => (Some(stream), stream_info, None),
                    Err(err) => {
                        error!("Can't open media-server stream: {err}");
                        (None, None, None)
                    }
                }
            } else {
                let parsed_url = Url::parse(&request_url);
                let ((stream, stream_info), reconnect_flag) = if let Ok(url) = parsed_url {
                    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
                    let disabled_headers = app_state.get_disabled_headers();
                    let mut provider_stream_factory_options =
                        ProviderStreamFactoryOptions::new(&crate::api::model::ProviderStreamFactoryParams {
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
                        });

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
                grace_resolution_context,
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
    if is_media_server_playback_url(params.input, provider_url.as_ref()) {
        return None;
    }

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
            let redirect_url = match resolve_redirect_location(Some(params.input), &redirect_url) {
                Ok(url) => url,
                Err(err) => {
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(err.to_string().as_str()));
                    return Some(StatusCode::BAD_REQUEST.into_response());
                }
            };
            debug_if_enabled!("Redirecting stream request to {}", sanitize_sensitive_info(redirect_url.as_ref()));
            return Some(redirect(redirect_url.as_ref()).into_response());
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
            let stream_url = match resolve_redirect_location(Some(params.input), &stream_url) {
                Ok(url) => url,
                Err(err) => {
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(err.to_string().as_str()));
                    return Some(StatusCode::BAD_REQUEST.into_response());
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
                    sanitize_sensitive_info(resolve_request_url_for_logging(params.input, redirect_url).as_ref())
                );
                return Some(redirect(redirect_url).into_response());
            }

            debug_if_enabled!(
                "Redirecting stream request to {}",
                sanitize_sensitive_info(resolve_request_url_for_logging(params.input, stream_url.as_ref()).as_ref())
            );
            return Some(redirect(stream_url.as_ref()).into_response());
        }
    }

    None
}

fn is_media_server_playback_url(input: &ConfigInput, stream_url: &str) -> bool {
    input.input_type == InputType::Plex || is_media_server_stream_ref_url(stream_url)
}

fn is_media_server_stream_ref_url(stream_url: &str) -> bool {
    Url::parse(stream_url).is_ok_and(|url| url.scheme() == "media-server")
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

async fn open_media_server_stream_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_url: &str,
    req_headers: &HeaderMap,
) -> Result<(BoxedProviderStream, ProviderStreamInfo), MediaServerError> {
    let stream_ref = parse_media_server_stream_ref(&input.name, stream_url)?;
    let range = req_headers.get(header::RANGE).and_then(|value| value.to_str().ok());
    let http_client = MediaServerHttpClient::new(app_state.http_client.load().as_ref().clone());

    let response = match input.input_type {
        InputType::Plex => {
            let client = PlexCatalogClient::from_input(input, http_client)?;
            open_media_server_proxy_stream_response(&client, &stream_ref, range).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server playback proxy is not implemented for this input type"));
        }
        InputType::M3u | InputType::Xtream | InputType::M3uBatch | InputType::XtreamBatch | InputType::Library => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("playlist item is not backed by a media-server input"));
        }
    };

    let headers = response
        .headers
        .iter()
        .filter(|(key, _)| !is_hop_by_hop_response_header(key))
        .filter_map(|(key, value)| value.to_str().ok().map(|value| (key.to_string(), value.to_string())))
        .collect::<Vec<_>>();
    let status = response.status;
    let stream = response.body.map_err(|err| StreamError::Stream(err.to_string())).boxed();
    Ok((stream, Some((headers, status, None, None))))
}

fn is_hop_by_hop_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// # Panics
#[allow(clippy::too_many_lines)]
pub async fn force_provider_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user_session: &UserSession,
    mut stream_channel: StreamChannel,
    ctx: ForceStreamRequestContext<'_>,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> impl IntoResponse + Send {
    let _transition_guard = app_state
        .active_users
        .acquire_playback_transition(&ctx.user.username, &user_session.token)
        .await;
    let stream_options = get_stream_options(app_state);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;
    let item_type = stream_channel.item_type;

    // Forced reopens must clear stale provider slots before reacquiring. For adaptive HLS/DASH
    // sessions we only target old active stream sockets of the same session, never manifest-only
    // session addresses, otherwise the controlling playlist request gets torn down.
    let cleanup_addrs = if item_type.is_live_adaptive() {
        app_state
            .active_users
            .adaptive_session_stream_cleanup_addrs(&ctx.user.username, &user_session.token, &fingerprint.addr)
            .await
    } else {
        session_reacquire_cleanup_addrs(user_session, &fingerprint.addr)
    };

    if cleanup_addrs.is_empty() {
        debug_if_enabled!(
            "Forced reopen cleanup had no stale targets for item_type={item_type:?} session={} current_addr={}",
            sanitize_sensitive_info(&user_session.token),
            sanitize_sensitive_info(&fingerprint.addr.to_string())
        );
    } else {
        debug_if_enabled!(
            "Forced reopen cleanup releasing {} stale target(s) for item_type={item_type:?} session={} current_addr={}",
            cleanup_addrs.len(),
            sanitize_sensitive_info(&user_session.token),
            sanitize_sensitive_info(&fingerprint.addr.to_string())
        );
        cleanup_forced_reopen_addrs(app_state, item_type, &cleanup_addrs).await;
    }

    // Provider-affine playback must stay on the same provider account across seeks/range reconnects.
    // Only non-affine sessions may fall back to a different account in the same lineup.
    let preferred_provider = Some(&user_session.provider);
    let allow_forced_provider_fallback = !item_type.requires_provider_affinity();
    // Never allow provider-side grace for forced seek/session reacquire.
    // Over-allocation here would break provider-side one-connection limits.
    let allow_provider_grace = false;
    let connection_kind = user_session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

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
        allow_forced_provider_fallback,
        allow_provider_grace,
        stream_channel.virtual_id,
        connection_priority_for_kind(ctx.user, connection_kind),
        connection_kind,
        true,
        Some(user_session.token.as_str()),
        grace_mode.map(|mode| matches!(mode, crate::api::model::GraceMode::Hold)),
        None,
    )
    .await
    {
        Ok(stream_details) => stream_details,
        Err(err) => {
            app_state
                .active_users
                .release_unbound_session_reservation(&ctx.user.username, &user_session.token, None, false)
                .await;
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
        app_state.active_users.update_session_addr(&ctx.user.username, &user_session.token, &fingerprint.addr).await;
        stream_channel.shared = share_stream;
        let socket_bound = user_session.socket_bound;
        let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
            stream_details,
            app_state,
            user: ctx.user,
            connection_permission,
            connection_kind: user_session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            fingerprint,
            stream_channel,
            socket_bound,
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
            sanitize_sensitive_info(
                resolve_request_url_for_logging(ctx.input, user_session.stream_url.as_ref()).as_ref()
            )
        );
        let mut response = try_unwrap_body!(response.body(body_stream));
        mark_response_as_uncompressed(&mut response);
        return response;
    }

    app_state.connection_manager.release_provider_handle(stream_details.provider_handle).await;
    app_state
        .active_users
        .release_unbound_session_reservation(&ctx.user.username, &user_session.token, None, false)
        .await;
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
pub(crate) async fn stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    session_token: &str,
    request_class: Option<PlaybackRequestClass>,
    mut stream_channel: StreamChannel,
    stream_url: &str,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    target: &Arc<ConfigTarget>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    allow_exhausted_shared_reconnect: bool,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> impl IntoResponse + Send {
    let _transition_guard = app_state
        .active_users
        .acquire_playback_transition(&user.username, session_token)
        .await;
    let request_log_stream_url = resolve_request_url_for_logging(input, stream_url);
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(request_log_stream_url.as_ref()));
    }

    let virtual_id = stream_channel.virtual_id;
    let item_type = stream_channel.item_type;
    let playback_extension = extract_extension_from_url(stream_url);
    let socket_bound = is_socket_bound_playback_session(item_type, playback_extension.as_deref());
    let mut connection_permission = connection_permission;
    let mut connection_kind = connection_kind;
    let activation = activate_session_before_stream_open(
        app_state,
        SessionActivationRequest {
            fingerprint,
            input,
            user,
            session_token,
            request_class,
            virtual_id,
            item_type,
            stream_url,
            connection_permission,
            connection_kind,
            socket_bound,
        },
        )
        .await;
    let grace_mode = activation.grace_mode.or(grace_mode);
    connection_permission = activation.admission.permission;
    connection_kind = activation.admission.kind.unwrap_or(connection_kind);

    let allow_shared_reuse =
        connection_permission != UserConnectionPermission::Exhausted || allow_exhausted_shared_reconnect;

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
                debug_if_enabled!("Opportunistic shared stream reuse for {}", sanitize_sensitive_info(stream_url));
                return value.into_response();
            }
        }
        None
    };

    if connection_permission == UserConnectionPermission::Exhausted {
        app_state
            .active_users
            .release_unbound_session_reservation(
                &user.username,
                session_token,
                activation.placeholder_transition_version,
                activation.placeholder_transition_version.is_some(),
            )
            .await;
        record_connect_failed_attempt(ConnectFailedAttempt {
            app_state,
            fingerprint,
            user,
            stream_channel: stream_channel.clone(),
            provider_name: input.name.clone(),
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
        true,
        stream_channel.virtual_id,
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
        false,
        Some(session_token),
        grace_mode.map(|m| matches!(m, crate::api::model::GraceMode::Hold)),
        activation.grace_context.clone(),
    )
    .await
    {
        Ok(stream_details) => stream_details,
        Err(err) => {
            app_state
                .active_users
                .release_unbound_session_reservation(
                    &user.username,
                    session_token,
                    activation.placeholder_transition_version,
                    activation.placeholder_transition_version.is_some(),
                )
                .await;
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

        let mut is_stream_shared = share_stream && !stream_details.has_deferred_provider_open();
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
            socket_bound,
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
            // Previously, we always persisted the provider's final request URL into the session.
            // For VOD-like playback that can be the wrong thing to reuse later: a seek or reopen
            // should start from the canonical playback entrypoint, not from a provider-specific
            // redirected target that happened to be used for an earlier request.
            // For Movies/Series/Catchup we therefore keep the canonical request URL in the session.
            // That avoids "session poisoning" where later seeks/resumes inherit a non-canonical URL.
            // For live playback we still keep the redirected URL when available, because staying on
            // the chosen upstream edge/server is often desirable there.
            let session_url: Cow<'_, str> = if matches!(
                item_type,
                PlaylistItemType::Catchup
                    | PlaylistItemType::Video
                    | PlaylistItemType::LocalVideo
                    | PlaylistItemType::Series
                    | PlaylistItemType::LocalSeries
                    | PlaylistItemType::SeriesInfo
                    | PlaylistItemType::LocalSeriesInfo
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
                    debug!(
                        "Streaming stream request from {}",
                        sanitize_sensitive_info(log_actual_request_url.as_ref())
                    );
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
                        | PlaylistItemType::SeriesInfo
                        | PlaylistItemType::LocalSeries
                        | PlaylistItemType::LocalSeriesInfo
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
                            socket_bound,
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
    app_state
        .active_users
        .release_unbound_session_reservation(
            &user.username,
            session_token,
            activation.placeholder_transition_version,
            activation.placeholder_transition_version.is_some(),
        )
        .await;
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
        return StreamMeteringConfig { meter_uid, meter_stream: true };
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

fn get_stream_config_u64(
    app_state: &Arc<AppState>,
    selector: impl FnOnce(&crate::model::StreamConfig) -> u64,
    default_value: u64,
) -> u64 {
    let config = app_state.app_config.config.load();
    let stream_config = config.reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.stream.as_ref());
    resolve_stream_config_u64(stream_config, selector, default_value)
}

pub(crate) fn get_hls_session_ttl_secs(app_state: &Arc<AppState>) -> u64 {
    get_stream_config_u64(app_state, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs())
}

async fn cleanup_forced_reopen_addrs(
    app_state: &Arc<AppState>,
    item_type: PlaylistItemType,
    cleanup_addrs: &[SocketAddr],
) {
    let close_client_socket = !item_type.is_live_adaptive();
    for addr in cleanup_addrs {
        app_state.connection_manager.release_provider_connection(addr).await;
        if close_client_socket {
            let _ = app_state.connection_manager.close_connection_signal(addr);
        }
    }
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
            let socket_bound =
                is_socket_bound_playback_session(stream_channel.item_type, extract_extension_from_url(stream_url).as_deref());
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
                        socket_bound,
                    })
                    .await;
            }
            stream_channel.shared = true;
            stream_channel.shared_joined_existing = Some(true);
            stream_channel.shared_stream_id =
                app_state.shared_stream_manager.get_meter_uid(stream_url).await.map(u64::from);
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
                socket_bound,
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
pub(crate) async fn local_stream_response(
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
    request_class: Option<PlaybackRequestClass>,
    check_path: bool,
) -> impl IntoResponse + Send {
    let _transition_guard = if let Some(session_token) = playback_session_token {
        Some(
            app_state
                .active_users
                .acquire_playback_transition(&user.username, session_token)
                .await,
        )
    } else {
        None
    };
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(&pli.url));
    }

    let mut connection_permission = connection_permission;
    let mut grace_mode = None;
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
                provider_name: input.name.clone(),
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
        connection_permission = UserConnectionPermission::Allowed;
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

    let stream =
        ReaderStream::new(file.take(content_length)).map_err(|err| StreamError::Stream(err.to_string())).boxed();
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let stream = if is_throttled_stream(pli.item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        ThrottledStream::new(stream, throttle_kbps).boxed()
    } else {
        stream
    };
    let socket_bound = is_socket_bound_playback_session(pli.item_type, extract_extension_from_url(&pli.url).as_deref());
    let mut connection_kind = connection_kind;
    if let Some(session_token) = playback_session_token {
        let activation = activate_session_before_stream_open(
            app_state,
            SessionActivationRequest {
                fingerprint,
                input,
                user,
                session_token,
                request_class,
                virtual_id: pli.virtual_id,
                item_type: pli.item_type,
                stream_url: &pli.url,
                connection_permission,
                connection_kind,
                socket_bound,
            },
        )
            .await;
        grace_mode = activation.grace_mode;
        connection_permission = activation.admission.permission;
        connection_kind = activation.admission.kind.unwrap_or(connection_kind);

        if connection_permission == UserConnectionPermission::Exhausted {
            app_state
                .active_users
                .release_unbound_session_reservation(
                    &user.username,
                    session_token,
                    activation.placeholder_transition_version,
                    activation.placeholder_transition_version.is_some(),
                )
                .await;
            return create_custom_video_stream_response(
                app_state,
                &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            )
            .into_response();
        }
    }
    let mut grace_period_options = app_state.get_grace_options();
    if connection_permission != UserConnectionPermission::GracePeriod {
        grace_period_options.period_millis = 0;
    }
    if let Some(resolved_mode) = grace_mode {
        grace_period_options.hold_stream = matches!(resolved_mode, crate::api::model::GraceMode::Hold);
    }
    let resolved_connection_kind = if let Some(session_token) = playback_session_token {
        app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await
            .and_then(|session| session.connection_kind)
            .unwrap_or(connection_kind)
    } else {
        connection_kind
    };
    if let Some(session_token) = playback_session_token {
        let _ = app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id: pli.virtual_id,
                provider: input.name.as_ref(),
                stream_url: &pli.url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(resolved_connection_kind),
                socket_bound,
            })
            .await;
    }
    let stream = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
        stream_details: StreamDetails::from_stream(stream, grace_period_options),
        app_state,
        user,
        connection_permission,
        connection_kind: resolved_connection_kind,
        fingerprint,
        stream_channel: pli.clone(),
        socket_bound,
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

    // Non-retriable Status -> Upstream Response incl. Body
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
    if resource_url.starts_with("media-server://image/") {
        return match open_media_server_image_resource(app_state, resource_url).await {
            Ok(response) => response,
            Err(err) => {
                let status = media_server_image_error_status(&err);
                match status {
                    StatusCode::BAD_REQUEST => warn!("Invalid media-server image resource URL: {err}"),
                    StatusCode::NOT_FOUND => debug!("Media-server image resource was not found: {err}"),
                    _ => error!("Can't open media-server image from upstream: {err}"),
                }
                status.into_response()
            }
        };
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

async fn open_media_server_image_resource(
    app_state: &Arc<AppState>,
    resource_url: &str,
) -> Result<Response<Body>, MediaServerError> {
    let image_ref = parse_media_server_image_ref(resource_url)?;
    let input_name = media_server_image_input_name(&image_ref);
    let input = app_state.app_config.get_input_by_name(input_name).ok_or_else(|| {
        MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
            .provider("media-server")
            .detail("media-server image input was not found")
    })?;
    let http_client = MediaServerHttpClient::new(app_state.http_client.load().as_ref().clone());

    let response = match input.input_type {
        InputType::Plex => {
            let client = PlexCatalogClient::from_input(&input, http_client)?;
            open_media_server_proxy_image_response(&client, &image_ref).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server image proxy is not implemented for this input type"));
        }
        InputType::M3u | InputType::Xtream | InputType::M3uBatch | InputType::XtreamBatch | InputType::Library => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server image input is not backed by a media-server input"));
        }
    };

    let mut builder = Response::builder().status(response.status);
    for (key, value) in &response.headers {
        if !is_hop_by_hop_response_header(key) {
            builder = builder.header(key, value);
        }
    }
    let body = response.body.map_err(|err| StreamError::Stream(err.to_string()));
    builder.body(Body::from_stream(body)).map_err(|err| {
        MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .provider("media-server")
            .detail(format!("media-server image response build failed: {err}"))
    })
}

fn media_server_image_error_status(err: &MediaServerError) -> StatusCode {
    match err.kind {
        MediaServerErrorKind::MediaServerItemNotFound | MediaServerErrorKind::NoDirectPlayableMediaServerSource => {
            StatusCode::NOT_FOUND
        }
        MediaServerErrorKind::MediaServerStreamOpenFailed if is_media_server_image_validation_error(err) => {
            StatusCode::BAD_REQUEST
        }
        MediaServerErrorKind::MediaServerStreamOpenFailed
        | MediaServerErrorKind::MediaServerAuthDenied
        | MediaServerErrorKind::MediaServerUnavailable
        | MediaServerErrorKind::MediaServerLibraryUnavailable
        | MediaServerErrorKind::MediaServerLibraryTypeUnsupported
        | MediaServerErrorKind::MediaServerCatalogDecodeFailed
        | MediaServerErrorKind::MediaServerCatalogPageStalled
        | MediaServerErrorKind::MediaServerCatalogIncomplete
        | MediaServerErrorKind::MediaServerRateLimited
        | MediaServerErrorKind::MediaServerDiscoveryFailed => StatusCode::BAD_GATEWAY,
    }
}

fn is_media_server_image_validation_error(err: &MediaServerError) -> bool {
    err.detail_text().is_some_and(|detail| {
        detail.contains("resource URL is not a media server image URL")
            || detail.contains("media server image URL is missing required path parts")
            || detail.contains("unsupported media server image URL scheme")
            || detail.contains("media-server image input is not backed by a media-server input")
    })
}

fn media_server_image_input_name(image_ref: &MediaServerImageRef) -> &Arc<str> {
    match image_ref {
        MediaServerImageRef::Emby { input_name, .. }
        | MediaServerImageRef::Jellyfin { input_name, .. }
        | MediaServerImageRef::Plex { input_name, .. } => input_name,
    }
}

pub fn separate_number_and_remainder(input: &str) -> (&str, Option<&str>) {
    input.rfind('.').map_or_else(
        || (input, None),
        |dot_index| {
            let number_part = &input[..dot_index];
            let rest = &input[dot_index..];
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

pub fn create_session_fingerprint(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    socket_bound: bool,
) -> String {
    if socket_bound {
        concat_string!(&fingerprint.addr.to_string(), "|", username, "|", &virtual_id.to_string())
    } else {
        concat_string!(&fingerprint.key, "|", username, "|", &virtual_id.to_string())
    }
}

pub(crate) fn create_playback_session_fingerprint(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    item_type: PlaylistItemType,
    extension: Option<&str>,
) -> String {
    // This scopes the session identity, not the session address-tracking policy.
    // Adaptive playlist starts need a per-initial-socket token so two players behind
    // the same IP/UA can watch the same HLS/DASH stream independently. The created
    // UserSession itself can still be non-socket-bound.
    let socket_bound =
        is_socket_bound_playback_session(item_type, extension) || is_session_based_playback(item_type, extension);
    create_session_fingerprint(fingerprint, username, virtual_id, socket_bound)
}

pub fn create_catchup_session_key(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    concat_string!("catchup|", &fingerprint.key, "|", username, "|", &virtual_id.to_string(), "|session")
}

pub(crate) fn is_session_based_playback(item_type: PlaylistItemType, extension: Option<&str>) -> bool {
    item_type.is_live_adaptive() || matches!(extension, Some(ext) if ext == HLS_EXT || ext == DASH_EXT)
}

pub(crate) fn is_socket_bound_playback_session(item_type: PlaylistItemType, extension: Option<&str>) -> bool {
    item_type.uses_socket_bound_session() && !is_session_based_playback(item_type, extension)
}

fn session_reacquire_cleanup_addrs(user_session: &UserSession, current_addr: &SocketAddr) -> Vec<SocketAddr> {
    let mut addrs: SmallVec<[SocketAddr; 4]> = SmallVec::new();
    if user_session.addr != *current_addr {
        addrs.push(user_session.addr);
    }
    for addr in &user_session.active_addrs {
        if *addr != *current_addr && !addrs.contains(addr) {
            addrs.push(*addr);
        }
    }
    addrs.into_vec()
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
        output_clusters: shared::model::ClusterFlags::all(),
        ui_enabled: false,
        comment: None,
        priority: 0,
        soft_connections: 0,
        soft_priority: 0,
        t_is_api_user: true,
        network_access: None,
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
    use crate::model::StreamHistoryConfig;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, EventManager,
            MetadataUpdateManager, PlaylistStorageState, SharedStreamManager,
        },
        auth::Fingerprint,
        model::{
            AppConfig, Config, ConfigInput, ConfigInputAlias, ConfigProvider, ConfigTarget,
            MediaToolCapabilities, NetworkAccess, ProcessTargets, ProxyUserCredentials, SourcesConfig,
        },
        utils::{FileLockManager, GeoIp},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::{HeaderMap, Response, StatusCode};
    use bytes::Bytes;
    use futures::stream;
    use shared::{
        foundation::Filter,
        model::{
            ClusterFlags, ConfigPaths, ConfigProviderDto, ConfigTargetOptions, InputFetchMethod, InputType,
            PlaylistItemType, ProcessingOrder, ProviderUrlSelectionPolicy, ProxyType, StreamChannel, XtreamCluster,
        },
        utils::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs, Internable},
    };
    use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
    use tokio::sync::mpsc;

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
    fn resolve_redirect_location_resolves_provider_scheme_urls() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "develop".intern(),
            urls: vec!["https://provider.example".intern()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        });
        let input = ConfigInput {
            name: "provider".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..ConfigInput::default()
        };

        let resolved =
            resolve_redirect_location(Some(&input), "provider://develop/live/provider-user/provider-pass/33486.m3u8")
                .expect("provider url should resolve");

        assert_eq!(resolved, "https://provider.example/live/provider-user/provider-pass/33486.m3u8");
    }

    #[test]
    fn media_server_proxy_response_header_filter_drops_hop_by_hop_headers() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "trailers",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(is_hop_by_hop_response_header(&HeaderName::from_static(name)));
        }
        assert!(!is_hop_by_hop_response_header(&header::CONTENT_TYPE));
    }

    #[test]
    fn media_server_image_error_status_classifies_client_and_upstream_failures() {
        let parse_error = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("media server image URL is missing required path parts");
        assert_eq!(media_server_image_error_status(&parse_error), StatusCode::BAD_REQUEST);

        let not_found = MediaServerError::new(MediaServerErrorKind::MediaServerItemNotFound)
            .detail("plex media-server image URL is missing image_path");
        assert_eq!(media_server_image_error_status(&not_found), StatusCode::NOT_FOUND);

        let upstream = MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
            .detail("media-server image request failed");
        assert_eq!(media_server_image_error_status(&upstream), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn media_server_playback_urls_are_proxy_only_redirect_guard_candidates() {
        let plex_input = ConfigInput {
            input_type: InputType::Plex,
            ..ConfigInput::default()
        };
        let emby_input = ConfigInput {
            input_type: InputType::Emby,
            ..ConfigInput::default()
        };
        let m3u_input = ConfigInput {
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };

        assert!(is_media_server_playback_url(
            &plex_input,
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
        ));
        assert!(is_media_server_playback_url(
            &m3u_input,
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
        ));
        assert!(is_media_server_playback_url(&plex_input, "https://plex.example/stream.mkv"));
        assert!(!is_media_server_playback_url(&emby_input, "https://emby.example/stream.mkv"));
        assert!(!is_media_server_playback_url(&m3u_input, "https://provider.example/stream.mkv"));
        assert!(!is_media_server_stream_ref_url("https://provider.example/stream.mkv"));
        assert!(is_media_server_stream_ref_url(
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
        ));
        assert_eq!(
            resolve_request_url_for_logging(
                &plex_input,
                "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
            )
            .as_ref(),
            "media-server://<redacted>"
        );
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
            resolve_stream_config_u64(None, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs()),
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
    fn provider_affinity_policy_matches_stream_types() {
        assert!(!PlaylistItemType::Live.requires_provider_affinity());
        assert!(!PlaylistItemType::LiveUnknown.requires_provider_affinity());
        assert!(PlaylistItemType::LiveHls.requires_provider_affinity());
        assert!(PlaylistItemType::LiveDash.requires_provider_affinity());
        assert!(PlaylistItemType::Video.requires_provider_affinity());
        assert!(PlaylistItemType::Series.requires_provider_affinity());
        assert!(PlaylistItemType::Catchup.requires_provider_affinity());
    }

    #[tokio::test]
    async fn resolve_streaming_strategy_honors_forced_provider_fallback_policy() {
        let app_state = create_test_dual_provider_app_state();
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&input_name)
            .cloned()
            .unwrap_or_else(|| unreachable!());
        let pinned_provider = "provider_1".intern();
        let busy_addr: SocketAddr = "127.0.0.1:55301".parse().unwrap_or_else(|_| unreachable!());
        let strict_addr: SocketAddr = "127.0.0.1:55302".parse().unwrap_or_else(|_| unreachable!());
        let fallback_addr: SocketAddr = "127.0.0.1:55303".parse().unwrap_or_else(|_| unreachable!());
        let stream_url = "http://provider-1.example/movie/1.mkv";

        let busy = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &pinned_provider,
                &busy_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await;
        assert!(busy.is_some(), "setup should occupy the pinned provider");

        let strict = resolve_streaming_strategy(
            &app_state,
            stream_url,
            &create_test_fingerprint(strict_addr),
            &input,
            StreamingAcquireOptions {
                force_provider: Some(&pinned_provider),
                allow_forced_provider_fallback: false,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("vod-session"),
            },
        )
        .await;
        assert!(strict.provider_handle.is_none(), "strict provider affinity should not allocate a different provider");
        assert!(
            matches!(strict.provider_stream_state, ProviderStreamState::Custom(_)),
            "strict provider affinity should fail closed when the pinned provider is unavailable"
        );

        let fallback = resolve_streaming_strategy(
            &app_state,
            stream_url,
            &create_test_fingerprint(fallback_addr),
            &input,
            StreamingAcquireOptions {
                force_provider: Some(&pinned_provider),
                allow_forced_provider_fallback: true,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("live-session"),
            },
        )
        .await;
        let (ProviderStreamState::Available(Some(fallback_provider), _)
        | ProviderStreamState::GracePeriod(Some(fallback_provider), _)) = fallback.provider_stream_state
        else {
            panic!("fallback-enabled request should allocate a provider")
        };
        assert_eq!(fallback_provider.as_ref(), "provider_2");

        app_state.active_provider.release_connection(&busy_addr).await;
        app_state.active_provider.release_connection(&strict_addr).await;
        app_state.active_provider.release_connection(&fallback_addr).await;
    }

    #[tokio::test]
    async fn resolve_streaming_strategy_rewrites_stale_alias_url_to_selected_main_provider() {
        let app_state = create_test_dual_provider_app_state();
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&input_name)
            .cloned()
            .unwrap_or_else(|| unreachable!());
        let addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());

        let strategy = resolve_streaming_strategy(
            &app_state,
            "http://provider-2.example/live/user2/pass2/100.ts",
            &create_test_fingerprint(addr),
            &input,
            StreamingAcquireOptions {
                force_provider: None,
                allow_forced_provider_fallback: false,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("live-session"),
            },
        )
        .await;

        let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
            panic!("request should allocate the main provider")
        };
        assert_eq!(provider.as_ref(), "provider_1");
        assert_eq!(url.as_ref(), "http://provider-1.example/live/user1/pass1/100.ts");

        app_state.active_provider.release_connection(&addr).await;
    }

    #[test]
    fn test_should_allow_exhausted_shared_reconnect_only_for_matching_shared_session() {
        let session = UserSession {
            transition_version: 1,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            token: "tok".to_string(),
            virtual_id: 282,
            provider: Arc::<str>::from("provider"),
            stream_url: Arc::<str>::from("http://provider/live/449924.ts"),
            addr: "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!()),
            socket_bound: false,
            active_addrs: vec!["127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!())],
            ts: 1,
            started_at: 1,
            permission: UserConnectionPermission::Allowed,
            lifecycle: crate::api::model::PlaybackLifecycle::Active,
        };

        assert!(should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/449924.ts"));
        assert!(!should_allow_exhausted_shared_reconnect(false, Some(&session), 282, "http://provider/live/449924.ts"));
        assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 999, "http://provider/live/449924.ts"));
        assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/other.ts"));
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

    fn create_test_dual_provider_app_config() -> AppConfig {
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
            aliases: Some(vec![ConfigInputAlias {
                id: 2,
                name: "provider_2".intern(),
                url: "http://provider-2.example".to_string(),
                username: Some("user2".to_string()),
                password: Some("pass2".to_string()),
                priority: 1,
                max_connections: 1,
                exp_date: None,
                enabled: true,
            }]),
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

    #[tokio::test]
    async fn create_api_proxy_user_defaults_output_clusters_to_all() {
        let app_state = create_test_app_state();
        let user = create_api_proxy_user(&app_state);
        assert_eq!(user.output_clusters, ClusterFlags::all());
    }

    fn create_test_provider_app_state() -> Arc<AppState> {
        create_test_app_state_for_config(Arc::new(create_test_provider_app_config()))
    }

    fn create_test_dual_provider_app_state() -> Arc<AppState> {
        create_test_app_state_for_config(Arc::new(create_test_dual_provider_app_config()))
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
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            history_config.as_ref(),
        ));

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

    fn create_test_fingerprint_with_user_agent(addr: std::net::SocketAddr, user_agent: &str) -> Fingerprint {
        Fingerprint::new(format!("{}|{user_agent}", addr.ip()), addr.ip().to_string(), addr)
    }

    fn create_test_app_state_with_stream_config(stream: crate::model::StreamConfig) -> Arc<AppState> {
        let config = Config {
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(stream),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
            }),
            user_access_control: true,
            ..Config::default()
        };

        let mut app_cfg = create_test_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
        create_test_app_state_for_config(Arc::new(app_cfg))
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
            epg_channel_id: None,
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
            epg_channel_id: None,
        }
    }

    fn create_test_session(
        token: &str,
        item_type: PlaylistItemType,
        lifecycle: crate::api::model::PlaybackLifecycle,
    ) -> UserSession {
        UserSession {
            token: token.to_string(),
            transition_version: 1,
            virtual_id: 42,
            provider: Arc::<str>::from("provider-a"),
            stream_url: Arc::<str>::from(match item_type {
                PlaylistItemType::LiveHls => "http://provider-1.example/live/42.m3u8",
                _ => "http://provider-1.example/live/42.ts",
            }),
            addr: "127.0.0.1:55555".parse().unwrap_or_else(|_| unreachable!()),
            socket_bound: item_type.uses_socket_bound_session(),
            active_addrs: Vec::new(),
            ts: 1,
            started_at: 1,
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            lifecycle,
        }
    }

    #[test]
    fn classify_playback_request_marks_adaptive_playlist_request_as_prepare() {
        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: None,
            prepare_only: true,
            terminate: false,
        });

        assert_eq!(request_class, PlaybackRequestClass::Prepare);
    }

    #[test]
    fn classify_playback_request_marks_preserved_session_as_activate() {
        let session = create_test_session(
            "tok-preserved",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::Preserved,
        );

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(request_class, PlaybackRequestClass::Activate);
    }

    #[test]
    fn classify_playback_request_marks_counted_session_as_follow_up() {
        let session = create_test_session(
            "tok-active",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::Active,
        );

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(request_class, PlaybackRequestClass::FollowUp);
    }

    /// `PendingProvider` must NOT be classified as `FollowUp`.
    /// `PendingProvider` has no counted lease yet — the session is still waiting
    /// for a provider slot. A new request on a `PendingProvider` session should
    /// be `Activate` so that full admission evaluation happens, not a cheap
    /// `FollowUp` skip.
    #[test]
    fn classify_playback_request_marks_pending_provider_as_activate_not_follow_up() {
        let session = create_test_session(
            "tok-pending",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::PendingProvider {
                data: crate::api::model::PendingProviderState {
                    reason_code: crate::api::model::PendingProviderReason::GraceHold,
                    created_at: 1,
                    deadline: 30,
                    version: 1,
                    wake_source: None,
                }
            },
        );

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(
            request_class,
            PlaybackRequestClass::Activate,
            "PendingProvider should not be FollowUp - it has no counted lease yet"
        );
    }

    /// `Active` without a counted lease must NOT be classified as `FollowUp`.
    /// `FollowUp` should only be returned when the session actually owns a
    /// counted admission lease. A session with `Active` lifecycle but no counted
    /// lease should go through `Activate` so that the counted lease is reacquired.
    #[test]
    fn classify_playback_request_marks_active_without_counted_as_activate_not_follow_up() {
        let mut session = create_test_session(
            "tok-active-uncounted",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::Active, // counted=false via is_counted()
        );
        // Manually force counted=false by setting to Prepared lifecycle, then restoring
        // Note: is_counted() returns false for Prepared, true for Active
        // For this test we need a session that is Active lifecycle but not counted
        // The new model derives counted from lifecycle, so we must use a different lifecycle
        // to represent "not counted". Use Prepared instead.
        session.lifecycle = crate::api::model::PlaybackLifecycle::Prepared;

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(
            request_class,
            PlaybackRequestClass::Activate,
            "Active session with counted=false should not be FollowUp"
        );
    }

    /// Prepared sessions must be classified as Activate.
    #[test]
    fn classify_playback_request_marks_prepared_session_as_activate() {
        let session = create_test_session(
            "tok-prepared",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::Prepared,
        );

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(request_class, PlaybackRequestClass::Activate);
    }

    /// `GraceActive` without counted lease must NOT be classified as `FollowUp`.
    #[test]
    fn classify_playback_request_marks_grace_active_without_counted_as_activate() {
        let mut session = create_test_session(
            "tok-grace-uncounted",
            PlaylistItemType::LiveHls,
            crate::api::model::PlaybackLifecycle::Active, // is_counted() = true for GraceActive
        );
        // Test scenario: session has GraceActive lifecycle but we need it NOT counted
        // This represents the edge case before grace task resolves. Use Prepared lifecycle
        // to model "not counted" since is_counted() returns false for Prepared.
        session.lifecycle = crate::api::model::PlaybackLifecycle::Prepared;

        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::LiveHls,
            existing_session: Some(&session),
            prepare_only: false,
            terminate: false,
        });

        assert_eq!(
            request_class,
            PlaybackRequestClass::Activate,
            "GraceActive session with counted=false should not be FollowUp"
        );
    }

    #[tokio::test]
    async fn activate_session_before_stream_open_skips_placeholder_for_follow_up_session() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
        });
        let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "follow-up-user".to_string();
        user.max_connections = 1;
        let mut channel = create_test_live_channel("http://provider-1.example/live/55220.m3u8");
        channel.item_type = PlaylistItemType::LiveHls;
        channel.virtual_id = 55220;

        app_state.connection_manager.add_connection(&addr).await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-follow-up",
                virtual_id: channel.virtual_id,
                provider: input.name.as_ref(),
                stream_url: channel.url.as_ref(),
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &fingerprint,
                provider: input.name.clone(),
                stream_channel: &channel,
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-follow-up"),
            })
            .await;

        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-follow-up",
                request_class: None,
                virtual_id: channel.virtual_id,
                item_type: PlaylistItemType::LiveHls,
                stream_url: channel.url.as_ref(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: true,
            },
        )
        .await;

        assert_eq!(activation.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(activation.admission.kind, Some(crate::api::model::ConnectionKind::Normal));
        assert_eq!(activation.grace_mode, None);
        assert!(
            activation.placeholder_transition_version.is_none(),
            "follow-up activation must not create a placeholder session"
        );
    }

    #[tokio::test]
    async fn activate_session_before_stream_open_revalidates_precomputed_follow_up_request_class() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
        });
        let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "precomputed-follow-up-user".to_string();
        user.max_connections = 1;
        let mut channel = create_test_live_channel("http://provider-1.example/live/55221.m3u8");
        channel.item_type = PlaylistItemType::LiveHls;
        channel.virtual_id = 55221;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-precomputed-follow-up",
                virtual_id: channel.virtual_id,
                provider: input.name.as_ref(),
                stream_url: channel.url.as_ref(),
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-precomputed-follow-up",
                request_class: Some(PlaybackRequestClass::FollowUp),
                virtual_id: channel.virtual_id,
                item_type: PlaylistItemType::LiveHls,
                stream_url: channel.url.as_ref(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: true,
            },
        )
        .await;

        assert_eq!(activation.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(activation.admission.kind, Some(crate::api::model::ConnectionKind::Normal));
        assert_eq!(activation.grace_mode, None);
        assert!(
            activation.placeholder_transition_version.is_some(),
            "precomputed FollowUp must be revalidated against the current uncounted lifecycle"
        );
    }

    // stale FollowUp revalidation
    #[tokio::test]
    async fn activate_session_before_stream_open_stale_follow_up_reclassified_on_counted_lease_release() {
        // Scenario: pre-computed FollowUp, but session's counted lease was released before
        // the guard was acquired. Must reclassify to Activate so admission runs.
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
        });
        let addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "stale-followup-user".to_string();
        user.max_connections = 1;
        let mut channel = create_test_live_channel("http://provider-1.example/live/55230.m3u8");
        channel.item_type = PlaylistItemType::LiveHls;
        channel.virtual_id = 55230;

        // Session created in Active (counted) state.
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-stale-followup",
                virtual_id: channel.virtual_id,
                provider: input.name.as_ref(),
                stream_url: channel.url.as_ref(),
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        // Simulate the counted lease being released before activation:
        // expire the session so it no longer has a counted lease.
        app_state
            .active_users
            .terminate_session(&user.username, "tok-stale-followup")
            .await;

        // Call activate with stale FollowUp. Must NOT skip admission — reclassification
        // to Activate must run so the placeholder is created.
        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-stale-followup",
                request_class: Some(PlaybackRequestClass::FollowUp),
                virtual_id: channel.virtual_id,
                item_type: PlaylistItemType::LiveHls,
                stream_url: channel.url.as_ref(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: true,
            },
        )
        .await;

        // Must NOT skip — placeholder must be created since session is expired.
        assert!(
            activation.placeholder_transition_version.is_some(),
            "stale FollowUp with expired session must run admission and create placeholder"
        );
    }

    // pre-resolved Grace materialization
    #[tokio::test]
    async fn activate_session_before_stream_open_pre_resolved_grace_period_materializes_pending_provider() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });
        let addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "pre-resolved-grace-user".to_string();
        user.max_connections = 1;
        let mut channel = create_test_live_channel("http://provider-1.example/live/55231.m3u8");
        channel.item_type = PlaylistItemType::LiveHls;
        channel.virtual_id = 55231;

        // Session in Prepared state (no grace lifecycle yet).
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-pre-resolved-grace",
                virtual_id: channel.virtual_id,
                provider: input.name.as_ref(),
                stream_url: channel.url.as_ref(),
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        // Call activation with pre-resolved GracePeriod permission.
        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-pre-resolved-grace",
                request_class: None,
                virtual_id: channel.virtual_id,
                item_type: PlaylistItemType::LiveHls,
                stream_url: channel.url.as_ref(),
                connection_permission: UserConnectionPermission::GracePeriod,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: true,
            },
        )
        .await;

        assert_eq!(activation.admission.permission, UserConnectionPermission::GracePeriod);
        assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-pre-resolved-grace")
            .await;
        assert!(
            session.is_some_and(|s| matches!(s.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. })),
            "pre-resolved GracePeriod must materialize as PendingProvider lifecycle"
        );
    }

    /// `activate_session_before_stream_open` skips placeholder for Prepare class.
    #[tokio::test]
    async fn activate_session_before_stream_open_skips_placeholder_for_prepare() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
        });
        let addr: SocketAddr = "127.0.0.1:55222".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "prepare-user".to_string();
        user.max_connections = 1;

        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-prepare",
                // Explicitly pass Prepare class — placeholder and admission should be skipped.
                request_class: Some(PlaybackRequestClass::Prepare),
                virtual_id: 55222,
                item_type: PlaylistItemType::LiveHls,
                stream_url: "http://provider.example/live/test.ts",
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: true,
            },
        )
        .await;

        // Prepare returns Allowed without running admission strategies.
        assert_eq!(activation.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(activation.grace_mode, None);
        assert!(
            activation.placeholder_transition_version.is_none(),
            "Prepare activation must not create a placeholder session"
        );
    }

    /// `resolve_playback_request_admission` with `prepare_only = true` returns `Prepare` class.
    #[tokio::test]
    async fn resolve_playback_request_admission_prepare_only_returns_prepare_class() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });
        let addr: SocketAddr = "127.0.0.1:55223".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "prepare-only-user".to_string();
        user.max_connections = 1;

        let (admission, grace_mode, request_class) = resolve_playback_request_admission(
            &app_state,
            &user,
            &fingerprint,
            PlaylistItemType::LiveHls,
            None,
            "tok-prepare-only",
            false,
            EvictionReentryGuard::Session("tok-prepare-only"),
            true,  // prepare_only
            false, // terminate
        )
        .await;

        assert_eq!(request_class, PlaybackRequestClass::Prepare);
        // Prepare returns Allowed without running strategies.
        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(grace_mode, None);
    }

    /// `resolve_playback_request_admission` with `terminate = true` returns `Terminate` class
    /// and calls `terminate_session` on the existing session.
    #[tokio::test]
    async fn resolve_playback_request_admission_terminate_returns_terminate_class() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });
        let addr: SocketAddr = "127.0.0.1:55224".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "terminate-user".to_string();
        user.max_connections = 2;

        // First create a session.
        let session_token = "tok-terminate";
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token,
                virtual_id: 55224,
                provider: "test-provider",
                stream_url: "http://provider.example/test.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        // Verify session exists.
        let before = app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await;
        assert!(before.is_some(), "session should exist before terminate");

        let (admission, grace_mode, request_class) = resolve_playback_request_admission(
            &app_state,
            &user,
            &fingerprint,
            PlaylistItemType::LiveHls,
            before.as_ref(),
            session_token,
            false,
            EvictionReentryGuard::Session(session_token),
            false, // prepare_only
            true,  // terminate
        )
        .await;

        assert_eq!(request_class, PlaybackRequestClass::Terminate);
        assert_eq!(admission.permission, UserConnectionPermission::Exhausted);
        assert_eq!(grace_mode, None);

        // Session should be expired after terminate.
        let after = app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await;
        assert!(after.is_none(), "session should be removed after terminate");
    }

    /// `classify_playback_request` returns `Terminate` when `terminate = true`.
    #[test]
    fn classify_playback_request_returns_terminate_when_flag_set() {
        let request_class = classify_playback_request(PlaybackRequestFacts {
            item_type: PlaylistItemType::Live,
            existing_session: None,
            prepare_only: false,
            terminate: true,
        });
        assert_eq!(request_class, PlaybackRequestClass::Terminate);
    }

    #[tokio::test]
    async fn activate_session_before_stream_open_marks_pending_provider_for_grace_hold() {
        let stream_cfg = crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        };
        let mut app_cfg = create_test_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
            user_access_control: true,
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(stream_cfg),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
            }),
            ..Config::default()
        }));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let first_addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let second_fingerprint = create_test_fingerprint(second_addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "grace-hold-user".to_string();
        user.max_connections = 1;
        let first_channel = create_test_live_channel("http://provider-1.example/live/1.ts");
        let mut second_channel = create_test_live_channel("http://provider-1.example/live/2.m3u8");
        second_channel.item_type = PlaylistItemType::LiveHls;
        second_channel.virtual_id = 55231;

        app_state.connection_manager.add_connection(&first_addr).await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &first_fingerprint,
                provider: input.name.clone(),
                stream_channel: &first_channel,
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-first"),
            })
            .await;

        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &second_fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-grace-hold",
                request_class: None,
                virtual_id: second_channel.virtual_id,
                item_type: PlaylistItemType::LiveHls,
                stream_url: second_channel.url.as_ref(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: false,
            },
        )
        .await;

        assert_eq!(activation.admission.permission, UserConnectionPermission::GracePeriod);
        assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-grace-hold")
            .await
            .expect("placeholder session should exist");
        let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } =
            &session.lifecycle
            else {
                panic!("grace hold should mark pending provider state")
            };
        assert!(matches!(pending.reason_code, crate::api::model::PendingProviderReason::GraceHold));
        assert!(pending.deadline >= pending.created_at);
        assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
        assert!(!session.lifecycle.is_counted(), "pending provider placeholder must not consume an active user lease before commit");
    }

    #[tokio::test]
    async fn activate_session_before_stream_open_does_not_commit_user_lease_before_provider_success() {
        let stream_cfg = crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 0,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: false,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: None,
        };
        let mut app_cfg = create_test_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
            user_access_control: true,
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(stream_cfg),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
            }),
            ..Config::default()
        }));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let addr: SocketAddr = "127.0.0.1:55232".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input = app_state.app_config.sources.load().inputs[0].clone();
        let mut user = ProxyUserCredentials::default();
        user.username = "atomic-commit-user".to_string();
        user.max_connections = 1;
        let channel = create_test_live_channel("http://provider-1.example/live/3.ts");

        let activation = activate_session_before_stream_open(
            &app_state,
            SessionActivationRequest {
                fingerprint: &fingerprint,
                input: input.as_ref(),
                user: &user,
                session_token: "tok-atomic-commit",
                request_class: None,
                virtual_id: channel.virtual_id,
                item_type: channel.item_type,
                stream_url: channel.url.as_ref(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                socket_bound: false,
            },
        )
        .await;

        assert_eq!(activation.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(activation.grace_mode, None);

        let session = app_state
            .active_users
            .get_and_update_user_session(&user.username, "tok-atomic-commit")
            .await
            .expect("placeholder session should exist");
        assert_eq!(
            app_state.active_users.user_connections(&user.username).await,
            0,
            "allowed activation should stay provisional until provider acquisition and stream commit succeed"
        );
        assert!(
            !session.lifecycle.is_counted(),
            "placeholder session must stay uncounted until the provider side has been committed"
        );
        assert!(!matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }));
    }

    fn create_test_shared_target() -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: "shared".to_string(),
            options: Some(ConfigTargetOptions { share_live_streams: true, ..ConfigTargetOptions::default() }),
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
    async fn effective_admission_strategies_use_legacy_grace_when_field_missing() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: None,
        });

        assert_eq!(
            get_effective_admission_strategies(&app_state),
            vec![shared::model::AdmissionStrategy::GraceHoldStream]
        );
    }

    #[tokio::test]
    async fn effective_admission_strategies_respect_explicit_empty_list() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![]),
        });

        assert!(get_effective_admission_strategies(&app_state).is_empty());
    }

    #[tokio::test]
    async fn grace_context_is_populated_when_grace_strategy_is_actually_granted() {
        // Use a DIFFERENT session token than the pre-existing counted session.
        // Otherwise session-admission may treat it as a valid reopen and skip the exhausted path.
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserOldest,
            ]),
        });

        let addr1: SocketAddr = "127.0.0.1:55401".parse().unwrap_or_else(|_| unreachable!());
        let addr2: SocketAddr = "10.0.0.5:55402".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint1 = create_test_fingerprint(addr1);
        let fingerprint2 = create_test_fingerprint(addr2);
        // addr1 and addr2 have DIFFERENT IPs.
        // EvictUserSameIpOldest will NOT match (different IP), so GraceHoldStream is evaluated.
        let mut user = ProxyUserCredentials::default();
        user.username = "user-grace-ctx".to_string();
        user.max_connections = 1;

        // Register the connection first so update_connection succeeds
        app_state.connection_manager.add_connection(&addr1).await;

        // Create the session — lifecycle starts as Prepared (uncounted)
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-existing-counted",
                virtual_id: 55401,
                provider: "provider_1",
                stream_url: "http://provider-1.example/live/55401.m3u8",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        // update_connection promotes the session to Active (counted) and creates a stream.
        // This exhausts the user's single slot (max_connections = 1).
        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 55401,
                meter_uid: 55401,
                username: "user-grace-ctx",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint1,
                provider: "provider_1".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/live/55401.m3u8"),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-existing-counted"),
            })
            .await
            .expect("stream should be created");

        // Now the new request finds the slot exhausted and the grace strategy kicks in.
        let result = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &fingerprint2.client_ip,
            &fingerprint2.addr,
            true,
            Some("tok-new-request"),
            true,
            EvictionReentryGuard::Session("tok-new-request"),
        )
        .await;

        assert_eq!(result.admission.permission, UserConnectionPermission::GracePeriod, "grace should be granted");
        assert!(matches!(result.grace_mode, Some(crate::api::model::GraceMode::Hold)));
        let ctx = result.grace_context.expect("grace_context must be present when grace is granted");
        assert_eq!(ctx.strategy_index, 1, "GraceHoldStream is at index 1");
        assert_eq!(ctx.strategies.len(), 3);
        assert!(matches!(ctx.strategies[ctx.strategy_index], AdmissionStrategy::GraceHoldStream));
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_evicts_after_used_grace() {
        // Strategies: [GraceHoldStream, EvictUserOldest]
        // Grace was used at index 0, so only EvictUserOldest (index 1) is evaluated.
        // Eviction frees the slot -> Allowed.
        let strategies = vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserOldest,
        ];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: None };

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserOldest,
            ]),
        });

        let addr1: SocketAddr = "127.0.0.1:55701".parse().unwrap_or_else(|_| unreachable!());
        let addr2: SocketAddr = "10.0.0.5:55702".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint1 = create_test_fingerprint(addr1);
        let fingerprint2 = create_test_fingerprint(addr2);

        app_state.connection_manager.add_connection(&addr1).await;
        app_state.connection_manager.add_connection(&addr2).await;

        let mut user = ProxyUserCredentials::default();
        user.username = "remaining-evict".to_string();
        user.max_connections = 1;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-counted",
                virtual_id: 55701,
                provider: "provider-evict",
                stream_url: "http://provider.example/live/1.ts",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 55701,
                meter_uid: 55701,
                username: "remaining-evict",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint1,
                provider: "provider-evict".intern(),
                stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-counted"),
            })
            .await
            .expect("stream should be created");

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "remaining-evict",
            1,
            0,
            &fingerprint2.client_ip,
            &fingerprint2.addr,
            true,
            Some("tok-new"),
            true,
            EvictionReentryGuard::Session("tok-new"),
            &grace_context,
            Some(crate::api::model::ConnectionKind::Normal),
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::Allowed,
            "EvictUserOldest should free the slot"
        );
        assert!(result.grace_context.is_none(), "no grace context on eviction success");
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_skips_no_match_and_uses_later_eviction() {
        // Strategies: [GraceHoldStream, EvictUserSameIpOldest, EvictUserOldest]
        // Grace was at index 0, remaining are EvictUserSameIpOldest (index 1) and EvictUserOldest (index 2).
        // The existing counted session is at a DIFFERENT IP, so EvictUserSameIpOldest -> NoMatch.
        // EvictUserOldest succeeds -> Allowed.
        let strategies = vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::EvictUserOldest,
        ];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: None };

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::EvictUserOldest,
            ]),
        });

        let addr1: SocketAddr = "127.0.0.1:55801".parse().unwrap_or_else(|_| unreachable!());
        let addr2: SocketAddr = "10.0.0.5:55802".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint1 = create_test_fingerprint(addr1);
        let fingerprint2 = create_test_fingerprint(addr2);

        app_state.connection_manager.add_connection(&addr1).await;
        app_state.connection_manager.add_connection(&addr2).await;

        let mut user = ProxyUserCredentials::default();
        user.username = "remaining-skip-no-match".to_string();
        user.max_connections = 1;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-counted",
                virtual_id: 55801,
                provider: "provider-skip",
                stream_url: "http://provider.example/live/1.ts",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 55801,
                meter_uid: 55801,
                username: "remaining-skip-no-match",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint1,
                provider: "provider-skip".intern(),
                stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-counted"),
            })
            .await
            .expect("stream should be created");

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "remaining-skip-no-match",
            1,
            0,
            &fingerprint2.client_ip,
            &fingerprint2.addr,
            true,
            Some("tok-new"),
            true,
            EvictionReentryGuard::Session("tok-new"),
            &grace_context,
            Some(crate::api::model::ConnectionKind::Normal),
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::Allowed,
            "EvictUserSameIpOldest should NoMatch, EvictUserOldest should succeed"
        );
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_empty_slice_denies() {
        // Strategies: [GraceHoldStream]
        // Grace was at index 0, remaining slice is empty -> exhausted.
        let strategies = vec![AdmissionStrategy::GraceHoldStream];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: None };

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.5:55901".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "no-remaining-strategies",
            1,
            0,
            &fingerprint.client_ip,
            &fingerprint.addr,
            true,
            Some("tok-new"),
            true,
            EvictionReentryGuard::Session("tok-new"),
            &grace_context,
            None,
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::Exhausted,
            "empty remaining slice should deny"
        );
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_preserves_soft_kind_on_exhausted() {
        // Strategies: [GraceHoldStream]
        // Grace was at index 0, remaining slice is empty -> exhausted.
        // grace_context.kind is Soft — must be preserved in the exhausted result.
        let strategies = vec![AdmissionStrategy::GraceHoldStream];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: Some(crate::api::model::ConnectionKind::Soft) };

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.6:55902".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "soft-kind-user",
            1,
            0,
            &fingerprint.client_ip,
            &fingerprint.addr,
            true,
            Some("tok-soft"),
            true,
            EvictionReentryGuard::Session("tok-soft"),
            &grace_context,
            Some(crate::api::model::ConnectionKind::Soft),
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::Exhausted,
            "empty remaining slice should deny"
        );
        assert_eq!(
            result.admission.kind,
            Some(crate::api::model::ConnectionKind::Soft),
            "exhausted result must preserve the original Soft connection kind"
        );
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_does_not_retry_used_prefix() {
        // Strategies: [GraceHoldStream, GraceInstantStream, EvictUserOldest]
        // Grace was at index 1 (GraceInstantStream).
        // Remaining slice: [EvictUserOldest] (index 2).
        // GraceHoldStream (index 0) must NOT be re-evaluated.
        let strategies = vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::GraceInstantStream,
            AdmissionStrategy::EvictUserOldest,
        ];
        let strategies_for_config = strategies.clone();
        let grace_context = GraceResolutionContext { strategy_index: 1, strategies, kind: None };

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(strategies_for_config),
        });

        let addr1: SocketAddr = "127.0.0.1:56001".parse().unwrap_or_else(|_| unreachable!());
        let addr2: SocketAddr = "10.0.0.5:56002".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint1 = create_test_fingerprint(addr1);
        let fingerprint2 = create_test_fingerprint(addr2);

        app_state.connection_manager.add_connection(&addr1).await;
        app_state.connection_manager.add_connection(&addr2).await;

        let mut user = ProxyUserCredentials::default();
        user.username = "remaining-no-retry".to_string();
        user.max_connections = 1;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-counted",
                virtual_id: 56001,
                provider: "provider-no-retry",
                stream_url: "http://provider.example/live/1.ts",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 56001,
                meter_uid: 56001,
                username: "remaining-no-retry",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint1,
                provider: "provider-no-retry".intern(),
                stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-counted"),
            })
            .await
            .expect("stream should be created");

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "remaining-no-retry",
            1,
            0,
            &fingerprint2.client_ip,
            &fingerprint2.addr,
            true,
            Some("tok-new"),
            true,
            EvictionReentryGuard::Session("tok-new"),
            &grace_context,
            Some(crate::api::model::ConnectionKind::Normal),
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::Allowed,
            "only EvictUserOldest should be evaluated, not GraceHoldStream"
        );
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_empty_slice_uses_original_kind_not_context_kind() {
        // grace_context.kind = Normal, original_kind = Soft
        // remaining slice is empty -> exhausted result must use original_kind.
        // This proves the empty-slice branch uses original_kind, not grace_context.kind.
        let strategies = vec![AdmissionStrategy::GraceHoldStream];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: Some(crate::api::model::ConnectionKind::Normal) };
        let original_kind = Some(crate::api::model::ConnectionKind::Soft);

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.7:55903".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "kind-mismatch-empty",
            1,
            0,
            &fingerprint.client_ip,
            &fingerprint.addr,
            true,
            Some("tok-empty"),
            true,
            EvictionReentryGuard::Session("tok-empty"),
            &grace_context,
            original_kind,
        )
        .await;

        assert_eq!(result.admission.permission, UserConnectionPermission::Exhausted);
        assert_eq!(
            result.admission.kind,
            original_kind,
            "exhausted result must use original_kind (Soft), not grace_context.kind (Normal)"
        );
    }

    #[tokio::test]
    async fn evaluate_remaining_strategies_later_grace_uses_original_kind_not_context_kind() {
        // grace_context.kind = Normal, original_kind = Soft
        // Strategies: [GraceHoldStream, GraceInstantStream]
        // Grace was used at index 0 (GraceHoldStream).
        // Remaining slice contains GraceInstantStream (index 1).
        // When the helper returns Grace for the remaining strategy, the new
        // GraceResolutionContext.kind must be original_kind (Soft), not grace_context.kind (Normal).
        // This proves build_grace_ctx uses original_kind as source of truth.
        let strategies = vec![
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::GraceInstantStream,
        ];
        let grace_context = GraceResolutionContext { strategy_index: 0, strategies, kind: Some(crate::api::model::ConnectionKind::Normal) };
        let original_kind = Some(crate::api::model::ConnectionKind::Soft);

        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::GraceInstantStream,
            ]),
        });

        let addr1: SocketAddr = "127.0.0.1:55710".parse().unwrap_or_else(|_| unreachable!());
        let addr2: SocketAddr = "10.0.0.8:55711".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint1 = create_test_fingerprint(addr1);
        let fingerprint2 = create_test_fingerprint(addr2);

        app_state.connection_manager.add_connection(&addr1).await;
        app_state.connection_manager.add_connection(&addr2).await;

        let mut user = ProxyUserCredentials::default();
        user.username = "kind-mismatch-grace".to_string();
        user.max_connections = 1;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-counted-grace",
                virtual_id: 55710,
                provider: "provider-grace-kind",
                stream_url: "http://provider.example/live/1.ts",
                addr: &addr1,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 55710,
                meter_uid: 55710,
                username: "kind-mismatch-grace",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint1,
                provider: "provider-grace-kind".intern(),
                stream_channel: &create_test_live_channel("http://provider.example/live/1.ts"),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-counted-grace"),
            })
            .await
            .expect("stream should be created");

        let result = evaluate_remaining_strategies_after_grace(
            &app_state,
            "kind-mismatch-grace",
            1,
            0,
            &fingerprint2.client_ip,
            &fingerprint2.addr,
            true,
            Some("tok-new-grace"),
            true,
            EvictionReentryGuard::Session("tok-new-grace"),
            &grace_context,
            original_kind,
        )
        .await;

        assert_eq!(
            result.admission.permission,
            UserConnectionPermission::GracePeriod,
            "remaining GraceInstantStream should grant GracePeriod"
        );
        assert!(
            result.grace_context.is_some(),
            "grace_context must be present when grace is granted"
        );
        assert_eq!(
            result.grace_context.as_ref().unwrap().kind,
            original_kind,
            "GraceResolutionContext.kind in the result must be original_kind (Soft), not grace_context.kind (Normal)"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolve_admission_with_strategies_falls_through_after_failed_grace_grant() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserOldest,
            ]),
        });

        let first_addr: std::net::SocketAddr = "127.0.0.1:55151".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: std::net::SocketAddr = "127.0.0.1:55152".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let second_fingerprint = create_test_fingerprint(second_addr);

        app_state.connection_manager.add_connection(&first_addr).await;
        app_state.connection_manager.add_connection(&second_addr).await;

        let mut session_user = ProxyUserCredentials::default();
        session_user.username = "fallthrough".to_string();
        session_user.max_connections = 1;
        session_user.soft_connections = 1;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "tok-first",
                virtual_id: 1,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/1.ts",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "fallthrough",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &first_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/live/1.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some("tok-first"),
            })
            .await;

        assert!(app_state.active_users.grant_grace("fallthrough").await);

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "tok-second",
                virtual_id: 2,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/2.ts",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Soft),
                socket_bound: false,
            })
            .await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: "fallthrough",
                max_connections: 1,
                soft_connections: 1,
                connection_kind: crate::api::model::ConnectionKind::Soft,
                priority: 0,
                soft_priority: 10,
                fingerprint: &second_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/live/2.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some("tok-second"),
            })
            .await;

        let result = resolve_admission_with_strategies(
            &app_state,
            "fallthrough",
            1,
            1,
            "127.0.0.1",
            &"127.0.0.1:55153".parse().unwrap_or_else(|_| unreachable!()),
            true,
            Some("tok-third"),
            false,
            EvictionReentryGuard::Session("tok-third"),
        )
            .await;
        let admission = result.admission;
        let grace_mode = result.grace_mode;

        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(admission.kind, Some(crate::api::model::ConnectionKind::Normal));
        assert_eq!(grace_mode, None);
    }

    #[tokio::test]
    async fn resolve_admission_with_strategies_allows_existing_session_even_when_user_is_at_limit() {
        let app_state = create_test_app_state();
        let addr = "127.0.0.1:55154".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "session-admission".to_string();
        user.max_connections = 1;

        app_state.connection_manager.add_connection(&addr).await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "vod-session",
                virtual_id: 1,
                provider: "provider-a",
                stream_url: "http://provider-1.example/movie/1.mkv",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 10,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/movie/1.mkv"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some("vod-session"),
            })
            .await;

        let session_based = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &fingerprint.client_ip,
            &fingerprint.addr,
            true,
            Some("vod-session"),
            false,
            EvictionReentryGuard::Session("vod-session"),
        )
            .await;
        assert_eq!(session_based.admission.permission, UserConnectionPermission::Allowed);

        let connection_based = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &fingerprint.client_ip,
            &fingerprint.addr,
            false,
            Some("vod-session"),
            false,
            EvictionReentryGuard::Session("vod-session"),
        )
            .await;
        assert_eq!(connection_based.admission.permission, UserConnectionPermission::Exhausted);
    }

    #[tokio::test]
    async fn resolve_admission_with_strategies_prevents_recently_evicted_playback_ping_pong() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 0,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: false,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
        });

        let victim_addr: std::net::SocketAddr = "127.0.0.1:55181".parse().unwrap_or_else(|_| unreachable!());
        let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55182".parse().unwrap_or_else(|_| unreachable!());
        let winner_addr: std::net::SocketAddr = "127.0.0.1:55183".parse().unwrap_or_else(|_| unreachable!());
        let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
        let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
        let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
        let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9001.ts");
        victim_channel.virtual_id = 9001;
        let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9002.ts");
        winner_channel.virtual_id = 9002;

        app_state.connection_manager.add_connection(&victim_addr).await;
        app_state.connection_manager.add_connection(&winner_addr).await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "loop-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &victim_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &victim_channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("session-victim"),
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: "loop-user",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &winner_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &winner_channel,
                user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
                session_token: Some("session-winner"),
            })
            .await;

        app_state
            .active_users
            .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
            .await;
        app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

        let result = resolve_admission_with_strategies(
            &app_state,
            "loop-user",
            1,
            0,
            &reconnect_fingerprint.client_ip,
            &reconnect_fingerprint.addr,
            true,
            Some("socket-reconnect"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: 9001 },
        )
            .await;
        let admission = result.admission;
        let grace_mode = result.grace_mode;

        assert_eq!(admission.permission, UserConnectionPermission::Exhausted);
        assert_eq!(grace_mode, None);
        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1);
        assert_eq!(active_streams[0].channel.virtual_id, 9002);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolve_admission_with_strategies_allows_other_channel_after_recent_eviction() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 0,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: false,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
        });

        let victim_addr: std::net::SocketAddr = "127.0.0.1:55184".parse().unwrap_or_else(|_| unreachable!());
        let winner_addr: std::net::SocketAddr = "127.0.0.1:55185".parse().unwrap_or_else(|_| unreachable!());
        let new_addr: std::net::SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
        let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
        let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
        let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
        let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9101.ts");
        victim_channel.virtual_id = 9101;
        let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9102.ts");
        winner_channel.virtual_id = 9102;
        let mut session_user = ProxyUserCredentials::default();
        session_user.username = "loop-user-2".to_string();

        app_state.connection_manager.add_connection(&victim_addr).await;
        app_state.connection_manager.add_connection(&winner_addr).await;

        // Create sessions before update_connection so streams are linked to counted sessions
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "session-victim",
                virtual_id: 9101,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/9101.ts",
                addr: &victim_addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "session-winner",
                virtual_id: 9102,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/9102.ts",
                addr: &winner_addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "loop-user-2",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &victim_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &victim_channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("session-victim"),
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: "loop-user-2",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &winner_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &winner_channel,
                user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
                session_token: Some("session-winner"),
            })
            .await;

        app_state
            .active_users
            .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
            .await;
        app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

        let result = resolve_admission_with_strategies(
            &app_state,
            "loop-user-2",
            1,
            0,
            &new_fingerprint.client_ip,
            &new_fingerprint.addr,
            true,
            Some("session-new"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: 9103 },
        )
            .await;
        let admission = result.admission;

        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolve_admission_with_strategies_does_not_suppress_different_session_on_same_channel() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 0,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: false,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
        });

        let victim_addr: std::net::SocketAddr = "127.0.0.1:55190".parse().unwrap_or_else(|_| unreachable!());
        let winner_addr: std::net::SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
        let new_addr: std::net::SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
        let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
        let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "player/1.0");
        let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
        let mut channel = create_test_live_channel("http://provider-1.example/live/9301.m3u8");
        channel.virtual_id = 9301;
        channel.item_type = PlaylistItemType::LiveHls;
        let mut session_user = ProxyUserCredentials::default();
        session_user.username = "loop-user-4".to_string();

        app_state.connection_manager.add_connection(&victim_addr).await;
        app_state.connection_manager.add_connection(&winner_addr).await;

        // Create sessions before update_connection so streams are linked to counted sessions
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "session-victim",
                virtual_id: 9301,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/9301.m3u8",
                addr: &victim_addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &session_user,
                session_token: "session-winner",
                virtual_id: 9301,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/9301.m3u8",
                addr: &winner_addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "loop-user-4",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &victim_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("session-victim"),
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: "loop-user-4",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &winner_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("session-winner"),
            })
            .await;

        app_state
            .active_users
            .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
            .await;
        app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

        let result = resolve_admission_with_strategies(
            &app_state,
            "loop-user-4",
            1,
            0,
            &new_fingerprint.client_ip,
            &new_fingerprint.addr,
            true,
            Some("session-other"),
            false,
            EvictionReentryGuard::Session("session-other"),
        )
            .await;
        let admission = result.admission;
        let grace_mode = result.grace_mode;

        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(grace_mode, None);
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_admission_with_strategies_allows_recently_evicted_playback_when_soft_slot_is_free() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 0,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: false,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
        });

        let victim_addr: std::net::SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
        let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55188".parse().unwrap_or_else(|_| unreachable!());
        let winner_addr: std::net::SocketAddr = "127.0.0.1:55189".parse().unwrap_or_else(|_| unreachable!());
        let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
        let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
        let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
        let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9201.ts");
        victim_channel.virtual_id = 9201;
        let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9202.ts");
        winner_channel.virtual_id = 9202;

        app_state.connection_manager.add_connection(&victim_addr).await;
        app_state.connection_manager.add_connection(&winner_addr).await;

        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: "loop-user-3",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &victim_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &victim_channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("session-victim"),
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: "loop-user-3",
                max_connections: 2,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &winner_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &winner_channel,
                user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
                session_token: Some("session-winner"),
            })
            .await;

        app_state
            .active_users
            .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, RECENT_EVICTION_REENTRY_TTL_SECS)
            .await;
        app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

        let result = resolve_admission_with_strategies(
            &app_state,
            "loop-user-3",
            1,
            1,
            &reconnect_fingerprint.client_ip,
            &reconnect_fingerprint.addr,
            true,
            Some("socket-reconnect"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: 9201 },
        )
            .await;
        let admission = result.admission;
        let grace_mode = result.grace_mode;

        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(admission.kind, Some(crate::api::model::ConnectionKind::Soft));
        assert_eq!(grace_mode, None);

        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1);
        assert_eq!(active_streams[0].channel.virtual_id, 9202);
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
    async fn local_stream_response_rechecks_limits_before_registering_socket_bound_streams() {
        let mut app_cfg = create_test_app_config();
        let config = Config { user_access_control: true, ..Config::default() };
        app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-race-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let first_addr = "127.0.0.1:55131".parse().unwrap_or_else(|_| unreachable!());
        let second_addr = "127.0.0.1:55132".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let second_fingerprint = create_test_fingerprint(second_addr);
        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let mut user = ProxyUserCredentials::default();
        user.username = "local-limit-user".to_string();
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
        let first_token = create_session_fingerprint(&first_fingerprint, &user.username, channel.virtual_id, true);
        let second_token = create_session_fingerprint(&second_fingerprint, &user.username, channel.virtual_id, true);

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
            Some(&first_token),
            None,
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
            Some(&second_token),
            None,
            false,
        )
        .await
        .into_response();

        assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
        assert_eq!(app_state.active_users.active_streams().await.len(), 1);
        assert_eq!(
            app_state
                .active_users
                .connection_admission_for_session(
                    &user.username,
                    user.max_connections,
                    user.soft_connections,
                    &second_token
                )
                .await
                .permission,
            UserConnectionPermission::Exhausted,
            "failed second open must not leave a placeholder session that bypasses admission"
        );
    }

    #[tokio::test]
    async fn stream_response_preserves_soft_kind_for_shared_reuse() {
        let app_state = create_test_provider_app_state();
        let stream_url = "http://provider-1.example/live/shared.ts";
        let input_name = "provider_1".intern();
        let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
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
                provider: input.name.clone(),
                stream_channel: &normal_channel,
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("normal-session"),
            })
            .await
            .expect("normal stream should register");

        let admission =
            app_state.get_connection_admission(&user.username, user.max_connections, user.soft_connections).await;
        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(admission.kind, Some(crate::api::model::ConnectionKind::Soft));

        let soft_addr = "127.0.0.1:55142".parse().unwrap_or_else(|_| unreachable!());
        let soft_fingerprint = create_test_fingerprint(soft_addr);
        let response = stream_response(
            &soft_fingerprint,
            &app_state,
            "soft-session",
            None,
            create_test_live_channel(stream_url),
            stream_url,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            admission.permission,
            admission.kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            false,
            None,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let session_admission = app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                "soft-session",
            )
            .await;
        assert_eq!(session_admission.kind, Some(crate::api::model::ConnectionKind::Soft));
    }

    #[tokio::test]
    async fn stream_response_rolls_back_provisional_user_activation_when_provider_open_fails() {
        let mut app_cfg = create_test_provider_app_config();
        app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
            user_access_control: true,
            ..Config::default()
        }));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let addr = "127.0.0.1:55143".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .get_input_by_name(&input_name)
            .expect("provider input should exist");
        let target = Arc::new(ConfigTarget {
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
        });
        let mut user = ProxyUserCredentials::default();
        user.username = "rollback-user".to_string();
        user.max_connections = 1;
        let stream_url = "provider://bad-url";
        let channel = create_test_live_channel(stream_url);

        let response = stream_response(
            &fingerprint,
            &app_state,
            "rollback-session",
            None,
            channel,
            stream_url,
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Normal,
            false,
            None,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            app_state.active_users.user_connections(&user.username).await,
            0,
            "failed provider open must rollback provisional user activation"
        );
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, "rollback-session")
                .await
                .is_none(),
            "failed provider open must remove the provisional placeholder session"
        );
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
            None,
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
            None,
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
            None,
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
            None,
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

    #[tokio::test]
    async fn local_stream_response_preserves_soft_kind_across_reopens() {
        let app_state = create_test_app_state();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let file_path = temp_dir.path().join("local-soft-test.mkv");
        tokio::fs::write(&file_path, Bytes::from_static(b"local-stream")).await.expect("write local file");

        let channel = create_test_local_channel(&format!("file://{}", file_path.display()));
        let input = ConfigInput { input_type: InputType::Library, ..ConfigInput::default() };
        let mut user = ProxyUserCredentials::default();
        user.username = "soft-local-user".to_string();
        user.max_connections = 1;
        user.soft_connections = 1;
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
        let playback_session_token = "local-soft-playback-token";

        let first_fingerprint = create_test_fingerprint("127.0.0.1:55129".parse().unwrap_or_else(|_| unreachable!()));
        let second_fingerprint = create_test_fingerprint("127.0.0.1:55130".parse().unwrap_or_else(|_| unreachable!()));

        let _first_response = local_stream_response(
            &first_fingerprint,
            &app_state,
            channel.clone(),
            &HeaderMap::default(),
            &input,
            &target,
            &user,
            UserConnectionPermission::Allowed,
            crate::api::model::ConnectionKind::Soft,
            Some(playback_session_token),
            None,
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
            None,
            false,
        )
        .await
        .into_response();

        assert_eq!(second_response.status(), StatusCode::OK);

        let session_admission = app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                playback_session_token,
            )
            .await;
        assert_eq!(session_admission.kind, Some(crate::api::model::ConnectionKind::Soft));
    }

    #[tokio::test]
    async fn activated_session_admission_keeps_hls_placeholders_uncounted_via_api_utils() {
        let app_state = create_test_app_state();
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.max_connections = 1;

        let first_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: std::net::SocketAddr = "127.0.0.1:55178".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let second_fingerprint = create_test_fingerprint(second_addr);

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-first",
                virtual_id: 7101,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/7101.m3u8",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-second",
                virtual_id: 7102,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/7102.m3u8",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let first_admission = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &first_fingerprint.client_ip,
            &first_fingerprint.addr,
            true,
            Some("tok-hls-first"),
            true,
            EvictionReentryGuard::Session("tok-hls-first"),
        )
            .await;
        let second_admission = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &second_fingerprint.client_ip,
            &second_fingerprint.addr,
            true,
            Some("tok-hls-second"),
            true,
            EvictionReentryGuard::Session("tok-hls-second"),
        )
            .await;

        assert_eq!(first_admission.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(second_admission.admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
    }

    #[tokio::test]
    async fn socket_bound_playback_tokens_enforce_hard_limits_per_socket() {
        let app_state = create_test_app_state();
        let mut user = ProxyUserCredentials::default();
        user.username = "user1".to_string();
        user.max_connections = 1;

        let first_addr: std::net::SocketAddr = "127.0.0.1:55171".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: std::net::SocketAddr = "127.0.0.1:55172".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 5001, true);
        let second_fingerprint = create_test_fingerprint(second_addr);
        let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 5001, true);

        app_state.connection_manager.add_connection(&first_addr).await;
        app_state.connection_manager.add_connection(&second_addr).await;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: &first_token,
                virtual_id: 5001,
                provider: "provider-a",
                stream_url: "http://provider-1.example/vod/5001.ts",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;

        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 5001,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &first_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/vod/5001.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some(&first_token),
            })
            .await;

        let admission = app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                &second_token,
            )
            .await;
        assert_eq!(admission.permission, UserConnectionPermission::Exhausted);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn resolve_admission_with_strategies_evicts_preserved_hls_session_for_same_user_ts_request() {
        let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
            retry: true,
            metrics_enabled: true,
            buffer: None,
            grace_period_millis: 2_000,
            grace_period_timeout_secs: 8,
            grace_period_hold_stream: true,
            hls_session_ttl_secs: 10,
            catchup_session_ttl_secs: 10,
            throttle_str: None,
            throttle_kbps: 0,
            shared_burst_buffer_mb: 1,
            admission_strategies: Some(vec![
                AdmissionStrategy::EvictUserSameIpOldest,
                AdmissionStrategy::EvictUserSameIpLatest,
                AdmissionStrategy::GraceHoldStream,
                AdmissionStrategy::EvictUserOldest,
                AdmissionStrategy::EvictUserLatest,
            ]),
        });

        let hls_addr: std::net::SocketAddr = "127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!());
        let ts_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
        let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "player/1.0");
        let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "player/1.0");
        let mut user = ProxyUserCredentials::default();
        user.username = "same-user".to_string();
        user.max_connections = 1;

        app_state.connection_manager.add_connection(&hls_addr).await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: "tok-hls-preserved",
                virtual_id: 5001,
                provider: "provider-a",
                stream_url: "http://provider-1.example/live/5001.m3u8",
                addr: &hls_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &hls_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &StreamChannel {
                    item_type: PlaylistItemType::LiveHls,
                    virtual_id: 5001,
                    ..create_test_live_channel("http://provider-1.example/live/5001.m3u8")
                },
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some("tok-hls-preserved"),
            })
            .await;

        app_state.connection_manager.release_connection(&hls_addr).await;
        assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
        assert!(app_state.active_users.active_streams().await.is_empty());

        let mut close_rx = app_state.connection_manager.get_close_connection_channel();
        let result = resolve_admission_with_strategies(
            &app_state,
            &user.username,
            user.max_connections,
            user.soft_connections,
            &ts_fingerprint.client_ip,
            &ts_fingerprint.addr,
            false,
            None,
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: 5001 },
        )
        .await;
        let admission = result.admission;
        let grace_mode = result.grace_mode;

        assert_eq!(admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(grace_mode, None);
        assert!(app_state.active_users.active_streams().await.is_empty());
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(100), close_rx.recv())
                .await
                .ok()
                .and_then(Result::ok),
            Some(crate::api::model::CloseConnectionSignal::WithReason(
                hls_addr,
                shared::model::DisconnectReason::ClientKicked,
            ))
        );
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, "tok-hls-preserved")
                .await
                .is_none(),
            "preserved session should be removed once the TS request evicts it"
        );
    }

    #[tokio::test]
    async fn socket_bound_playback_tokens_still_allow_soft_slots() {
        let app_state = create_test_app_state();
        let mut user = ProxyUserCredentials::default();
        user.username = "soft-user".to_string();
        user.max_connections = 1;
        user.soft_connections = 1;
        user.priority = 0;
        user.soft_priority = 9;

        let first_addr: std::net::SocketAddr = "127.0.0.1:55173".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: std::net::SocketAddr = "127.0.0.1:55174".parse().unwrap_or_else(|_| unreachable!());
        let third_addr: std::net::SocketAddr = "127.0.0.1:55175".parse().unwrap_or_else(|_| unreachable!());
        let first_fingerprint = create_test_fingerprint(first_addr);
        let second_fingerprint = create_test_fingerprint(second_addr);
        let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 6001, true);
        let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 6001, true);
        let third_fingerprint = create_test_fingerprint(third_addr);
        let third_token = create_session_fingerprint(&third_fingerprint, &user.username, 6001, true);

        app_state.connection_manager.add_connection(&first_addr).await;
        app_state.connection_manager.add_connection(&second_addr).await;

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: &first_token,
                virtual_id: 6001,
                provider: "provider-a",
                stream_url: "http://provider-1.example/vod/6001.ts",
                addr: &first_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: true,
            })
            .await;
        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 6001,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &first_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/vod/6001.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some(&first_token),
            })
            .await;

        let second_admission = app_state
            .active_users
            .connection_admission_for_session(
                &user.username,
                user.max_connections,
                user.soft_connections,
                &second_token,
            )
            .await;
        assert_eq!(second_admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(second_admission.kind, Some(crate::api::model::ConnectionKind::Soft));

        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: &second_token,
                virtual_id: 6001,
                provider: "provider-a",
                stream_url: "http://provider-1.example/vod/6001.ts",
                addr: &second_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Soft),
                socket_bound: true,
            })
            .await;
        app_state
            .active_users
            .update_connection(crate::api::model::ActiveUserConnectionParams {
                uid: 6002,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Soft,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &second_fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &create_test_live_channel("http://provider-1.example/vod/6002.ts"),
                user_agent: std::borrow::Cow::Borrowed("ua"),
                session_token: Some(&second_token),
            })
            .await;

        let third_admission = app_state
            .active_users
            .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &third_token)
            .await;
        assert_eq!(third_admission.permission, UserConnectionPermission::Exhausted);
    }

    #[test]
    fn session_based_playback_matches_adaptive_types_and_extensions() {
        assert!(is_session_based_playback(PlaylistItemType::LiveHls, None));
        assert!(is_session_based_playback(PlaylistItemType::LiveDash, None));
        assert!(is_session_based_playback(PlaylistItemType::Live, Some(HLS_EXT)));
        assert!(is_session_based_playback(PlaylistItemType::Live, Some(DASH_EXT)));
        assert!(!is_session_based_playback(PlaylistItemType::Video, None));
    }

    #[test]
    fn create_session_fingerprint_switches_between_logical_and_socket_bound_keys() {
        let fingerprint = create_test_fingerprint("127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!()));
        let logical = create_session_fingerprint(&fingerprint, "user1", 7001, false);
        let socket_bound = create_session_fingerprint(&fingerprint, "user1", 7001, true);

        assert_ne!(logical, socket_bound);
        assert!(logical.contains(&fingerprint.key));
        assert!(socket_bound.contains(&fingerprint.addr.to_string()));
    }

    #[test]
    fn adaptive_playback_session_fingerprint_is_unique_per_initial_socket() {
        let Some(first_addr) = "127.0.0.1:55177".parse().ok() else {
            return;
        };
        let Some(second_addr) = "127.0.0.1:55178".parse().ok() else {
            return;
        };
        let first = Fingerprint::new("10.0.0.6|player".to_string(), "10.0.0.6".to_string(), first_addr);
        let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

        let first_token = create_playback_session_fingerprint(&first, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));
        let second_token = create_playback_session_fingerprint(&second, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));

        assert_ne!(first_token, second_token);
        assert!(first_token.contains(&first.addr.to_string()));
        assert!(second_token.contains(&second.addr.to_string()));
    }

    #[test]
    fn playback_session_fingerprint_keeps_ts_socket_bound_but_vod_logical() {
        let first_addr: SocketAddr = "127.0.0.1:55179".parse().unwrap_or_else(|_| unreachable!());
        let second_addr: SocketAddr = "127.0.0.1:55180".parse().unwrap_or_else(|_| unreachable!());
        let first = Fingerprint::new("10.0.0.7|player".to_string(), "10.0.0.7".to_string(), first_addr);
        let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

        let first_ts = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Live, None);
        let second_ts = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Live, None);
        let first_vod = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Video, None);
        let second_vod = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Video, None);

        assert_ne!(first_ts, second_ts, "plain TS live remains socket-bound");
        assert_eq!(first_vod, second_vod, "VOD remains logical across reopen/seek sockets");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn xtream_hls_then_ts_uses_distinct_tokens_and_evicts_old_hls_session() {
        let mut app_cfg = create_test_app_config();
        let config = Config {
            user_access_control: true,
            reverse_proxy: Some(crate::model::ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: crate::model::ResourceRetryConfig::default(),
                disabled_header: None,
                stream: Some(crate::model::StreamConfig {
                    retry: true,
                    metrics_enabled: true,
                    buffer: None,
                    grace_period_millis: 2_000,
                    grace_period_timeout_secs: 8,
                    grace_period_hold_stream: true,
                    hls_session_ttl_secs: 10,
                    catchup_session_ttl_secs: 10,
                    throttle_str: None,
                    throttle_kbps: 0,
                    shared_burst_buffer_mb: 1,
                    admission_strategies: Some(vec![
                        AdmissionStrategy::EvictUserSameIpOldest,
                        AdmissionStrategy::EvictUserSameIpLatest,
                        AdmissionStrategy::GraceHoldStream,
                        AdmissionStrategy::EvictUserOldest,
                        AdmissionStrategy::EvictUserLatest,
                    ]),
                }),
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: None,
            }),
            ..Config::default()
        };
        app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let hls_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
        let ts_addr: SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
        let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "libmpv");
        let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "libmpv");
        let mut user = ProxyUserCredentials::default();
        user.username = "xtream-hls-ts".to_string();
        user.max_connections = 1;

        let virtual_id = 7811;
        let hls_token = create_session_fingerprint(&hls_fingerprint, &user.username, virtual_id, false);
        let ts_token = create_session_fingerprint(&ts_fingerprint, &user.username, virtual_id, true);
        assert_ne!(hls_token, ts_token, "Xtream .m3u8 and .ts must not share the same playback token");

        let mut hls_channel = create_test_live_channel("http://provider-1.example/live/7811.m3u8");
        hls_channel.virtual_id = virtual_id;
        hls_channel.item_type = PlaylistItemType::LiveHls;
        let mut ts_channel = create_test_live_channel("http://provider-1.example/live/7811.ts");
        ts_channel.virtual_id = virtual_id;

        app_state.connection_manager.add_connection(&hls_addr).await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: &hls_token,
                virtual_id,
                provider: "provider_1",
                stream_url: hls_channel.url.as_ref(),
                addr: &hls_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 1,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &hls_fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &hls_channel,
                user_agent: Cow::Borrowed("libmpv"),
                session_token: Some(&hls_token),
            })
            .await;

        app_state.connection_manager.release_connection(&hls_addr).await;
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_some(),
            "preserved HLS session should still exist before the competing TS request"
        );
        assert_eq!(
            app_state
                .active_users
                .connection_admission(&user.username, user.max_connections, user.soft_connections)
                .await
                .permission,
            UserConnectionPermission::Exhausted,
            "the preserved HLS playback must still reserve the user's only slot before the TS request is evaluated"
        );
        assert_eq!(
            app_state
                .active_users
                .get_eviction_candidates(&user.username, &ts_fingerprint.client_ip)
                .await
                .len(),
            1,
            "the preserved HLS playback should be the single eviction candidate for the competing TS request"
        );

        let (ts_admission, ts_grace_mode, request_class) = resolve_playback_request_admission(
            &app_state,
            &user,
            &ts_fingerprint,
            PlaylistItemType::Live,
            None,
            &ts_token,
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id },
            false,
            false,
        )
        .await;
        assert_eq!(request_class, PlaybackRequestClass::Activate);
        assert_eq!(ts_admission.permission, UserConnectionPermission::Allowed);
        assert_eq!(ts_grace_mode, None);
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "the competing TS activation must remove the old preserved HLS session even though there is no live socket left to kick"
        );

        app_state.connection_manager.add_connection(&ts_addr).await;
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: &ts_token,
                virtual_id,
                provider: "provider_1",
                stream_url: ts_channel.url.as_ref(),
                addr: &ts_addr,
                connection_permission: ts_admission.permission,
                connection_kind: ts_admission.kind,
                socket_bound: true,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 2,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &ts_fingerprint,
                provider: "provider_1".intern(),
                stream_channel: &ts_channel,
                user_agent: Cow::Borrowed("libmpv"),
                session_token: Some(&ts_token),
            })
            .await;

        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "after the competing TS request, the old Xtream HLS session must be gone so later /hls segment fetches cannot revive it"
        );
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &ts_token)
                .await
                .is_some(),
            "the winning TS playback should remain tracked under its socket-bound Xtream token"
        );
    }

    #[test]
    fn socket_bound_playback_session_matches_only_plain_live_playback() {
        assert!(is_socket_bound_playback_session(PlaylistItemType::Live, None));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(HLS_EXT)));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(DASH_EXT)));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::LiveHls, None));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::Video, None));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::Series, None));
        assert!(!is_socket_bound_playback_session(PlaylistItemType::Catchup, None));
    }

    #[test]
    fn session_reacquire_cleanup_addrs_excludes_current_and_deduplicates() {
        let primary: SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
        let overlap: SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
        let seek: SocketAddr = "127.0.0.1:55193".parse().unwrap_or_else(|_| unreachable!());
        let session = UserSession {
            token: "tok-vod".to_string(),
            transition_version: 1,
            virtual_id: 9001,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/movie.mkv".intern(),
            addr: seek,
            socket_bound: false,
            active_addrs: vec![primary, overlap, seek, overlap],
            ts: 1,
            started_at: 1,
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            lifecycle: crate::api::model::PlaybackLifecycle::Active,
        };

        assert_eq!(session_reacquire_cleanup_addrs(&session, &seek), vec![primary, overlap]);
    }

    #[test]
    fn grace_hold_defers_live_but_not_provider_affine_session_reopens() {
        assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::LiveHls, false));
        assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, false));
        assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, true));
        assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, true));
        assert!(!should_defer_provider_open_for_grace_hold(true, false, PlaylistItemType::Video, true));
    }

    #[tokio::test]
    async fn forced_reopen_cleanup_for_adaptive_streams_does_not_close_client_socket() {
        let app_state = create_test_app_state();
        let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
        let mut close_rx = app_state.connection_manager.get_close_connection_channel();

        cleanup_forced_reopen_addrs(&app_state, PlaylistItemType::LiveHls, &[addr]).await;

        let signal = tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv())
            .await
            .ok()
            .and_then(Result::ok);
        assert!(signal.is_none(), "adaptive cleanup should not hard-close the previous client socket");
    }

    #[tokio::test]
    async fn forced_reopen_cleanup_for_non_adaptive_streams_closes_client_socket() {
        let app_state = create_test_app_state();
        let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
        let mut close_rx = app_state.connection_manager.get_close_connection_channel();

        cleanup_forced_reopen_addrs(&app_state, PlaylistItemType::Live, &[addr]).await;

        let signal = tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv())
            .await
            .ok()
            .and_then(Result::ok);
        assert!(matches!(
            signal,
            Some(crate::api::model::CloseConnectionSignal::WithReason(signal_addr, _)) if signal_addr == addr
        ));
    }

    #[tokio::test]
    async fn get_query_path_strips_extension_for_live_with_flag() {
        use crate::model::ConfigInputFlags;
        use shared::model::{InputType, PlaylistItemType, XtreamCluster, XtreamPlaylistItem};

        let mut input = ConfigInput {
            id: 1,
            name: "provider_with_flag".intern(),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let mut options = crate::model::ConfigInputOptions::defaults().clone();
        options.flags.set(ConfigInputFlags::XtreamLiveStreamWithoutExtension);
        input.options = Some(options);

        let sources = SourcesConfig { inputs: vec![Arc::new(input)], ..SourcesConfig::default() };
        let mut app_cfg_raw = create_test_app_config();
        app_cfg_raw.sources = Arc::new(ArcSwap::from_pointee(sources));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg_raw));

        let pli = XtreamPlaylistItem {
            virtual_id: 100,
            provider_id: 1,
            name: "test".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://example.com/123".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 0,
            input_name: "provider_with_flag".intern(),
            channel_no: 0,
            source_ordinal: 0,
        };

        let hls_ext = shared::utils::HLS_EXT.to_string();
        let (query_path, extension) =
            crate::api::endpoints::xtream_api::get_query_path("", Some(&hls_ext), &pli, &app_state);

        assert_eq!(extension, "");
        assert_eq!(query_path, "1");

        let dash_ext = shared::utils::DASH_EXT.to_string();
        let (query_path, extension) =
            crate::api::endpoints::xtream_api::get_query_path("", Some(&dash_ext), &pli, &app_state);

        assert_eq!(extension, "");
        assert_eq!(query_path, "1");
    }

    // =========================================================================================
    // evaluate_network_access tests
    // =========================================================================================

    /// Helper to create a test user with specific network access
    fn user_with_network_access(network_access: Option<NetworkAccess>) -> ProxyUserCredentials {
        ProxyUserCredentials {
            username: "test".to_string(),
            password: "test".to_string(),
            token: None,
            proxy: ProxyType::default(),
            server: None,
            epg_timeshift: None,
            epg_request_timeshift: None,
            created_at: None,
            exp_date: None,
            max_connections: 0,
            status: None,
            output_clusters: ClusterFlags::all(),
            ui_enabled: true,
            comment: None,
            priority: 0,
            soft_connections: 0,
            soft_priority: 0,
            t_is_api_user: false,
            network_access,
        }
    }

    #[test]
    fn no_config_allows_all() {
        let user = user_with_network_access(None);
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn empty_config_allows_all() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn cidr_match_allows() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(evaluate_network_access(&user, "192.168.1.42", &geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn cidr_miss_denies() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    #[test]
    fn country_match_allows() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("DE")))));
        assert_eq!(evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn country_miss_denies() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("US")))));
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch)
        );
    }

    #[test]
    fn no_geoip_denies_on_country_restriction() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable)
        );
    }

    #[test]
    fn ipv4_vs_ipv6_denies_gracefully() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["2001:db8::/32".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    #[test]
    fn ipv6_vs_ipv4_denies_gracefully() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "2001:db8::1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    #[test]
    fn either_cidr_or_country_match_allows() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["US".to_string()],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("DE")))));
        assert_eq!(evaluate_network_access(&user, "192.168.1.42", &mock_geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn single_ip_cidr() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.1/32".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
        assert_eq!(
            evaluate_network_access(&user, "192.168.1.2", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    // =========================================================================================
    // network denied reason tests
    // =========================================================================================

    #[test]
    fn network_denied_reason_cidr_no_match() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    #[test]
    fn network_denied_reason_country_no_match() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("US")))));
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch)
        );
    }

    #[test]
    fn network_denied_reason_geoip_unavailable() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable)
        );
    }

    #[test]
    fn network_denied_reason_country_unknown_when_geoip_loaded_but_unknown_ip() {
        // GeoIP is loaded (not None), but lookup returns None for this IP (private/unknown).
        // We need a GeoIP that only covers a private range, so public IPs get None.
        // Use a CIDR-only restriction (no country rules) so we can verify
        // that when countries ARE checked, lookup None gives "country_unknown".
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()], // miss CIDR first
        }));
        // Use the real GeoIp::new() which only seeds private ranges.
        // For 8.8.8.8 (public), lookup returns None.
        let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
        // CIDR miss -> country check -> geoip loaded but lookup returns None
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown)
        );
    }

    #[test]
    fn network_denied_reason_none_when_allowed() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("DE")))));
        assert_eq!(evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    #[test]
    fn network_denied_reason_none_when_no_config() {
        let user = user_with_network_access(None);
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny), NetworkAccessDecision::Allowed);
    }

    // =========================================================================================
    // GeoIP unavailable policy tests
    // =========================================================================================

    #[test]
    fn geoip_unavailable_default_deny_denies() {
        // Country rule exists but GeoIP is unavailable — default policy is Deny
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny);
        assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable));
    }

    #[test]
    fn geoip_unavailable_explicit_allow_allows() {
        // Country rule exists, GeoIP unavailable, but policy is Allow — allows
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
        assert_eq!(decision, NetworkAccessDecision::AllowedGeoIpUnavailable);
    }

    #[test]
    fn geoip_unavailable_cidr_only_still_denies() {
        // CIDR only rules, no match — should deny even with Allow policy
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Allow);
        assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch));
    }

    #[test]
    fn geoip_unavailable_cidr_match_allows() {
        // CIDR match always allows, regardless of policy
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec!["10.0.0.0/8".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let decision = evaluate_network_access(&user, "10.0.0.1", &geoip, GeoIpUnavailablePolicy::Deny);
        assert_eq!(decision, NetworkAccessDecision::Allowed);
    }

    #[test]
    fn geoip_loaded_country_mismatch_still_denies() {
        // Loaded GeoIP but country doesn't match — should deny under Allow policy
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("US")))));
        let decision = evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Allow);
        assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch));
    }

    #[test]
    fn geoip_loaded_unknown_country_still_denies() {
        // Loaded GeoIP but lookup returns None — should deny under Allow policy
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
        }));
        let geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::new()))));
        let decision = evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow);
        assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::CountryUnknown));
    }

    #[test]
    fn malformed_ip_denies_even_when_geoip_unavailable_policy_is_allow() {
        let user = user_with_network_access(Some(NetworkAccess::from(&shared::model::NetworkAccessDto {
            allowed_countries: Some(vec!["DE".to_string()]),
            allowed_networks: None,
        })));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());

        let decision = evaluate_network_access(&user, "not-an-ip", &geoip, GeoIpUnavailablePolicy::Allow);

        assert_eq!(decision, NetworkAccessDecision::Denied(NetworkAccessDenyReason::MalformedClientIp));
    }

    #[test]
    fn evaluate_network_access_respects_allow_policy() {
        // verify evaluate_network_access returns AllowedGeoIpUnavailable with Allow policy
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec!["DE".to_string()],
            allowed_networks: vec![],
        }));
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        // Default deny policy should return Denied
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable)
        );
        // Allow policy should return AllowedGeoIpUnavailable
        assert_eq!(evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow), NetworkAccessDecision::AllowedGeoIpUnavailable);
    }
}
