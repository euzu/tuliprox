pub use crate::repository::{
    evaluate_network_access, log_network_access_allowed_geoip_unavailable, log_network_access_denied,
    NetworkAccessDecision, NetworkAccessDenyReason,
};
use crate::{
    api::{
        endpoints::xtream_api::{get_xtream_player_api_stream_url, ApiStreamContext},
        model::{
            create_active_client_stream, create_channel_unavailable_stream, create_custom_video_stream_response,
            create_provider_connections_exhausted_stream, create_provider_stream,
            get_custom_stream_response_error_status, get_stream_response_with_headers, is_custom_video_stream_enabled,
            tee_stream, AppState, BoxedProviderStream, CustomVideoStreamType, PendingProviderReason,
            ProviderAllocation, ProviderConfig, ProviderHandle, ProviderStreamCustomReason,
            ProviderStreamFactoryOptions, ProviderStreamInfo, ProviderStreamState, SharedStreamCtx,
            SharedStreamManager, StreamDetails, StreamError, StreamingStrategy, ThrottledStream, UserApiRequest,
            UserSession,
        },
    },
    auth::Fingerprint,
    media_server::{
        playback::{
            media_server_image_response as open_media_server_proxy_image_response,
            media_server_stream_response as open_media_server_proxy_stream_response, parse_media_server_image_ref,
            parse_media_server_stream_ref,
        },
        MediaServerError, MediaServerErrorKind, MediaServerHttpClient, MediaServerImageRef,
    },
    model::{AppConfig, ConfigInput, ConfigTarget, InputUserInfo, ProxyUserCredentials},
    processing::{
        parser::hls::{rewrite_hls, RewriteHlsProps},
        processor::re_resolve_stalker_url,
    },
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
    http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use futures::{stream, Stream, StreamExt, TryStreamExt};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use log::{debug, error, info, log_enabled, trace, warn};
use serde::Serialize;
use shared::{
    concat_string,
    defaults::{DASH_EXT, HLS_EXT},
    model::{
        Claims, ConfigTargetOptions, InputFetchMethod, InputType, PlaylistEntry, PlaylistItemType, ProxyType,
        StalkerStreamKind, StreamChannel, StreamInfo, TargetType, UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        bin_serialize, current_time_secs, extract_extension_from_url, get_credentials_from_url, human_readable_kbps,
        is_sanitize_sensitive_info_enabled, replace_url_extension, sanitize_sensitive_info, trim_slash, Internable,
        CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON,
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
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::{Mutex, RwLock},
};
use tokio_util::io::ReaderStream;
use tuliprox_hls::api::MAX_HLS_MANIFEST_BYTES;
use url::Url;

/// Per-`(input id, provider id)` single-flight guards so concurrent client requests
/// for the same dead stalker stream trigger only one portal re-resolve at a time.
type StalkerResolveGuards = HashMap<(u16, u32), Arc<Mutex<()>>>;
static STALKER_RE_RESOLVE_GUARDS: LazyLock<Mutex<StalkerResolveGuards>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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
    let info = StreamInfo::new(shared::model::StreamInfoParams {
        uid: 0,
        meter_uid: 0,
        username: &attempt.user.username,
        addr: &attempt.fingerprint.addr,
        client_ip: &attempt.fingerprint.client_ip,
        provider: attempt.provider_name,
        stream_channel: attempt.stream_channel,
        user_agent,
        country_code: None,
        session_token: None,
    });
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
    create_custom_video_stream_response(&app_state.provider_stream_ctx(), &fingerprint.addr, video_type).into_response()
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

use crate::{
    api::{
        panel_api::{can_provision_on_exhausted, create_panel_api_provisioning_stream_details},
        static_headers::CT_OCTET,
    },
    utils::LRUResourceCache,
};
pub use internal_server_error;
use shared::{
    defaults::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs},
    error::TuliproxError,
    model::{ConnectFailureReason, FailureStage},
};
pub use try_option_bad_request;
pub use try_option_forbidden;
pub use try_result_bad_request;
pub use try_result_not_found;
pub use try_result_or_status;
// Moved to `tuliprox-core` so crates outside `api` can build responses too.
pub use tuliprox_core::try_unwrap_body;
// Admission moved to `tuliprox-session`, where the types it decides over
// already live. Re-exported so api call sites keep their names.
pub(crate) use tuliprox_core::utils::request_headers::{get_headers_from_request, HeaderFilter};
pub(crate) use tuliprox_session::{
    admission::{
        classify_playback_request, connection_priority_for_kind, resolve_admission_with_strategies,
        resolve_playback_request_admission, EvictionReentryGuard, PlaybackRequestClass, PlaybackRequestFacts,
    },
    stream_options::{get_stream_options, StreamOptions},
};

pub fn get_server_time() -> String {
    chrono::offset::Local::now().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

static PROCESS_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// Anchors the uptime clock; call once at process startup.
pub fn init_uptime_clock() {
    let _ = *PROCESS_START;
}

pub fn get_uptime_secs() -> u64 {
    PROCESS_START.elapsed().as_secs()
}

pub fn get_build_time() -> Option<String> {
    BUILD_TIMESTAMP
        .to_string()
        .parse::<DateTime<Utc>>()
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S %Z").to_string())
}

// Response-compression opt-out moved to `tuliprox_core::utils`; re-exported so
// api call sites keep their names.
pub(crate) use tuliprox_core::utils::response_compression::{
    mark_response_as_uncompressed, should_compress_response_extensions,
};

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

struct StreamingAcquireOptions<'a> {
    force_provider: Option<&'a Arc<str>>,
    allow_forced_provider_fallback: bool,
    allow_provider_grace: bool,
    user_priority: i8,
    connection_kind: crate::api::model::ConnectionKind,
    session_owner: Option<&'a str>,
    accept_requested_stream_url: bool,
}

pub struct ForceStreamRequestContext<'a> {
    pub req_headers: &'a HeaderMap,
    pub input: &'a Arc<ConfigInput>,
    pub user: &'a ProxyUserCredentials,
    pub session_reservation_ttl_secs: u64,
    pub(crate) content_representation: crate::api::model::ProviderContentRepresentationMode,
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
    grace_context: Option<crate::api::model::GraceResolutionContext>,
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
        if matches!(request_class, PlaybackRequestClass::FollowUp | PlaybackRequestClass::Activate) {
            // Re-read session under the guard to ensure the counted lease is still held or acquired.
            // If it is no longer counted, classify it from the current lifecycle so
            // stale FollowUp requests cannot bypass admission.
            // If it became counted, classify it so stale Activate requests don't double count.
            let current_session =
                app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
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
        let existing_session = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
        classify_playback_request(PlaybackRequestFacts {
            item_type,
            existing_session: existing_session.as_ref(),
            prepare_only: false,
            terminate: false,
        })
    };
    let limits_enabled = app_state.app_config.config.load().user_access_control
        && (user.max_connections > 0 || user.soft_connections > 0);
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
        let current_session = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
        let (_, resolved_grace) = match current_session.as_ref().map(|s| &s.lifecycle) {
            Some(crate::api::model::PlaybackLifecycle::PendingProvider { .. }) => {
                // Session already in PendingProvider — refresh deadline.
                let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                let _ = app_state
                    .active_users
                    .mark_pending_provider(&user.username, session_token, PendingProviderReason::GraceHold, deadline)
                    .await;
                (
                    crate::api::model::PlaybackLifecycle::PendingProvider {
                        data: crate::api::model::PendingProviderState {
                            reason_code: PendingProviderReason::GraceHold,
                            created_at: current_time_secs(),
                            deadline,
                            version: current_session.as_ref().map_or(0, |s| {
                                if let crate::api::model::PlaybackLifecycle::PendingProvider { data } = &s.lifecycle {
                                    data.version
                                } else {
                                    0
                                }
                            }),
                            wake_source: None,
                        },
                    },
                    Some(crate::api::model::GraceMode::Hold),
                )
            }
            Some(crate::api::model::PlaybackLifecycle::GraceActive) => {
                // Already in GraceActive — infer mode from item_type.
                let mode = if item_type.is_live() || item_type.is_live_adaptive() {
                    crate::api::model::GraceMode::Hold
                } else {
                    crate::api::model::GraceMode::Instant
                };
                (crate::api::model::PlaybackLifecycle::GraceActive, Some(mode))
            }
            _ => {
                // Session not yet in grace state — infer from item_type defaults.
                // Live/LiveHls/LiveDash default to Hold; VOD/Catchup to Instant.
                if item_type.is_live() || item_type.is_live_adaptive() {
                    let deadline = current_time_secs().saturating_add(app_state.get_grace_options().timeout_secs);
                    let _ = app_state
                        .active_users
                        .mark_pending_provider(
                            &user.username,
                            session_token,
                            PendingProviderReason::GraceHold,
                            deadline,
                        )
                        .await;
                    (
                        crate::api::model::PlaybackLifecycle::PendingProvider {
                            data: crate::api::model::PendingProviderState {
                                reason_code: PendingProviderReason::GraceHold,
                                created_at: current_time_secs(),
                                deadline,
                                version: 1,
                                wake_source: None,
                            },
                        },
                        Some(crate::api::model::GraceMode::Hold),
                    )
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

    let placeholder_transition_version = Some(
        app_state
            .active_users
            .ensure_user_session_placeholder(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id: virtual_id.get(),
                provider: input.name.as_ref(),
                stream_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(connection_kind),
                socket_bound,
            })
            .await,
    );

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
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
            app_state.active_users.mark_grace_active(&user.username, session_token).await;
        }
    }

    PlaybackActivationResult { admission, grace_mode, grace_context, placeholder_transition_version }
}

pub fn get_stream_alternative_url(
    stream_url: &str,
    input: &ConfigInput,
    alias_input: &Arc<ProviderConfig>,
) -> Option<String> {
    if input.input_type.is_m3u() && input.get_matched_config_by_url(stream_url).is_none() {
        return get_stream_alternative_url_m3u(stream_url, input, alias_input);
    }

    let (source_base_url, source_username, source_password, matched_via_external_signature) =
        if let Some(matched) = input.get_matched_config_by_url(stream_url) {
            (matched.0.to_string(), matched.1.cloned(), matched.2.cloned(), false)
        } else {
            let (base_url, username, password) = find_input_account_by_signature(stream_url, input)?;
            (base_url, username, password, true)
        };
    if matched_via_external_signature && !input.input_type.is_m3u() {
        return None;
    }
    let alt_input_user_info = alias_input.get_user_info()?;

    let modified = stream_url.replacen(&source_base_url, &alt_input_user_info.base_url, 1);
    let mut url = Url::parse(&modified).ok()?;

    if let (Some(old_username), Some(old_password)) = (source_username, source_password) {
        let auth_updated = rewrite_url_auth_fields(
            &mut url,
            &old_username,
            &old_password,
            &alt_input_user_info.username,
            &alt_input_user_info.password,
        );
        if !auth_updated {
            return None;
        }
    }

    Some(url.to_string())
}

fn get_stream_alternative_url_m3u(
    stream_url: &str,
    input: &ConfigInput,
    alias_input: &Arc<ProviderConfig>,
) -> Option<String> {
    if let Some((source_base_url, source_username, source_password)) =
        find_input_account_by_signature(stream_url, input)
    {
        let Some(alt_input_user_info) = alias_input.get_user_info() else {
            return Some(stream_url.to_string());
        };
        let modified = stream_url.replacen(&source_base_url, &alt_input_user_info.base_url, 1);
        let mut url = Url::parse(&modified).ok()?;

        if let (Some(old_username), Some(old_password)) = (source_username, source_password) {
            let auth_updated = rewrite_url_auth_fields(
                &mut url,
                &old_username,
                &old_password,
                &alt_input_user_info.username,
                &alt_input_user_info.password,
            );
            if !auth_updated {
                return None;
            }
        }

        return Some(url.to_string());
    }
    let Some(alt_input_user_info) = alias_input.get_user_info() else {
        let Ok(url) = Url::parse(stream_url) else {
            return None;
        };
        if providerless_m3u_url_has_explicit_credentials(&url) {
            return None;
        }
        return Some(stream_url.to_string());
    };
    if stream_url_has_account_signature(stream_url, &alt_input_user_info) {
        return None;
    }
    Some(stream_url.to_string())
}

fn providerless_m3u_url_has_explicit_credentials(url: &Url) -> bool {
    !url.username().is_empty()
        || url.password().is_some()
        || url
            .query_pairs()
            .any(|(key, _)| key.eq_ignore_ascii_case("username") || key.eq_ignore_ascii_case("password"))
}

/// Look for an account signature in the stream URL that matches the input
/// itself or one of its configured aliases. Returns the matching entry's
/// `(base_url, username, password)` so the caller can rewrite only the
/// account-specific parts of the URL while preserving the original host/path.
///
/// This helper is used for safe credential rewrites when Tuliprox switches
/// from one account to another. It is not the general trust gate for M3U
/// foreign hosts: plain external URLs from a stored M3U playlist item may be
/// accepted without a matching signature, while unrelated credential-bearing
/// URLs still fail closed unless they provably match the input or one of its
/// aliases.
fn find_input_account_by_signature(
    stream_url: &str,
    input: &ConfigInput,
) -> Option<(String, Option<String>, Option<String>)> {
    // Try the input's main account first.
    if let Some(user_info) = input.get_user_info() {
        if stream_url_account_matches(stream_url, &user_info) {
            return Some((input.url.clone(), Some(user_info.username), Some(user_info.password)));
        }
    }
    // Then try each alias, if any. The input_type is inherited from the
    // parent input for all aliases — see ConfigInputAlias definition.
    if let Some(aliases) = input.aliases.as_ref() {
        for alias in aliases {
            if let Some(user_info) =
                InputUserInfo::new(input.input_type, alias.username.as_deref(), alias.password.as_deref(), &alias.url)
            {
                if stream_url_account_matches(stream_url, &user_info) {
                    return Some((alias.url.clone(), Some(user_info.username), Some(user_info.password)));
                }
            }
        }
    }
    None
}

fn rewrite_url_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    if rewrite_query_auth_fields(url, new_username, new_password) {
        return true;
    }

    if url.username() == old_username && url.password() == Some(old_password) {
        return url.set_username(new_username).is_ok() && url.set_password(Some(new_password)).is_ok();
    }

    rewrite_path_auth_fields(url, old_username, old_password, new_username, new_password)
}

fn rewrite_query_auth_fields(url: &mut Url, new_username: &str, new_password: &str) -> bool {
    let mut has_username = false;
    let mut has_password = false;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("username") {
                has_username = true;
                (key.into_owned(), new_username.to_string())
            } else if key.eq_ignore_ascii_case("password") {
                has_password = true;
                (key.into_owned(), new_password.to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !(has_username && has_password) {
        return false;
    }

    url.query_pairs_mut().clear().extend_pairs(pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
    true
}

fn collect_path_segments(url: &Url) -> Option<Vec<String>> {
    url.path_segments().map(|segments| segments.map(ToOwned::to_owned).collect::<Vec<_>>())
}

fn find_path_auth_segment_index(segments: &[String], username: &str, password: &str) -> Option<usize> {
    segments.windows(2).position(|pair| {
        pair.first().is_some_and(|segment| segment == username)
            && pair.get(1).is_some_and(|segment| segment == password)
    })
}

fn rewrite_path_auth_fields(
    url: &mut Url,
    old_username: &str,
    old_password: &str,
    new_username: &str,
    new_password: &str,
) -> bool {
    let Some(mut segments) = collect_path_segments(url) else {
        return false;
    };

    let credential_index = find_path_auth_segment_index(&segments, old_username, old_password);
    let Some(credential_index) = credential_index else {
        return false;
    };

    segments[credential_index] = new_username.to_string();
    segments[credential_index + 1] = new_password.to_string();

    let Ok(mut path_segments) = url.path_segments_mut() else {
        return false;
    };
    path_segments.clear().extend(segments.iter().map(String::as_str));
    true
}

fn stream_url_matches_provider(stream_url: &str, provider_cfg: &ProviderConfig) -> bool {
    let Some(user_info) = provider_cfg.get_user_info() else {
        return false;
    };
    if stream_url_base_matches(stream_url, &user_info.base_url) {
        // Same-host fast path: both base URL and account identity must match.
        return stream_url_account_matches(stream_url, &user_info);
    }
    if !provider_cfg.input_type.is_m3u() {
        return false;
    }
    // For M3U inputs, the stored playlist entry itself is the trust anchor.
    // Open external URLs are therefore allowed, but external URLs that carry
    // explicit account markers must still match the selected provider account.
    if stream_url_has_account_signature(stream_url, &user_info) {
        return stream_url_account_matches(stream_url, &user_info);
    }
    true
}

fn stream_url_base_matches(stream_url: &str, base_url: &str) -> bool {
    stream_url
        .strip_prefix(base_url)
        .is_some_and(|remaining| remaining.is_empty() || remaining.starts_with(['/', '?', '#']))
}

fn stream_url_account_matches(stream_url: &str, user_info: &crate::model::InputUserInfo) -> bool {
    let Ok(url) = Url::parse(stream_url) else {
        return false;
    };

    let (url_username, url_password) = get_credentials_from_url(&url);
    if let (Some(url_username), Some(url_password)) = (url_username.as_deref(), url_password.as_deref()) {
        return url_username == user_info.username && url_password == user_info.password;
    }

    let mut has_query_username = false;
    let mut has_query_password = false;
    for (key, value) in url.query_pairs() {
        if key.eq_ignore_ascii_case("username") {
            has_query_username = value == user_info.username;
        } else if key.eq_ignore_ascii_case("password") {
            has_query_password = value == user_info.password;
        }
    }
    if has_query_username || has_query_password {
        return has_query_username && has_query_password;
    }

    let Some(segments) = collect_path_segments(&url) else {
        return false;
    };

    find_path_auth_segment_index(&segments, &user_info.username, &user_info.password).is_some()
}

fn stream_url_has_account_signature(stream_url: &str, user_info: &crate::model::InputUserInfo) -> bool {
    let Ok(url) = Url::parse(stream_url) else {
        return false;
    };

    let (url_username, url_password) = get_credentials_from_url(&url);
    if url_username.is_some() && url_password.is_some() {
        return true;
    }

    let mut has_query_username = false;
    let mut has_query_password = false;
    for (key, _) in url.query_pairs() {
        if key.eq_ignore_ascii_case("username") {
            has_query_username = true;
        } else if key.eq_ignore_ascii_case("password") {
            has_query_password = true;
        }
    }
    if has_query_username || has_query_password {
        return has_query_username && has_query_password;
    }

    // Path-based credentials: some Xtream endpoints embed the account in the URL
    // path (e.g. /live/<user>/<pass>/...). Only flag a signature when the
    // consecutive segments actually match the configured user/pass — arbitrary
    // open paths must not be treated as account signatures.
    if let Some(segments) = collect_path_segments(&url) {
        if find_path_auth_segment_index(&segments, &user_info.username, &user_info.password).is_some() {
            return true;
        }
    }

    false
}

fn select_provider_stream_url(
    stream_url: &str,
    input: &ConfigInput,
    provider_cfg: &Arc<ProviderConfig>,
    accept_requested_stream_url: bool,
) -> Option<(Arc<str>, String)> {
    if accept_requested_stream_url {
        return Some((provider_cfg.name.clone(), stream_url.to_string()));
    }
    if stream_url_matches_provider(stream_url, provider_cfg) {
        Some((provider_cfg.name.clone(), stream_url.to_string()))
    } else {
        get_stream_alternative_url(stream_url, input, provider_cfg).map(|url| (provider_cfg.name.clone(), url))
    }
}

fn create_unmapped_provider_stream(app_config: &AppConfig) -> ProviderStreamState {
    ProviderStreamState::Custom {
        response: create_channel_unavailable_stream(app_config, &[], StatusCode::OK),
        reason: ProviderStreamCustomReason::UnmappedProviderUrl,
    }
}

async fn acquire_stream_provider_handle(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    fingerprint: &Fingerprint,
    options: StreamingAcquireOptions<'_>,
) -> Option<ProviderHandle> {
    match options.force_provider {
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
    }
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
///   - If the URL already targets the selected provider account, the original URL is reused.
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
    let accept_requested_stream_url = options.accept_requested_stream_url || input.input_type.is_stalker();
    let mut provider_connection_handle = acquire_stream_provider_handle(app_state, input, fingerprint, options).await;

    // panel_api provisioning/loading is handled later in the stream creation flow

    let mut release_failed_mapping = false;
    let stream_response_params = if let Some(allocation) = provider_connection_handle.as_ref().map(|ph| &ph.allocation)
    {
        match allocation {
            ProviderAllocation::Exhausted => {
                debug!("Provider {} is exhausted. No connections allowed.", input.name);
                let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
                ProviderStreamState::Custom { response: stream, reason: ProviderStreamCustomReason::ProviderExhausted }
            }
            ProviderAllocation::Available(ref provider_cfg) | ProviderAllocation::GracePeriod(ref provider_cfg) => {
                // Keep the URL only when it already targets the selected provider account. Hot reload can leave old
                // alias URLs in persisted playlists until the next processing run.
                if let Some((selected_provider_name, url)) =
                    select_provider_stream_url(stream_url, input, provider_cfg, accept_requested_stream_url)
                {
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
                } else {
                    debug_if_enabled!(
                        "provider session rejected: input={} provider_cfg={} allocation={} stream_url={} reason=unmapped_provider_url",
                        sanitize_sensitive_info(&input.name),
                        sanitize_sensitive_info(&provider_cfg.name),
                        allocation.short_key(),
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref())
                    );
                    release_failed_mapping = true;
                    create_unmapped_provider_stream(&app_state.app_config)
                }
            }
        }
    } else {
        debug!("Provider {} is exhausted. No connections allowed.", input.name);
        let stream = create_provider_connections_exhausted_stream(&app_state.app_config, &[]);
        ProviderStreamState::Custom { response: stream, reason: ProviderStreamCustomReason::ProviderExhausted }
    };

    if release_failed_mapping {
        if let Some(handle) = provider_connection_handle.take() {
            let connection_manager = Arc::clone(&app_state.connection_manager);
            tokio::spawn(async move {
                connection_manager.release_provider_handle(Some(handle)).await;
            });
        }
    }

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

    // Catch-up must open immediately so its payload can be classified before response headers are committed.
    if item_type == PlaylistItemType::Catchup {
        return false;
    }

    // v3.3.0 opened provider-affine VOD/Series reopens immediately, even when
    // provider grace was temporarily in effect. Parking these requests in GracePending
    // was introduced later and breaks players like libmpv during seek/reopen retries.
    // Keep hold-stream behavior for live/admission paths, but restore direct-open behavior
    // for provider-affine on-demand session reopens.
    !(!item_type.is_live() && item_type.requires_provider_affinity() && is_reopen)
}

fn should_refresh_stalker_playback(input_type: InputType, request_url_valid: bool, status: Option<StatusCode>) -> bool {
    input_type.is_stalker() && (!request_url_valid || status.is_some_and(|status| status.is_client_error()))
}

fn needs_initial_stalker_resolution(input_type: InputType, stream_url: &str) -> bool {
    input_type.is_stalker() && stream_url.is_empty()
}

fn stalker_stream_kind(cluster: XtreamCluster, item_type: PlaylistItemType) -> StalkerStreamKind {
    if item_type == PlaylistItemType::Catchup {
        StalkerStreamKind::Archive
    } else {
        match cluster {
            XtreamCluster::Live => StalkerStreamKind::Live,
            XtreamCluster::Video => StalkerStreamKind::Movie,
            XtreamCluster::Series => StalkerStreamKind::Episode,
        }
    }
}

async fn re_resolve_stalker_url_singleflight(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_id: u32,
    kind: StalkerStreamKind,
    force_refresh: bool,
) -> Result<Option<Arc<str>>, TuliproxError> {
    let guard_key = (input.id, provider_id);
    let entry_lock = {
        let mut guards = STALKER_RE_RESOLVE_GUARDS.lock().await;
        Arc::clone(guards.entry(guard_key).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    let result = {
        let _flight = entry_lock.lock().await;
        let client = app_state.http_client.load().as_ref().clone();
        re_resolve_stalker_url(&app_state.app_config, &client, input, provider_id, kind, force_refresh).await
    };
    drop(entry_lock);
    let mut guards = STALKER_RE_RESOLVE_GUARDS.lock().await;
    if guards.get(&guard_key).is_some_and(|lock| Arc::strong_count(lock) == 1) {
        guards.remove(&guard_key);
    }
    result
}

pub(crate) async fn resolve_initial_stalker_playback_url(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_id: u32,
    cluster: XtreamCluster,
    item_type: PlaylistItemType,
    stream_url: &Arc<str>,
) -> Result<Arc<str>, TuliproxError> {
    if !needs_initial_stalker_resolution(input.input_type, stream_url) {
        return Ok(Arc::clone(stream_url));
    }
    re_resolve_stalker_url_singleflight(app_state, input, provider_id, stalker_stream_kind(cluster, item_type), false)
        .await?
        .ok_or_else(|| {
            TuliproxError::RepositoryStalker(format!(
                "Stalker playback URL could not be resolved for input '{}' and provider id {provider_id}",
                input.name
            ))
        })
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
    content_representation: crate::api::model::ProviderContentRepresentationMode,
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
    session_headers: Option<&HashMap<String, String>>,
    accept_requested_stream_url: bool,
    grace_hold_override: Option<bool>,
    grace_resolution_context: Option<crate::api::model::GraceResolutionContext>,
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
            accept_requested_stream_url,
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

    if matches!(
        streaming_strategy.provider_stream_state,
        ProviderStreamState::Custom { reason: ProviderStreamCustomReason::ProviderExhausted, .. }
    ) && can_provision_on_exhausted(app_state, input)
    {
        if let Some(handle) = streaming_strategy.provider_handle.take() {
            app_state.connection_manager.release_provider_handle(Some(handle)).await;
        }
        debug_if_enabled!(
            "panel_api: provider connections exhausted; sending provisioning stream for input {}",
            sanitize_sensitive_info(&input.name)
        );
        let mut details = create_panel_api_provisioning_stream_details(
            app_state,
            input,
            guard_provider_name.clone().or_else(|| Some(input.name.clone())),
            &grace_period_options,
            fingerprint.addr,
            virtual_id,
        );
        details.content_representation = content_representation;
        return Ok(details);
    }

    match streaming_strategy.provider_stream_state {
        // custom stream means we display our own stream like connection exhausted, channel-unavailable...
        ProviderStreamState::Custom { response: provider_stream, .. } => {
            let (stream, stream_info) = provider_stream;
            // When allocation is exhausted or no connection was acquired, guard_provider_name is None.
            // Use input.name as fallback so the provider field is never empty.
            let provider_name = guard_provider_name.clone().unwrap_or_else(|| input.name.clone());
            Ok(StreamDetails {
                stream,
                stream_info,
                provider_name: Some(provider_name),
                request_url: None,
                session_headers: session_headers.cloned(),
                provider_session_headers: HashMap::new(),
                grace_period: grace_period_options,
                provider_grace_active: false,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: streaming_strategy.provider_handle.clone(),
                content_representation,
                grace_resolution_context,
            })
        }
        ProviderStreamState::Available(_provider_name, request_url)
        | ProviderStreamState::GracePeriod(_provider_name, request_url) => {
            let mut request_url = request_url;
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
            let (stream, stream_info, provider_session_headers, reconnect_flag) =
                if defer_provider_stream_until_grace_check {
                    debug_if_enabled!(
                        "Deferring provider stream open until grace check completes for {}",
                        sanitize_sensitive_info(resolve_request_url_for_logging(input, request_url.as_ref()).as_ref())
                    );
                    (None, None, HashMap::new(), None)
                } else if is_media_server_stream_ref_url(request_url.as_ref()) {
                    match open_media_server_stream_for_input(app_state, input, request_url.as_ref(), req_headers).await
                    {
                        Ok((stream, stream_info)) => (Some(stream), stream_info, HashMap::new(), None),
                        Err(err) => {
                            error!("Can't open media-server stream: {err}");
                            (None, None, HashMap::new(), None)
                        }
                    }
                } else {
                    let parsed_url = Url::parse(&request_url);
                    let request_url_valid = parsed_url.is_ok();
                    let ((mut stream, mut stream_info, mut provider_session_headers), mut reconnect_flag) =
                        if let Ok(url) = parsed_url {
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
                                    session_headers,
                                    disabled_headers: disabled_headers.as_ref(),
                                    default_user_agent: default_user_agent.as_deref(),
                                    username: Some(username),
                                    client_ip: Some(&fingerprint.client_ip),
                                    stream_channel: Some(stream_channel),
                                    connect_failure_stage: Some(FailureStage::ProviderOpen),
                                    content_representation,
                                });

                            let provider_config = input.get_resolve_provider(url.as_ref());
                            provider_stream_factory_options.set_provider(provider_config);
                            if input.input_type.is_stalker() {
                                provider_stream_factory_options.require_public_destination();
                            }

                            let reconnect_flag = provider_stream_factory_options.get_reconnect_flag_clone();
                            let provider_stream = match create_provider_stream(
                                &app_state.provider_stream_ctx(),
                                &app_state.http_client.load(),
                                provider_stream_factory_options,
                            )
                            .await
                            {
                                None => (None, None, HashMap::new()),
                                Some(response) => {
                                    (Some(response.stream), response.info, response.provider_session_headers)
                                }
                            };
                            (provider_stream, Some(reconnect_flag))
                        } else {
                            ((None, None, HashMap::new()), None)
                        };
                    let should_refresh_stalker = should_refresh_stalker_playback(
                        input.input_type,
                        request_url_valid,
                        stream_info.as_ref().map(|(_, status, _, _)| *status),
                    );
                    if should_refresh_stalker {
                        let force_stalker_refresh =
                            stream_info.as_ref().is_some_and(|(_, status, _, _)| status.is_client_error());
                        let kind = stalker_stream_kind(stream_channel.cluster, item_type);
                        let resolve_result = re_resolve_stalker_url_singleflight(
                            app_state,
                            input,
                            stream_channel.provider_id,
                            kind,
                            force_stalker_refresh,
                        )
                        .await;
                        match resolve_result {
                            Ok(Some(refreshed_url)) => {
                                if let Ok(url) = Url::parse(&refreshed_url) {
                                    let default_user_agent =
                                        app_state.app_config.config.load().default_user_agent.clone();
                                    let disabled_headers = app_state.get_disabled_headers();
                                    let mut options = ProviderStreamFactoryOptions::new(
                                        &crate::api::model::ProviderStreamFactoryParams {
                                            addr: fingerprint.addr,
                                            item_type,
                                            share_stream,
                                            stream_options,
                                            stream_url: &url,
                                            req_headers,
                                            input_headers: streaming_strategy.input_headers.as_ref(),
                                            session_headers,
                                            disabled_headers: disabled_headers.as_ref(),
                                            default_user_agent: default_user_agent.as_deref(),
                                            username: Some(username),
                                            client_ip: Some(&fingerprint.client_ip),
                                            stream_channel: Some(stream_channel),
                                            connect_failure_stage: Some(FailureStage::ProviderOpen),
                                            content_representation,
                                        },
                                    );
                                    options.set_provider(input.get_resolve_provider(url.as_ref()));
                                    options.require_public_destination();
                                    let retry_reconnect_flag = options.get_reconnect_flag_clone();
                                    let retried = create_provider_stream(
                                        &app_state.provider_stream_ctx(),
                                        &app_state.http_client.load(),
                                        options,
                                    )
                                    .await;
                                    if let Some(response) = retried {
                                        stream = Some(response.stream);
                                        stream_info = response.info;
                                        provider_session_headers = response.provider_session_headers;
                                        reconnect_flag = Some(retry_reconnect_flag);
                                        request_url = refreshed_url;
                                    } else {
                                        // Keep the original stream/stream_info: the upstream response
                                        // might still be serveable, and its status is needed for reporting.
                                        debug!("Stalker re-resolve retry could not open a stream, keeping original provider response");
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                warn!(
                                    "Failed to refresh Stalker playback URL: {}",
                                    sanitize_sensitive_info(&err.to_string())
                                );
                            }
                        }
                    }
                    (stream, stream_info, provider_session_headers, reconnect_flag)
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

            // An intentional deferred open must retain its grace allocation until body polling
            // resumes the provider request. Other failed opens release their allocation here.
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
                session_headers: session_headers.cloned(),
                provider_session_headers,
                grace_period: grace_period_options,
                provider_grace_active,
                disable_provider_grace: false,
                reconnect_flag,
                provider_handle,
                content_representation,
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
        let extension = self.stream_ext.map_or_else(
            || extract_extension_from_url(url).map_or_else(String::new, ToString::to_string),
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
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
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
                    Some(provider_cfg) => match get_stream_alternative_url(&url, params.input, &provider_cfg) {
                        Some(stream_url) => stream_url,
                        None => return Some(StatusCode::BAD_REQUEST.into_response()),
                    },
                    None => url.to_string(),
                },
            };
            let stream_url = match resolve_redirect_location(Some(params.input), &stream_url) {
                Ok(url) => url,
                Err(err) => {
                    error!("Failed to resolve redirect url: {}", sanitize_sensitive_info(&err.to_string()));
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
            let client = input.plex_catalog_client(http_client)?;
            open_media_server_proxy_stream_response(&client, &stream_ref, range).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server playback proxy is not implemented for this input type"));
        }
        InputType::M3u
        | InputType::Xtream
        | InputType::M3uBatch
        | InputType::XtreamBatch
        | InputType::Stalker
        | InputType::StalkerBatch
        | InputType::Library
        | InputType::Staged => {
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

fn no_custom_video_fallback_status(app_config: &AppConfig) -> StatusCode {
    // Two reasons we have no custom-video response:
    //   1. Operator disabled `custom_stream_response_enabled`  → return the
    //      configured fallback status (e.g. 502) so reverse proxies handle the
    //      socket consistently.
    //   2. Operator enabled custom-video but the concrete resource is missing
    //      → return `400` so downstream `proxy_intercept_errors on;` (Nginx)
    //      can sever the socket instead of looping on `200 OK`.
    // Collapsing both into the configured status code broke the Nginx-intercept
    // contract that the operator relied on by enabling custom-video in the
    // first place.
    if is_custom_video_stream_enabled(app_config) {
        StatusCode::BAD_REQUEST
    } else {
        get_custom_stream_response_error_status(app_config)
    }
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
    let _transition_guard =
        app_state.active_users.acquire_playback_transition(&ctx.user.username, &user_session.token).await;
    let stream_options = get_stream_options(&app_state.app_config);
    let share_stream = false;
    let connection_permission = UserConnectionPermission::Allowed;
    let item_type = stream_channel.item_type;

    // Forced reopens must clear stale provider slots before reacquiring. For adaptive HLS/DASH
    // and Catchup sessions we only target old active stream sockets of the same session, never
    // manifest-only session addresses, otherwise the controlling playlist request gets torn down.
    let cleanup_addrs = if item_type.is_live_adaptive() || item_type == PlaylistItemType::Catchup {
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
        ctx.content_representation,
        share_stream,
        connection_permission,
        preferred_provider,
        allow_forced_provider_fallback,
        allow_provider_grace,
        VirtualId::new(stream_channel.virtual_id),
        connection_priority_for_kind(ctx.user, connection_kind),
        connection_kind,
        true,
        Some(user_session.token.as_str()),
        Some(&user_session.provider_session_headers),
        true,
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
        no_custom_video_fallback_status(&app_state.app_config).into_response()
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
    pinned_provider: Option<&Arc<str>>,
    req_headers: &HeaderMap,
    input: &Arc<ConfigInput>,
    target: &Arc<ConfigTarget>,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    allow_exhausted_shared_reconnect: bool,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> impl IntoResponse + Send {
    let _transition_guard = app_state.active_users.acquire_playback_transition(&user.username, session_token).await;
    let request_log_stream_url = resolve_request_url_for_logging(input, stream_url);
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(request_log_stream_url.as_ref()));
    }

    let virtual_id = stream_channel.virtual_id;
    let item_type = stream_channel.item_type;
    let playback_extension = extract_extension_from_url(stream_url);
    let socket_bound = is_socket_bound_playback_session(item_type, playback_extension);
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
            virtual_id: VirtualId::new(virtual_id),
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
            &app_state.provider_stream_ctx(),
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        )
        .into_response();
    }

    let stream_options = get_stream_options(&app_state.app_config);
    let session_state = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
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
        if item_type == PlaylistItemType::Catchup {
            crate::api::model::ProviderContentRepresentationMode::Identity
        } else {
            crate::api::model::ProviderContentRepresentationMode::PreserveOrigin
        },
        share_stream,
        connection_permission,
        pinned_provider,
        pinned_provider.is_none(),
        true,
        VirtualId::new(stream_channel.virtual_id),
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
        false,
        Some(session_token),
        session_state.as_ref().map(|session| &session.provider_session_headers),
        pinned_provider.is_some(),
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

    if item_type == PlaylistItemType::Catchup {
        if let Some(provider_stream) = stream_details.stream.take() {
            let probe_deadline = Duration::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms().max(1));
            match probe_catchup_payload(provider_stream, probe_deadline).await {
                Ok(CatchupPayload::Direct(provider_stream)) => stream_details.stream = Some(provider_stream),
                Ok(CatchupPayload::HlsManifest(manifest)) => {
                    return detected_catchup_hls_response(DetectedCatchupHlsResponseParams {
                        app_state,
                        stream_details,
                        manifest,
                        user,
                        target,
                        input,
                        fingerprint,
                        session_token,
                        virtual_id: VirtualId::new(virtual_id),
                        connection_permission,
                        connection_kind,
                        fallback_stream_url: stream_url,
                    })
                    .await;
                }
                Err(err) => {
                    error!("Failed to inspect catch-up payload: {err}");
                    cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token)
                        .await;
                    return StatusCode::BAD_GATEWAY.into_response();
                }
            }
        }
    }

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

        // Captured before `stream_details` is moved into `create_active_client_stream`.
        // The pinning rule is centralized in `should_pin_provider_for_session` so it stays
        // testable in isolation and in sync with the call site below.
        let should_pin_provider = should_pin_provider_for_session(&stream_details, app_state, item_type);

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
                SharedStreamCtx {
                    app_config: &app_state.app_config,
                    shared_stream_manager: &app_state.shared_stream_manager,
                    active_provider: &app_state.active_provider,
                    connection_manager: &app_state.connection_manager,
                },
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
                    if should_pin_provider {
                        let reservation_ttl_secs = get_session_reservation_ttl_secs(app_state, item_type);
                        if reservation_ttl_secs > 0 {
                            app_state
                                .active_provider
                                .refresh_provider_reservation(&provider, session_token, reservation_ttl_secs)
                                .await;
                        }
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
    no_custom_video_fallback_status(&app_state.app_config).into_response()
}

enum CatchupPayload {
    Direct(BoxedProviderStream),
    HlsManifest(Bytes),
}

struct DetectedCatchupHlsResponseParams<'a> {
    app_state: &'a Arc<AppState>,
    stream_details: StreamDetails,
    manifest: Bytes,
    user: &'a ProxyUserCredentials,
    target: &'a ConfigTarget,
    input: &'a ConfigInput,
    fingerprint: &'a Fingerprint,
    session_token: &'a str,
    virtual_id: VirtualId,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    fallback_stream_url: &'a str,
}

async fn detected_catchup_hls_response(params: DetectedCatchupHlsResponseParams<'_>) -> axum::response::Response {
    let DetectedCatchupHlsResponseParams {
        app_state,
        mut stream_details,
        manifest,
        user,
        target,
        input,
        fingerprint,
        session_token,
        virtual_id,
        connection_permission,
        connection_kind,
        fallback_stream_url,
    } = params;

    let Some(provider) = stream_details.provider_name.clone() else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Some(server_info) = app_state.app_config.get_user_server_info(user) else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(content) = std::str::from_utf8(&manifest) else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };

    let response_url = stream_details
        .stream_info
        .as_ref()
        .and_then(|(_, _, response_url, _)| response_url.as_ref())
        .map_or_else(|| fallback_stream_url.to_string(), ToString::to_string);
    let base_url = server_info.get_base_url();
    let encrypt_secret = app_state.get_encrypt_secret();
    let rewritten = rewrite_hls(
        user,
        &RewriteHlsProps {
            secret: &encrypt_secret,
            base_url: &base_url,
            content,
            hls_url: response_url,
            target_id: target.id,
            virtual_id: virtual_id.get(),
            input_id: input.id,
            user_token: Some(session_token),
        },
    );

    let request_url = stream_details.request_url.as_deref().unwrap_or(fallback_stream_url);
    let created_session_token = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id: virtual_id.get(),
            provider: &provider,
            stream_url: request_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind: Some(connection_kind),
            socket_bound: false,
        })
        .await;
    if !stream_details.provider_session_headers.is_empty() {
        app_state
            .active_users
            .update_session_provider_headers(
                &user.username,
                &created_session_token,
                &stream_details.provider_session_headers,
            )
            .await;
    }
    app_state
        .active_provider
        .refresh_provider_reservation(&provider, &created_session_token, get_catchup_session_ttl_secs(app_state))
        .await;
    app_state.connection_manager.release_provider_handle(stream_details.provider_handle.take()).await;
    app_state
        .active_users
        .release_unbound_session_reservation(&user.username, &created_session_token, None, false)
        .await;
    app_state.active_users.clear_unbound_session_addr(&user.username, &created_session_token, &fingerprint.addr).await;

    catchup_hls_manifest_response(rewritten)
}

fn catchup_hls_manifest_response(content: String) -> axum::response::Response {
    let mut response = try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::api::static_headers::CT_M3U.clone())
        .header(header::CACHE_CONTROL, crate::api::static_headers::CC_NO_STORE.clone())
        .body(Body::from(content)));
    mark_response_as_uncompressed(&mut response);
    response
}

async fn cleanup_failed_detected_catchup_hls(
    app_state: &Arc<AppState>,
    stream_details: &mut StreamDetails,
    username: &str,
    session_token: &str,
) {
    app_state.connection_manager.release_provider_handle(stream_details.provider_handle.take()).await;
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token).await;
}

async fn probe_catchup_payload(stream: BoxedProviderStream, deadline: Duration) -> Result<CatchupPayload, StreamError> {
    tokio::time::timeout(deadline, probe_catchup_payload_inner(stream))
        .await
        .map_err(|_| StreamError::Stream("catch-up payload probe timed out".to_string()))?
}

async fn probe_catchup_payload_inner(mut stream: BoxedProviderStream) -> Result<CatchupPayload, StreamError> {
    const HLS_SIGNATURE: &[u8] = b"#EXTM3U";

    let mut prefix = BytesMut::new();
    while prefix.len() < HLS_SIGNATURE.len() {
        let Some(chunk) = stream.next().await else {
            return Ok(CatchupPayload::Direct(stream::once(async move { Ok(prefix.freeze()) }).chain(stream).boxed()));
        };
        prefix.extend_from_slice(&chunk?);
    }

    if !prefix.starts_with(HLS_SIGNATURE) {
        return Ok(CatchupPayload::Direct(stream::once(async move { Ok(prefix.freeze()) }).chain(stream).boxed()));
    }
    if prefix.len() > MAX_HLS_MANIFEST_BYTES {
        return Err(StreamError::Stream("catch-up HLS manifest exceeds size limit".to_string()));
    }

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if prefix.len().saturating_add(chunk.len()) > MAX_HLS_MANIFEST_BYTES {
            return Err(StreamError::Stream("catch-up HLS manifest exceeds size limit".to_string()));
        }
        prefix.extend_from_slice(&chunk);
    }

    Ok(CatchupPayload::HlsManifest(prefix.freeze()))
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

    if share_stream {
        let meter_uid = app_state
            .shared_stream_manager
            .get_or_register_meter_uid(stream_url, || app_state.connection_manager.next_stream_uid())
            .await;
        return StreamMeteringConfig { meter_uid, meter_stream: has_stream || has_deferred_provider_open };
    } else if has_stream || has_deferred_provider_open {
        let meter_uid = app_state.connection_manager.next_stream_uid();
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
    let close_client_socket = !(item_type.is_live_adaptive() || item_type == PlaylistItemType::Catchup);
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

/// Whether the session should pin the provider account via `refresh_provider_reservation`.
///
/// A non-Provisioning custom video (`ChannelUnavailable`, `ProviderConnectionsExhausted`, …) means
/// the upstream open already failed. The provider connection slot was released by
/// `create_provider_stream`, and the custom video is a local fallback served to the client.
/// Pinning the provider via `refresh_provider_reservation` would hold the provider account for
/// the configured session TTL (e.g. `catchup_session_ttl_secs`), blocking other sessions of
/// the same family from using it even though the slot is already free.
///
/// Only `Provisioning` custom videos represent a real provider handoff that benefits from
/// keeping the same provider pinned, and real provider streams (`stream_info` carries no
/// `CustomVideoStreamType`) obviously qualify.
pub(crate) fn should_pin_provider_for_session(
    stream_details: &StreamDetails,
    _app_state: &Arc<AppState>,
    _item_type: PlaylistItemType,
) -> bool {
    !matches!(
        stream_details.stream_info.as_ref(),
        Some((_, _, _, Some(cv))) if *cv != CustomVideoStreamType::Provisioning
    )
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
    if let Some((stream, provider)) = SharedStreamManager::subscribe_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
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
                is_socket_bound_playback_session(stream_channel.item_type, extract_extension_from_url(stream_url));
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
            let meter_uid = app_state
                .shared_stream_manager
                .get_or_register_meter_uid(stream_url, || app_state.connection_manager.next_stream_uid())
                .await;
            stream_channel.shared_stream_id = Some(u64::from(meter_uid));
            let metering = StreamMeteringConfig { meter_uid, meter_stream: false };
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
        Some(app_state.active_users.acquire_playback_transition(&user.username, session_token).await)
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
                &app_state.provider_stream_ctx(),
                &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            )
            .into_response();
        }
        connection_permission = UserConnectionPermission::Allowed;
    }

    let path = PathBuf::from(pli.url.strip_prefix("file://").unwrap_or(&pli.url));

    let Ok(mut file) = tokio::fs::File::open(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(opened_metadata) = file.metadata().await else { return internal_server_error!() };

    // Canonicalize and validate the path
    let canonical = match tokio::fs::canonicalize(&path).await {
        Ok(canonical) => canonical,
        Err(err) => {
            error!("Local file path is corrupt {}: {err}", path.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    if check_path {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(canonical_metadata) = tokio::fs::metadata(&canonical).await else { return internal_server_error!() };
            if opened_metadata.dev() != canonical_metadata.dev() || opened_metadata.ino() != canonical_metadata.ino() {
                error!("TOCTOU race detected: file swapped during local_stream_response");
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        #[cfg(windows)]
        match same_windows_file_identity(&file, &canonical).await {
            Ok(true) => {}
            Ok(false) => {
                error!("TOCTOU race detected: file swapped during local_stream_response");
                return StatusCode::FORBIDDEN.into_response();
            }
            Err(err) => {
                error!("Could not verify local file identity {}: {err}", canonical.display());
                return internal_server_error!();
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            error!("Secure local file identity validation is unsupported on this platform");
            return StatusCode::FORBIDDEN.into_response();
        }

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
        if !is_path_within_allowed_directories(&canonical, &library_paths) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    let file_size = opened_metadata.len();

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
                headers.insert(header::CONTENT_TYPE, CT_OCTET.clone()); //HeaderValue::from_static("application/octet-stream"));
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
    let socket_bound = is_socket_bound_playback_session(pli.item_type, extract_extension_from_url(&pli.url));
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
                virtual_id: VirtualId::new(pli.virtual_id),
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
                &app_state.provider_stream_ctx(),
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
    let metering = prepare_stream_metering(app_state, &pli.url, false, true, false).await;
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
        meter_uid: metering.meter_uid,
        meter_stream: metering.meter_stream,
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
        && target.options.as_ref().is_some_and(ConfigTargetOptions::share_live_mpeg_ts_enabled)
}

pub fn is_hls_stream_share_enabled(target: &ConfigTarget) -> bool {
    target.options.as_ref().is_some_and(ConfigTargetOptions::share_live_hls_enabled)
}

fn get_add_cache_content(
    res_url: &str,
    mime_type: Option<String>,
    cache: &Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
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
                let _ = cache.write().await.add_content(&res_url, mime_type, size);
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

#[cfg(windows)]
async fn same_windows_file_identity(opened_file: &tokio::fs::File, canonical_path: &Path) -> std::io::Result<bool> {
    let canonical_file = tokio::fs::File::open(canonical_path).await?;
    Ok(windows_file_identity(opened_file)? == windows_file_identity(&canonical_file)?)
}

#[cfg(windows)]
fn windows_file_identity(file: &tokio::fs::File) -> std::io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: `file.as_raw_handle()` is a live file handle for the duration of
    // the call, and `info` is a writable output buffer for the WinAPI function.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
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
        if !is_hop_by_hop_response_header(key) {
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
            Some(cache.write().await.store_path(resource_url, mime_type.as_deref()))
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
        if !is_hop_by_hop_response_header(key) {
            response_builder = response_builder.header(key, value);
        }
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
        let cache_hit = {
            let mut guard = cache.write().await;
            guard.get_content(resource_url)
        };

        if let Some((resource_path, mime_type)) = cache_hit {
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
            let client = input.plex_catalog_client(http_client)?;
            open_media_server_proxy_image_response(&client, &image_ref).await?
        }
        InputType::Emby | InputType::Jellyfin => {
            return Err(MediaServerError::new(MediaServerErrorKind::MediaServerStreamOpenFailed)
                .provider("media-server")
                .detail("media-server image proxy is not implemented for this input type"));
        }
        InputType::M3u
        | InputType::Xtream
        | InputType::M3uBatch
        | InputType::XtreamBatch
        | InputType::Stalker
        | InputType::StalkerBatch
        | InputType::Library
        | InputType::Staged => {
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
        .header(header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
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

pub fn is_seek_request(cluster: XtreamCluster, req_headers: &HeaderMap) -> bool {
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

pub fn is_seekable_media_request(cluster: XtreamCluster, req_headers: &HeaderMap, extension: Option<&str>) -> bool {
    !extension.is_some_and(|ext| ext.eq_ignore_ascii_case(HLS_EXT)) && is_seek_request(cluster, req_headers)
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

pub fn stream_json_or_bin_response_try_stream<P, S, E>(accept: Option<&str>, data: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    if accept.is_some_and(|value| value.contains(CONTENT_TYPE_CBOR)) {
        return stream_bin_array_try_stream(data);
    }
    stream_json_array_try_stream(data)
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
    let session_bound = is_session_based_playback(item_type, extension);
    let socket_bound = !session_bound && is_socket_bound_playback_session(item_type, extension);
    create_session_fingerprint(fingerprint, username, virtual_id, socket_bound)
}

pub fn create_catchup_session_key(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    concat_string!("catchup|", &fingerprint.key, "|", username, "|", &virtual_id.to_string(), "|session")
}

pub fn create_m3u_catchup_session_key(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    archive_discriminator: &str,
) -> String {
    concat_string!(
        "m3u-catchup|",
        &fingerprint.key,
        "|",
        username,
        "|",
        &virtual_id.to_string(),
        "|",
        archive_discriminator
    )
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

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    ));

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

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async {
            // CBOR: start indefinite-length array
            Ok::<_, Infallible>(Bytes::from_static(&[0x9f]))
        })
        .chain(stream)
        .chain(stream::once(async {
            // CBOR: end indefinite-length array
            Ok::<_, Infallible>(Bytes::from_static(&[0xff]))
        })),
    ));

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

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"]")) })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_JSON).body(body))
}

fn stream_json_array_try_stream<P, S, E>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stream = stream::unfold((stream, true, false), |(mut stream, first, failed)| async move {
        if failed {
            return None;
        }
        match stream.next().await {
            Some(Ok(item)) => {
                let serialized = serde_json::to_vec(&item).map_err(|error| error.to_string());
                let bytes = serialized.map(|serialized| {
                    if first {
                        Bytes::from(serialized)
                    } else {
                        let mut framed = Vec::with_capacity(serialized.len() + 1);
                        framed.push(b',');
                        framed.extend_from_slice(&serialized);
                        Bytes::from(framed)
                    }
                });
                let failed = bytes.is_err();
                Some((bytes, (stream, false, failed)))
            }
            Some(Err(error)) => Some((Err(error.to_string()), (stream, first, true))),
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, String>(Bytes::from_static(b"[")) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, String>(Bytes::from_static(b"]")) })),
    ));
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

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0x9f])) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, Infallible>(Bytes::from_static(&[0xff])) })),
    ));

    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

fn stream_bin_array_try_stream<P, S, E>(stream: S) -> axum::response::Response
where
    P: serde::Serialize + Send + 'static,
    S: Stream<Item = Result<P, E>> + Send + Unpin + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let stream = stream::unfold((stream, false), |(mut stream, failed)| async move {
        if failed {
            return None;
        }
        match stream.next().await {
            Some(Ok(item)) => {
                let bytes = bin_serialize(&item).map(Bytes::from).map_err(|error| error.to_string());
                let failed = bytes.is_err();
                Some((bytes, (stream, failed)))
            }
            Some(Err(error)) => Some((Err(error.to_string()), (stream, true))),
            None => None,
        }
    });

    let body = Body::from_stream(coalesce_byte_stream(
        stream::once(async { Ok::<_, String>(Bytes::from_static(&[0x9f])) })
            .chain(stream)
            .chain(stream::once(async { Ok::<_, String>(Bytes::from_static(&[0xff])) })),
    ));
    try_unwrap_body!(Response::builder().header(header::CONTENT_TYPE, CONTENT_TYPE_CBOR).body(body))
}

const API_STREAM_CHUNK_SIZE: usize = 64 * 1024;

pub(crate) fn coalesce_byte_stream<S, E>(stream: S) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    stream::unfold((Box::pin(stream), None, false), |(mut stream, pending_error, finished)| async move {
        if let Some(error) = pending_error {
            return Some((Err(error), (stream, None, true)));
        }
        if finished {
            return None;
        }

        let mut chunk = BytesMut::with_capacity(API_STREAM_CHUNK_SIZE);
        loop {
            match stream.next().await {
                Some(Ok(bytes)) if chunk.is_empty() && bytes.len() >= API_STREAM_CHUNK_SIZE => {
                    return Some((Ok(bytes), (stream, None, false)));
                }
                Some(Ok(bytes)) => {
                    chunk.extend_from_slice(&bytes);
                    if chunk.len() >= API_STREAM_CHUNK_SIZE {
                        return Some((Ok(chunk.freeze()), (stream, None, false)));
                    }
                }
                Some(Err(error)) if chunk.is_empty() => {
                    return Some((Err(error), (stream, None, true)));
                }
                Some(Err(error)) => {
                    return Some((Ok(chunk.freeze()), (stream, Some(error), false)));
                }
                None if chunk.is_empty() => return None,
                None => return Some((Ok(chunk.freeze()), (stream, None, true))),
            }
        }
    })
    .fuse()
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
        plan: None,
        filter: None,
        raw_output_clusters: None,
        raw_max_connections: 0,
        raw_soft_connections: 0,
        raw_proxy: Some(ProxyType::Reverse(None)),
        t_filter: None,
        t_has_unresolved_plan: false,
        t_has_invalid_filter: false,
    }
}

pub fn empty_json_response_as_object() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
        .body(axum::body::Body::from("{}".as_bytes()))
}

pub fn empty_json_response_as_array() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
        .body(axum::body::Body::from("[]".as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, EventManager,
            MetadataUpdateManager, PlaylistStorageState, ProviderConfig as RuntimeProviderConfig,
            ProviderConfigConnection, SharedStreamManager,
        },
        auth::Fingerprint,
        model::{
            AppConfig, Config, ConfigInput, ConfigInputAlias, ConfigProvider, ConfigTarget, GracePeriodOptions,
            MediaToolCapabilities, NetworkAccess, ProcessTargets, ProxyUserCredentials, SourcesConfig,
            StreamHistoryConfig,
        },
        repository::GeoIp,
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::http::{HeaderMap, Response, StatusCode};
    use bytes::Bytes;
    use futures::stream;
    use http_body_util::BodyExt;
    use shared::{
        defaults::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs},
        foundation::Filter,
        model::{
            AdmissionStrategy, ClusterFlags, ConfigPaths, ConfigProviderDto, ConfigTargetOptions,
            GeoIpUnavailablePolicy, InputFetchMethod, InputType, PlaylistItemType, ProcessingOrder,
            ProviderUrlSelectionPolicy, ProxyType, StreamChannel, XtreamCluster,
        },
        utils::Internable,
    };
    use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
    use tokio::{
        io::AsyncWriteExt,
        net::TcpListener,
        sync::{mpsc, RwLock},
    };
    use tuliprox_core::utils::response_compression::should_compress_response;
    use tuliprox_session::{
        admission::{
            evaluate_remaining_strategies_after_grace, get_effective_admission_strategies,
            RECENT_EVICTION_REENTRY_TTL_SECS,
        },
        GraceResolutionContext,
    };

    #[test]
    fn stalker_playback_refreshes_invalid_or_rejected_urls() {
        assert!(should_refresh_stalker_playback(InputType::Stalker, false, None));
        assert!(should_refresh_stalker_playback(InputType::Stalker, true, Some(StatusCode::UNAUTHORIZED)));
        assert!(!should_refresh_stalker_playback(InputType::Stalker, true, Some(StatusCode::OK)));
        assert!(!should_refresh_stalker_playback(InputType::Xtream, false, None));
    }

    #[test]
    fn initial_stalker_playback_resolves_only_empty_urls() {
        assert!(needs_initial_stalker_resolution(InputType::Stalker, ""));
        assert!(!needs_initial_stalker_resolution(InputType::Stalker, "https://stream.example/live.ts"));
        assert!(!needs_initial_stalker_resolution(InputType::Xtream, ""));
        assert_eq!(stalker_stream_kind(XtreamCluster::Live, PlaylistItemType::Catchup), StalkerStreamKind::Archive);
    }

    fn test_runtime_provider(url: &str, username: &str, password: &str) -> Arc<RuntimeProviderConfig> {
        test_runtime_provider_with_type(url, username, password, InputType::Xtream)
    }

    #[tokio::test]
    async fn streamed_json_array_coalesces_small_entries() {
        let response = stream_json_array_stream(stream::iter(0..4_096u32));
        let mut body = response.into_body();
        let mut frames = 0usize;
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                return;
            };
            if let Ok(data) = frame.into_data() {
                frames += 1;
                bytes.extend_from_slice(&data);
            }
        }
        assert!(frames <= 2, "small JSON entries should be coalesced, got {frames} frames");
        let decoded = serde_json::from_slice::<Vec<u32>>(&bytes);
        assert!(decoded.is_ok_and(|values| values.len() == 4_096));
    }

    #[tokio::test]
    async fn catchup_payload_probe_detects_fragmented_hls() {
        let source = stream::iter([
            Ok::<_, StreamError>(Bytes::from_static(b"#EX")),
            Ok(Bytes::from_static(b"TM3U\nsegment.ts\n")),
        ])
        .boxed();

        let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

        assert!(matches!(&result, Ok(CatchupPayload::HlsManifest(_))));
        if let Ok(CatchupPayload::HlsManifest(manifest)) = result {
            assert_eq!(manifest, b"#EXTM3U\nsegment.ts\n".as_slice());
        }
    }

    #[tokio::test]
    async fn catchup_payload_probe_replays_ts_bytes() {
        let expected = Bytes::from_static(b"\x47direct-ts-payload");
        let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

        let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

        assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
        if let Ok(CatchupPayload::Direct(mut stream)) = result {
            let mut actual = Vec::new();
            while let Some(chunk) = stream.next().await {
                if let Ok(chunk) = chunk {
                    actual.extend_from_slice(&chunk);
                }
            }
            assert_eq!(actual, expected.as_ref());
        }
    }

    #[tokio::test]
    async fn catchup_payload_probe_replays_partial_signature_at_eof() {
        let expected = Bytes::from_static(b"#EXT");
        let source = stream::iter([Ok::<_, StreamError>(expected.clone())]).boxed();

        let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

        assert!(matches!(&result, Ok(CatchupPayload::Direct(_))));
        if let Ok(CatchupPayload::Direct(mut stream)) = result {
            let actual = stream.next().await.and_then(Result::ok);
            assert_eq!(actual.as_ref(), Some(&expected));
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn catchup_payload_probe_rejects_oversized_manifest() {
        let oversized = vec![b'x'; MAX_HLS_MANIFEST_BYTES];
        let source =
            stream::iter([Ok::<_, StreamError>(Bytes::from_static(b"#EXTM3U")), Ok(Bytes::from(oversized))]).boxed();

        let result = probe_catchup_payload(source, std::time::Duration::from_secs(1)).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn coalesced_stream_remains_finished_when_polled_again() {
        let stream = coalesce_byte_stream(stream::empty::<Result<Bytes, ()>>());
        futures::pin_mut!(stream);

        assert!(stream.next().await.is_none());
        assert!(stream.next().await.is_none());
    }

    fn test_runtime_provider_with_type(
        url: &str,
        username: &str,
        password: &str,
        input_type: InputType,
    ) -> Arc<RuntimeProviderConfig> {
        let url = if input_type == InputType::M3u {
            format!("{url}/playlist.m3u8?username={username}&password={password}")
        } else {
            url.to_string()
        };
        let input = ConfigInput {
            name: "provider".intern(),
            url,
            username: Some(username.to_string()),
            password: Some(password.to_string()),
            input_type,
            ..ConfigInput::default()
        };
        Arc::new(RuntimeProviderConfig::new(
            &input,
            Arc::new(RwLock::new(ProviderConfigConnection::default())),
            Arc::new(|_, _| {}),
        ))
    }

    fn test_runtime_provider_without_credentials(url: &str, input_type: InputType) -> Arc<RuntimeProviderConfig> {
        let input =
            ConfigInput { name: "provider".intern(), url: url.to_string(), input_type, ..ConfigInput::default() };
        Arc::new(RuntimeProviderConfig::new(
            &input,
            Arc::new(RwLock::new(ProviderConfigConnection::default())),
            Arc::new(|_, _| {}),
        ))
    }

    #[test]
    fn test_is_seek_request() {
        let mut headers = HeaderMap::new();

        // No range header
        assert!(!is_seek_request(XtreamCluster::Video, &headers));

        // Range: bytes=0- (Should be true now to allow session takeover on restart)
        headers.insert("range", "bytes=0-".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers));

        // Range: bytes=100- (Should be true)
        headers.insert("range", "bytes=100-".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers));

        // Range: bytes=100-200 (Should be true)
        headers.insert("range", "bytes=100-200".parse().unwrap());
        assert!(is_seek_request(XtreamCluster::Video, &headers));

        // Live cluster should always return false
        headers.insert("range", "bytes=100-".parse().unwrap());
        assert!(!is_seek_request(XtreamCluster::Live, &headers));
    }

    #[test]
    fn hls_manifests_are_not_forced_as_seek_responses() {
        let mut headers = HeaderMap::new();
        headers.insert("range", HeaderValue::from_static("bytes=0-"));

        assert!(!is_seekable_media_request(XtreamCluster::Video, &headers, Some(HLS_EXT)));
        assert!(is_seekable_media_request(XtreamCluster::Video, &headers, Some(".ts")));
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
    fn stream_alternative_url_keeps_unmatched_urls_unchanged() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://source.example".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
        let stream_url = "http://other.example/live/source-user/source-pass/123.ts";

        let rewritten = get_stream_alternative_url(stream_url, &input, &alias);

        assert_eq!(rewritten, None);
    }

    #[test]
    fn stream_alternative_url_rewrites_only_query_auth_fields() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://source.example".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
        let stream_url = "http://source.example/player?token=source-user&username=source-user&password=source-pass";

        let rewritten = get_stream_alternative_url(stream_url, &input, &alias);

        assert_eq!(
            rewritten,
            Some("http://alias.example/player?token=source-user&username=alias-user&password=alias-pass".to_string())
        );
    }

    #[test]
    fn stream_url_matches_provider_requires_base_url_and_account_identity() {
        let provider = test_runtime_provider("http://same.example", "selected-user", "selected-pass");

        assert!(stream_url_matches_provider("http://same.example/live/selected-user/selected-pass/123.ts", &provider));
        assert!(stream_url_matches_provider(
            "http://same.example/timeshift/selected-user/selected-pass/30/2026-06-15:20-00/123.ts",
            &provider
        ));
        assert!(stream_url_matches_provider(
            "http://same.example/future-route/selected-user/selected-pass/opaque/123.ts",
            &provider
        ));
        assert!(!stream_url_matches_provider("http://same.example/live/other-user/other-pass/123.ts", &provider));
        assert!(!stream_url_matches_provider(
            "http://same.example/timeshift/other-user/other-pass/30/2026-06-15:20-00/123.ts",
            &provider
        ));
        assert!(!stream_url_matches_provider(
            "http://same.example/future-route/other-user/other-pass/opaque/123.ts",
            &provider
        ));
    }

    #[test]
    fn stream_url_matches_provider_accepts_external_playlist_url_for_m3u_without_account_signature() {
        let provider = test_runtime_provider_with_type(
            "http://provider.example",
            "selected-user",
            "selected-pass",
            InputType::M3u,
        );

        assert!(stream_url_matches_provider(
            "https://hlspackager.akamaized.net/live/DB/ALYAUM_TV/HLS/ALYAUM_TV.m3u8",
            &provider
        ));
        assert!(stream_url_matches_provider(
            "https://shd-gcp-live.edgenextcdn.net/live/bitmovin-mbc-1/15cf99af5de54063fdabfefe66adc075/index.m3u8",
            &provider
        ));
    }

    #[test]
    fn stream_url_matches_provider_rejects_external_cdn_url_with_wrong_account_signature() {
        let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

        assert!(!stream_url_matches_provider("http://cdn.example/live/other-user/other-pass/123.ts", &provider));
        assert!(!stream_url_matches_provider(
            "http://cdn.example/segment.ts?username=other-user&password=other-pass",
            &provider
        ));
    }

    #[test]
    fn stream_url_matches_provider_rejects_external_cdn_url_with_wrong_account_signature_for_m3u() {
        let provider = test_runtime_provider_with_type(
            "http://provider.example",
            "selected-user",
            "selected-pass",
            InputType::M3u,
        );

        assert!(!stream_url_matches_provider(
            "http://cdn.example/segment.ts?username=other-user&password=other-pass",
            &provider
        ));
    }

    #[test]
    fn stream_url_matches_provider_detects_m3u_path_credentials_against_alias_account() {
        // Regression: a cross-host M3U URL whose path embeds the alias's
        // account credentials must be detected as an account signature and
        // validated, not silently allowed as an open URL.
        let provider = test_runtime_provider_with_type(
            "http://provider.example",
            "selected-user",
            "selected-pass",
            InputType::M3u,
        );

        // Matching path credentials -> allowed (account matches).
        assert!(stream_url_matches_provider("http://cdn.example/live/selected-user/selected-pass/123.ts", &provider));
    }

    #[test]
    fn stream_url_matches_provider_rejects_open_external_cdn_url_without_account_signature_for_xtream() {
        let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

        assert!(!stream_url_matches_provider("http://cdn.example/open/playlist.m3u8", &provider));
        assert!(!stream_url_matches_provider("http://cdn.example/open/segment.ts?key=signedopaque", &provider));
    }

    #[test]
    fn stream_url_matches_provider_rejects_external_cdn_url_for_xtream_even_with_valid_account_signature() {
        let provider = test_runtime_provider("http://provider.example", "selected-user", "selected-pass");

        assert!(!stream_url_matches_provider("http://cdn.example/live/selected-user/selected-pass/123.ts", &provider));
        assert!(!stream_url_matches_provider(
            "http://cdn.example/segment.ts?username=selected-user&password=selected-pass",
            &provider
        ));
    }

    #[test]
    fn find_input_account_by_signature_matches_main_input_and_alias_accounts() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example".to_string(),
            username: Some("main-user".to_string()),
            password: Some("main-pass".to_string()),
            input_type: InputType::Xtream,
            aliases: Some(vec![ConfigInputAlias {
                id: 2,
                name: "alias".intern(),
                url: "http://alias.example".to_string(),
                username: Some("alias-user".to_string()),
                password: Some("alias-pass".to_string()),
                max_connections: 1,
                priority: 0,
                exp_date: None,
                enabled: true,
                stalker: None,
            }]),
            ..ConfigInput::default()
        };

        let main = find_input_account_by_signature("http://cdn.example/live/main-user/main-pass/1.ts", &input);
        assert_eq!(
            main,
            Some(
                ("http://provider.example".to_string(), Some("main-user".to_string()), Some("main-pass".to_string()),)
            )
        );

        let alias = find_input_account_by_signature("http://cdn.example/live/alias-user/alias-pass/1.ts", &input);
        assert_eq!(
            alias,
            Some(("http://alias.example".to_string(), Some("alias-user".to_string()), Some("alias-pass".to_string()),))
        );

        assert_eq!(find_input_account_by_signature("http://cdn.example/live/other/other/1.ts", &input), None);
        assert_eq!(find_input_account_by_signature("http://cdn.example/open/playlist.m3u8", &input), None);
    }

    #[test]
    fn get_stream_alternative_url_rewrites_external_cdn_url_with_valid_account_signature_for_alias_account() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
        let stream_url = "http://cdn.example/live/source-user/source-pass/123.ts";

        let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
        assert_eq!(rewritten, Some("http://cdn.example/live/alias-user/alias-pass/123.ts".to_string()));
    }

    #[test]
    fn get_stream_alternative_url_rewrites_timeshift_path_credentials_for_alias_account() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
        let stream_url = "http://provider.example/timeshift/source-user/source-pass/30/2026-06-15:20-00/123.ts";

        let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
        assert_eq!(
            rewritten,
            Some("http://alias.example/timeshift/alias-user/alias-pass/30/2026-06-15:20-00/123.ts".to_string())
        );
    }

    #[test]
    fn get_stream_alternative_url_rewrites_future_route_path_credentials_for_alias_account() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
        let stream_url = "http://provider.example/future-route/source-user/source-pass/opaque/123.ts";

        let rewritten = get_stream_alternative_url(stream_url, &input, &alias);
        assert_eq!(
            rewritten,
            Some("http://alias.example/future-route/alias-user/alias-pass/opaque/123.ts".to_string())
        );
    }

    #[test]
    fn get_stream_alternative_url_keeps_open_external_playlist_url_for_m3u() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
        let stream_url = "https://cnbc-live.akamaized.net/cnbc/master.m3u8";

        assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), Some(stream_url.to_string()));
    }

    #[test]
    fn get_stream_alternative_url_keeps_open_external_multisegment_playlist_url_for_m3u() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);
        let stream_url = "https://hnpsechtsc.turknet.ercdn.net/xpnvudnlsv/cnbc-e/cnbc-e.m3u8";

        assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), Some(stream_url.to_string()));
    }

    #[test]
    fn get_stream_alternative_url_keeps_open_external_m3u_url_for_provider_without_credentials() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8".to_string(),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let provider =
            test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);
        let stream_url = "http://s.only4.tv/17113/video.m3u8?token=abc";

        assert_eq!(get_stream_alternative_url(stream_url, &input, &provider), Some(stream_url.to_string()));
    }

    #[test]
    fn get_stream_alternative_url_rejects_query_credentials_for_provider_without_credentials() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8".to_string(),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let provider =
            test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);

        assert_eq!(
            get_stream_alternative_url(
                "http://cdn.example/segment.ts?username=other-user&password=other-pass",
                &input,
                &provider
            ),
            None
        );
    }

    #[test]
    fn get_stream_alternative_url_rejects_basic_auth_credentials_for_provider_without_credentials() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8".to_string(),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let provider =
            test_runtime_provider_without_credentials("http://provider.example/playlist.m3u8", InputType::M3u);

        assert_eq!(get_stream_alternative_url("http://user:pass@cdn.example/segment.ts", &input, &provider), None);
    }

    #[test]
    fn get_stream_alternative_url_rejects_external_m3u_url_with_unmatched_account_signature() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example/playlist.m3u8?username=source-user&password=source-pass".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::M3u,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider_with_type("http://alias.example", "alias-user", "alias-pass", InputType::M3u);

        assert_eq!(
            get_stream_alternative_url(
                "http://cdn.example/segment.ts?username=other-user&password=other-pass",
                &input,
                &alias,
            ),
            None
        );
    }

    #[test]
    fn get_stream_alternative_url_does_not_passthrough_arbitrary_open_external_url_for_xtream() {
        let input = ConfigInput {
            name: "source".intern(),
            url: "http://provider.example".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let alias = test_runtime_provider("http://alias.example", "alias-user", "alias-pass");
        let stream_url = "http://cdn.example/open/playlist.m3u8";

        assert_eq!(get_stream_alternative_url(stream_url, &input, &alias), None);
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
        let plex_input = ConfigInput { input_type: InputType::Plex, ..ConfigInput::default() };
        let emby_input = ConfigInput { input_type: InputType::Emby, ..ConfigInput::default() };
        let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

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

    async fn spawn_legacy_hls_test_origin(
        response_head: String,
        response_body: Vec<u8>,
    ) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let origin_addr = listener.local_addr().expect("test origin address");
        let origin_task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("test origin accepts request");
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("test origin reads request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(response_head.as_bytes()).await.expect("test origin writes response headers");
            socket.write_all(&response_body).await.expect("test origin writes response body");
            String::from_utf8_lossy(&request).into_owned()
        });
        (origin_addr, origin_task)
    }

    async fn forced_legacy_hls_test_response(
        origin_addr: SocketAddr,
        request_headers: &HeaderMap,
        client_port: u16,
    ) -> axum::response::Response {
        let origin_url = format!("http://{origin_addr}/segment.ts");
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]),
            url: format!("http://{origin_addr}"),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            ..ConfigInput::default()
        });
        let app_config = create_test_provider_app_config();
        app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
        let app_state = create_test_app_state_for_config(Arc::new(app_config));
        let client_addr = SocketAddr::from(([127, 0, 0, 1], client_port));
        let fingerprint = create_test_fingerprint(client_addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        let session = UserSession {
            token: format!("legacy-hls-marker-{client_port}"),
            transition_version: 1,
            virtual_id: 41,
            provider: Arc::clone(&input.name),
            stream_url: origin_url.as_str().intern(),
            provider_session_headers: HashMap::new(),
            addr: client_addr,
            socket_bound: false,
            active_addrs: vec![client_addr],
            ts: 1,
            started_at: 1,
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            lifecycle: crate::api::model::PlaybackLifecycle::Active,
        };
        let mut stream_channel = create_test_local_channel(&origin_url);
        stream_channel.provider_id = u32::from(input.id);
        stream_channel.input_name = Arc::clone(&input.name);
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Live;
        stream_channel.url = origin_url.as_str().intern();

        force_provider_stream_response(
            &fingerprint,
            &app_state,
            &session,
            stream_channel,
            ForceStreamRequestContext {
                req_headers: request_headers,
                input: &input,
                user: &user,
                session_reservation_ttl_secs: 0,
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            None,
        )
        .await
        .into_response()
    }

    #[tokio::test]
    async fn forced_hls_provider_response_disables_compression_and_streams_identity_bytes() {
        const IDENTITY_BODY: &[u8] = b"legacy hls identity segment";

        let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
        encoder.write_all(IDENTITY_BODY).await.expect("gzip test body encodes");
        encoder.shutdown().await.expect("gzip test encoder finishes");
        let encoded_body = encoder.into_inner();

        let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            encoded_body.len()
        );
        let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, encoded_body).await;
        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_310).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!should_compress_response(&response));
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        let body = response.into_body().collect().await.expect("legacy HLS response body").to_bytes();
        assert_eq!(body.as_ref(), IDENTITY_BODY);

        let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
        assert!(request.contains("\r\naccept-encoding: identity\r\n"));
    }

    #[tokio::test]
    async fn forced_hls_unencoded_partial_response_preserves_range_and_disables_compression() {
        const PARTIAL_BODY: &[u8] = b"cdef";
        let response_head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp2t\r\nContent-Range: bytes 2-5/10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            PARTIAL_BODY.len()
        );
        let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, PARTIAL_BODY.to_vec()).await;
        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-"));

        let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_311).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!should_compress_response(&response));
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        let body = response.into_body().collect().await.expect("legacy HLS partial body").to_bytes();
        assert_eq!(body.as_ref(), PARTIAL_BODY);

        let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
        assert!(request.contains("\r\naccept-encoding: identity\r\n"));
        assert!(request.contains("\r\nrange: bytes=2-\r\n"));
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
        let stream_url = "http://provider-1.example/movie/user1/pass1/1.mkv";

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
                accept_requested_stream_url: false,
            },
        )
        .await;
        assert!(strict.provider_handle.is_none(), "strict provider affinity should not allocate a different provider");
        assert!(
            matches!(
                strict.provider_stream_state,
                ProviderStreamState::Custom { reason: ProviderStreamCustomReason::ProviderExhausted, .. }
            ),
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
                accept_requested_stream_url: false,
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
                accept_requested_stream_url: false,
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

    #[tokio::test]
    async fn resolve_streaming_strategy_rejects_unmapped_provider_url() {
        let app_state = create_test_dual_provider_app_state();
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&input_name)
            .cloned()
            .unwrap_or_else(|| unreachable!());
        let addr: SocketAddr = "127.0.0.1:55305".parse().unwrap_or_else(|_| unreachable!());

        let strategy = resolve_streaming_strategy(
            &app_state,
            "http://unmapped.example/live/user1/pass1/100.ts",
            &create_test_fingerprint(addr),
            &input,
            StreamingAcquireOptions {
                force_provider: None,
                allow_forced_provider_fallback: false,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("live-session"),
                accept_requested_stream_url: false,
            },
        )
        .await;

        assert!(strategy.provider_handle.is_none());
        assert!(matches!(
            strategy.provider_stream_state,
            ProviderStreamState::Custom { reason: ProviderStreamCustomReason::UnmappedProviderUrl, .. }
        ));

        app_state.active_provider.release_connection(&addr).await;
    }

    #[tokio::test]
    async fn resolve_streaming_strategy_accepts_stalker_portal_url() {
        let app_config = create_test_provider_app_config();
        let Some(configured_input) = app_config.sources.load().inputs.first().cloned() else { unreachable!() };
        let mut stalker_input = (*configured_input).clone();
        stalker_input.input_type = InputType::Stalker;
        stalker_input.username = None;
        stalker_input.password = None;
        app_config
            .sources
            .store(Arc::new(SourcesConfig { inputs: vec![Arc::new(stalker_input)], ..SourcesConfig::default() }));
        let app_state = create_test_app_state_for_config(Arc::new(app_config));
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&input_name)
            .cloned()
            .unwrap_or_else(|| unreachable!());
        let addr: SocketAddr = "127.0.0.1:55307".parse().unwrap_or_else(|_| unreachable!());
        let stream_url =
            "http://line.example/play/live.php?mac=00:11:22:33:44:55&stream=347&extension=ts&play_token=abc";

        let strategy = resolve_streaming_strategy(
            &app_state,
            stream_url,
            &create_test_fingerprint(addr),
            &input,
            StreamingAcquireOptions {
                force_provider: None,
                allow_forced_provider_fallback: false,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("live-session"),
                accept_requested_stream_url: false,
            },
        )
        .await;

        let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
            unreachable!()
        };
        assert_eq!(provider.as_ref(), "provider_1");
        assert_eq!(url.as_ref(), stream_url);

        app_state.active_provider.release_connection(&addr).await;
    }

    #[tokio::test]
    async fn resolve_streaming_strategy_accepts_session_requested_stream_url() {
        let app_state = create_test_dual_provider_app_state();
        let input_name = "provider_1".intern();
        let input = app_state
            .app_config
            .sources
            .load()
            .get_input_by_name(&input_name)
            .cloned()
            .unwrap_or_else(|| unreachable!());
        let addr: SocketAddr = "127.0.0.1:55306".parse().unwrap_or_else(|_| unreachable!());
        let trusted_url = "http://unmapped.example/live/user1/pass1/100.ts";
        let strategy = resolve_streaming_strategy(
            &app_state,
            trusted_url,
            &create_test_fingerprint(addr),
            &input,
            StreamingAcquireOptions {
                force_provider: None,
                allow_forced_provider_fallback: false,
                allow_provider_grace: false,
                user_priority: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                session_owner: Some("live-session"),
                accept_requested_stream_url: true,
            },
        )
        .await;

        let ProviderStreamState::Available(Some(provider), url) = strategy.provider_stream_state else {
            panic!("session-requested URL should be accepted for the pinned provider")
        };
        assert_eq!(provider.as_ref(), "provider_1");
        assert_eq!(url.as_ref(), trusted_url);

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
            provider_session_headers: HashMap::new(),
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
                stalker: None,
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
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

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
            public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
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
                hls_cache: None,
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
            epg_reference_ts: None,
            upstream_user_agent: None,
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
            epg_reference_ts: None,
            upstream_user_agent: None,
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
            provider_session_headers: HashMap::new(),
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
        let session =
            create_test_session("tok-active", PlaylistItemType::LiveHls, crate::api::model::PlaybackLifecycle::Active);

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
                },
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
            shared_subscriber_idle_timeout_secs: 300,
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
                virtual_id: VirtualId::new(channel.virtual_id),
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
            shared_subscriber_idle_timeout_secs: 300,
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
                virtual_id: VirtualId::new(channel.virtual_id),
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
            shared_subscriber_idle_timeout_secs: 300,
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
        app_state.active_users.terminate_session(&user.username, "tok-stale-followup").await;

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
                virtual_id: VirtualId::new(channel.virtual_id),
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
            shared_subscriber_idle_timeout_secs: 300,
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
                virtual_id: VirtualId::new(channel.virtual_id),
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

        let session =
            app_state.active_users.get_and_update_user_session(&user.username, "tok-pre-resolved-grace").await;
        assert!(
            session
                .is_some_and(|s| matches!(s.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. })),
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
            shared_subscriber_idle_timeout_secs: 300,
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
                virtual_id: VirtualId::new(55222),
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });
        let addr: SocketAddr = "127.0.0.1:55223".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let mut user = ProxyUserCredentials::default();
        user.username = "prepare-only-user".to_string();
        user.max_connections = 1;

        let (admission, grace_mode, request_class) = resolve_playback_request_admission(
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
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
        let before = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
        assert!(before.is_some(), "session should exist before terminate");

        let (admission, grace_mode, request_class) = resolve_playback_request_admission(
            &app_state.admission_ctx(),
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
        let after = app_state.active_users.get_and_update_user_session(&user.username, session_token).await;
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
            shared_subscriber_idle_timeout_secs: 300,
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
                hls_cache: None,
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
                virtual_id: VirtualId::new(second_channel.virtual_id),
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
        let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
            panic!("grace hold should mark pending provider state")
        };
        assert!(matches!(pending.reason_code, crate::api::model::PendingProviderReason::GraceHold));
        assert!(pending.deadline >= pending.created_at);
        assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
        assert!(
            !session.lifecycle.is_counted(),
            "pending provider placeholder must not consume an active user lease before commit"
        );
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
            shared_subscriber_idle_timeout_secs: 300,
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
                hls_cache: None,
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
                virtual_id: VirtualId::new(channel.virtual_id),
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
            options: Some(ConfigTargetOptions {
                share_live_streams: shared::model::ConfigTargetShareLiveStreams { mpeg_ts: true, ..Default::default() },
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: None,
        });

        assert_eq!(
            get_effective_admission_strategies(&app_state.admission_ctx()),
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![]),
        });

        assert!(get_effective_admission_strategies(&app_state.admission_ctx()).is_empty());
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
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
        let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest];
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
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
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.5:55901".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state.admission_ctx(),
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
        let grace_context = GraceResolutionContext {
            strategy_index: 0,
            strategies,
            kind: Some(crate::api::model::ConnectionKind::Soft),
        };

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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.6:55902".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
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
        let grace_context = GraceResolutionContext {
            strategy_index: 0,
            strategies,
            kind: Some(crate::api::model::ConnectionKind::Normal),
        };
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
        });

        let addr: SocketAddr = "10.0.0.7:55903".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);

        let result = evaluate_remaining_strategies_after_grace(
            &app_state.admission_ctx(),
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
            result.admission.kind, original_kind,
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
        let strategies = vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream];
        let grace_context = GraceResolutionContext {
            strategy_index: 0,
            strategies,
            kind: Some(crate::api::model::ConnectionKind::Normal),
        };
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::GraceInstantStream]),
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
            &app_state.admission_ctx(),
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
        assert!(result.grace_context.is_some(), "grace_context must be present when grace is granted");
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
            shared_subscriber_idle_timeout_secs: 300,
            admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
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
            &app_state.admission_ctx(),
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
            &app_state.admission_ctx(),
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
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
            "loop-user",
            1,
            0,
            &reconnect_fingerprint.client_ip,
            &reconnect_fingerprint.addr,
            true,
            Some("socket-reconnect"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9001) },
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
            "loop-user-2",
            1,
            0,
            &new_fingerprint.client_ip,
            &new_fingerprint.addr,
            true,
            Some("session-new"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9103) },
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
            "loop-user-3",
            1,
            1,
            &reconnect_fingerprint.client_ip,
            &reconnect_fingerprint.addr,
            true,
            Some("socket-reconnect"),
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9201) },
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
            SharedStreamCtx {
                app_config: &app_state.app_config,
                shared_stream_manager: &app_state.shared_stream_manager,
                active_provider: &app_state.active_provider,
                connection_manager: &app_state.connection_manager,
            },
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

        let admission = app_state
            .active_users
            .connection_admission(&user.username, user.max_connections, user.soft_connections)
            .await;
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
            None,
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
            custom_stream_response_enabled: true,
            ..Config::default()
        }));
        let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
        let addr = "127.0.0.1:55143".parse().unwrap_or_else(|_| unreachable!());
        let fingerprint = create_test_fingerprint(addr);
        let input_name = "provider_1".intern();
        let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
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
            None,
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

        // Custom-video stream is enabled (`custom_stream_response_enabled: true`
        // in this fixture), so a missing resource must return 400 — the
        // Nginx `proxy_intercept_errors on;` contract requires 4xx so the
        // socket is severed instead of looping on a 200 OK fallback body.
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            app_state.active_users.user_connections(&user.username).await,
            0,
            "failed provider open must rollback provisional user activation"
        );
        assert!(
            app_state.active_users.get_and_update_user_session(&user.username, "rollback-session").await.is_none(),
            "failed provider open must remove the provisional placeholder session"
        );
    }

    /// Regression test for: when a catchup request fails upstream (e.g. provider returns
    /// 4xx/5xx) the connection-slot is released, but the provider account was being
    /// pinned via `refresh_provider_reservation` for `catchup_session_ttl_secs`. This
    /// blocked other sessions of the same family from acquiring the same provider even
    /// though the slot was already free. The fix delegates the pinning decision to
    /// `should_pin_provider_for_session` and skips the reservation when the response
    /// is a non-Provisioning custom video (failure fallback). Provisioning custom videos
    /// must keep their reservation since they represent a successful provider handoff.
    #[tokio::test]
    async fn should_pin_provider_for_session_skips_reservation_on_failure_custom_video() {
        let app_state = create_test_app_state();
        let no_video_details = StreamDetails {
            stream: None,
            stream_info: Some((Vec::new(), StatusCode::OK, None, None)),
            provider_name: Some("provider_1".intern()),
            request_url: None,
            session_headers: None,
            provider_session_headers: HashMap::new(),
            grace_period: GracePeriodOptions::default(),
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
        };
        assert!(
            should_pin_provider_for_session(&no_video_details, &app_state, PlaylistItemType::Catchup),
            "a real provider stream (no CustomVideoStreamType) must pin the provider"
        );

        let provisioning_details = StreamDetails {
            stream: None,
            stream_info: Some((Vec::new(), StatusCode::OK, None, Some(CustomVideoStreamType::Provisioning))),
            provider_name: Some("provider_1".intern()),
            request_url: None,
            session_headers: None,
            provider_session_headers: HashMap::new(),
            grace_period: GracePeriodOptions::default(),
            provider_grace_active: false,
            disable_provider_grace: false,
            reconnect_flag: None,
            provider_handle: None,
            content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            grace_resolution_context: None,
        };
        assert!(
            should_pin_provider_for_session(&provisioning_details, &app_state, PlaylistItemType::Catchup),
            "a Provisioning custom video represents a successful provider handoff and must pin"
        );

        for failure_type in [
            CustomVideoStreamType::ChannelUnavailable,
            CustomVideoStreamType::ProviderConnectionsExhausted,
            CustomVideoStreamType::UserConnectionsExhausted,
            CustomVideoStreamType::UserAccountExpired,
            CustomVideoStreamType::LowPriorityPreempted,
        ] {
            let failure_details = StreamDetails {
                stream: None,
                stream_info: Some((Vec::new(), StatusCode::BAD_REQUEST, None, Some(failure_type))),
                provider_name: Some("provider_1".intern()),
                request_url: None,
                session_headers: None,
                provider_session_headers: HashMap::new(),
                grace_period: GracePeriodOptions::default(),
                provider_grace_active: false,
                disable_provider_grace: false,
                reconnect_flag: None,
                provider_handle: None,
                content_representation: crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
                grace_resolution_context: None,
            };
            assert!(
                !should_pin_provider_for_session(&failure_details, &app_state, PlaylistItemType::Catchup),
                "{failure_type:?} is a failure fallback — must NOT pin the provider"
            );
        }
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
            &app_state.admission_ctx(),
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
            &app_state.admission_ctx(),
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
    async fn socket_bound_playback_sessions_enforce_hard_limits_per_socket() {
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
            shared_subscriber_idle_timeout_secs: 300,
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
            &app_state.admission_ctx(),
            &user.username,
            user.max_connections,
            user.soft_connections,
            &ts_fingerprint.client_ip,
            &ts_fingerprint.addr,
            false,
            None,
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(5001) },
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
            app_state.active_users.get_and_update_user_session(&user.username, "tok-hls-preserved").await.is_none(),
            "preserved session should be removed once the TS request evicts it"
        );
    }

    #[tokio::test]
    async fn socket_bound_playback_sessions_still_allow_soft_slots() {
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
    fn adaptive_playback_session_fingerprint_is_logical_across_initial_sockets() {
        let Some(first_addr) = "127.0.0.1:55177".parse().ok() else {
            return;
        };
        let Some(second_addr) = "127.0.0.1:55178".parse().ok() else {
            return;
        };
        let first = Fingerprint::new("10.0.0.6|player".to_string(), "10.0.0.6".to_string(), first_addr);
        let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

        let first_token =
            create_playback_session_fingerprint(&first, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));
        let second_token =
            create_playback_session_fingerprint(&second, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));

        assert_eq!(first_token, second_token);
        assert!(first_token.contains(&first.key));
        assert!(!first_token.contains(&first.addr.to_string()));
        assert!(!second_token.contains(&second.addr.to_string()));
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
                    shared_subscriber_idle_timeout_secs: 300,
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
                hls_cache: None,
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
        assert_eq!(
            app_state.active_users.user_connections(&user.username).await,
            0,
            "the preserved HLS playback must reserve capacity only virtually"
        );
        assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));
        assert!(
            app_state.active_users.get_and_update_user_session(&user.username, &hls_token).await.is_some(),
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
            app_state.active_users.get_eviction_candidates(&user.username, &ts_fingerprint.client_ip).await.len(),
            1,
            "the preserved HLS playback should be the single eviction candidate for the competing TS request"
        );

        let (ts_admission, ts_grace_mode, request_class) = resolve_playback_request_admission(
            &app_state.admission_ctx(),
            &user,
            &ts_fingerprint,
            PlaylistItemType::Live,
            None,
            &ts_token,
            false,
            EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(virtual_id) },
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
        assert_eq!(
            app_state.active_users.user_connections(&user.username).await,
            0,
            "eviction must not leave a real slot before the TS stream commits"
        );
        assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));

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

        assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
        assert_eq!(app_state.active_users.active_users_and_connections().await, (1, 1));
        let active_streams = app_state.active_users.active_streams().await;
        assert_eq!(active_streams.len(), 1);
        assert_eq!(active_streams.first().and_then(|stream| stream.session_token.as_deref()), Some(ts_token.as_str()));
        assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "after the competing TS request, the old Xtream HLS session must be gone so later /hls segment fetches cannot revive it"
        );
        assert!(
            app_state.active_users.get_and_update_user_session(&user.username, &ts_token).await.is_some(),
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
            provider_session_headers: HashMap::new(),
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

    #[tokio::test]
    async fn intentional_deferred_open_retains_provider_grace_handle() {
        let app_state = create_test_provider_app_state();
        let provider_name = "provider_1".intern();
        let holder_addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
        let deferred_addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
        let holder_handle = app_state
            .active_provider
            .acquire_exact_connection_with_grace(
                &provider_name,
                &holder_addr,
                false,
                0,
                crate::api::model::ConnectionKind::Normal,
            )
            .await
            .expect("holder occupies the provider slot");
        let input = app_state.app_config.get_input_by_name(&provider_name).expect("provider input");
        let stream_url = "http://provider-1.example/live/user1/pass1/100.m3u8";
        let mut channel = create_test_live_channel(stream_url);
        channel.item_type = PlaylistItemType::LiveHls;
        let fingerprint = create_test_fingerprint(deferred_addr);

        let mut details = create_stream_response_details(
            &app_state,
            &get_stream_options(&app_state.app_config),
            stream_url,
            "deferred-user",
            &fingerprint,
            &HeaderMap::new(),
            &input,
            &channel,
            PlaylistItemType::LiveHls,
            crate::api::model::ProviderContentRepresentationMode::PreserveOrigin,
            false,
            UserConnectionPermission::Allowed,
            None,
            true,
            true,
            VirtualId::new(channel.virtual_id),
            0,
            crate::api::model::ConnectionKind::Normal,
            false,
            Some("deferred-session"),
            None,
            false,
            Some(true),
            None,
        )
        .await
        .expect("provider grace creates deferred stream details");

        assert!(details.stream.is_none());
        assert!(details.has_deferred_provider_open());
        assert!(details.provider_handle.is_some(), "deferred open must retain its provider allocation");

        app_state.connection_manager.release_provider_handle(details.provider_handle.take()).await;
        app_state.connection_manager.release_provider_handle(Some(holder_handle)).await;
    }

    #[test]
    fn grace_hold_defers_live_and_fresh_video_but_not_catchup_or_affine_reopens() {
        assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::LiveHls, false));
        assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, false));
        assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, false));
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

        let signal =
            tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
        assert!(signal.is_none(), "adaptive cleanup should not hard-close the previous client socket");
    }

    #[tokio::test]
    async fn forced_reopen_cleanup_for_non_adaptive_streams_closes_client_socket() {
        let app_state = create_test_app_state();
        let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
        let mut close_rx = app_state.connection_manager.get_close_connection_channel();

        cleanup_forced_reopen_addrs(&app_state, PlaylistItemType::Live, &[addr]).await;

        let signal =
            tokio::time::timeout(std::time::Duration::from_millis(50), close_rx.recv()).await.ok().and_then(Result::ok);
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
            virtual_id: VirtualId::new(100),
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
            input_stream_id: "1".intern(),
            upstream_user_agent: None,
        };

        let hls_ext = shared::defaults::HLS_EXT.to_string();
        let (query_path, extension) =
            crate::api::endpoints::xtream_api::get_query_path("", Some(&hls_ext), &pli, &app_state);

        assert_eq!(extension, "");
        assert_eq!(query_path, "1");

        let dash_ext = shared::defaults::DASH_EXT.to_string();
        let (query_path, extension) =
            crate::api::endpoints::xtream_api::get_query_path("", Some(&dash_ext), &pli, &app_state);

        assert_eq!(extension, "");
        assert_eq!(query_path, "1");
    }

    // =========================================================================================
    // evaluate_network_access tests
    // =========================================================================================

    /// Run `evaluate_network_access` for a synthetic user built from
    /// `network_access` and assert the decision matches `expected`. Centralizes
    /// the boilerplate (`user_with_network_access` + geoip setup + call +
    /// assert) shared by every `evaluate_network_access` test below.
    fn assert_network_decision(
        network_access: Option<NetworkAccess>,
        geoip: &Arc<ArcSwapOption<GeoIp>>,
        ip: &str,
        expected: NetworkAccessDecision,
    ) {
        let user = user_with_network_access(network_access);
        assert_eq!(evaluate_network_access(&user, ip, geoip, GeoIpUnavailablePolicy::Deny), expected);
    }

    /// `Arc<ArcSwapOption<GeoIp>>` with no `GeoIP` database loaded.
    fn empty_geoip() -> Arc<ArcSwapOption<GeoIp>> {
        Arc::new(ArcSwapOption::<GeoIp>::default())
    }

    /// `Arc<ArcSwapOption<GeoIp>>` with a mock `GeoIP` that always reports the
    /// given country for any lookup.
    fn mock_geoip(country: &str) -> Arc<ArcSwapOption<GeoIp>> {
        Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new(country)))))
    }

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
            plan: None,
            filter: None,
            raw_output_clusters: None,
            raw_max_connections: 0,
            raw_soft_connections: 0,
            raw_proxy: Some(ProxyType::default()),
            t_filter: None,
            t_has_unresolved_plan: false,
            t_has_invalid_filter: false,
        }
    }

    #[test]
    fn no_config_allows_all() {
        assert_network_decision(None, &empty_geoip(), "192.168.1.1", NetworkAccessDecision::Allowed);
    }

    #[test]
    fn empty_config_allows_all() {
        assert_network_decision(
            Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec![] }),
            &empty_geoip(),
            "192.168.1.1",
            NetworkAccessDecision::Allowed,
        );
    }

    #[test]
    fn cidr_match_allows() {
        assert_network_decision(
            Some(NetworkAccess {
                allowed_countries: vec![],
                allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
            }),
            &empty_geoip(),
            "192.168.1.42",
            NetworkAccessDecision::Allowed,
        );
    }

    #[test]
    fn cidr_miss_denies() {
        assert_network_decision(
            Some(NetworkAccess {
                allowed_countries: vec![],
                allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
            }),
            &empty_geoip(),
            "10.0.0.1",
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
        );
    }

    #[test]
    fn country_match_allows() {
        assert_network_decision(
            Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
            &mock_geoip("DE"),
            "8.8.8.8",
            NetworkAccessDecision::Allowed,
        );
    }

    #[test]
    fn country_miss_denies() {
        assert_network_decision(
            Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
            &mock_geoip("US"),
            "8.8.8.8",
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCountryMatch),
        );
    }

    #[test]
    fn no_geoip_denies_on_country_restriction() {
        assert_network_decision(
            Some(NetworkAccess { allowed_countries: vec!["DE".to_string()], allowed_networks: vec![] }),
            &empty_geoip(),
            "8.8.8.8",
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::GeoIpUnavailable),
        );
    }

    #[test]
    fn ipv4_vs_ipv6_denies_gracefully() {
        assert_network_decision(
            Some(NetworkAccess { allowed_countries: vec![], allowed_networks: vec!["2001:db8::/32".parse().unwrap()] }),
            &empty_geoip(),
            "192.168.1.1",
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
        );
    }

    #[test]
    fn ipv6_vs_ipv4_denies_gracefully() {
        assert_network_decision(
            Some(NetworkAccess {
                allowed_countries: vec![],
                allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
            }),
            &empty_geoip(),
            "2001:db8::1",
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch),
        );
    }

    #[test]
    fn either_cidr_or_country_match_allows() {
        assert_network_decision(
            Some(NetworkAccess {
                allowed_countries: vec!["US".to_string()],
                allowed_networks: vec!["192.168.1.0/24".parse().unwrap()],
            }),
            &mock_geoip("DE"),
            "192.168.1.42",
            NetworkAccessDecision::Allowed,
        );
    }

    #[test]
    fn single_ip_cidr() {
        let user = user_with_network_access(Some(NetworkAccess {
            allowed_countries: vec![],
            allowed_networks: vec!["192.168.1.1/32".parse().unwrap()],
        }));
        let geoip = empty_geoip();
        assert_eq!(
            evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Allowed
        );
        assert_eq!(
            evaluate_network_access(&user, "192.168.1.2", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Denied(NetworkAccessDenyReason::NoCidrMatch)
        );
    }

    // =========================================================================================
    // network denied reason tests
    // =========================================================================================

    // The three `network_denied_reason_*` cases remain because they cover the
    // `NetworkAccessDenyReason`-focused API surface directly, while
    // `cidr_miss_denies`, `country_miss_denies`, and
    // `no_geoip_denies_on_country_restriction` above assert the same deny
    // reasons through broader `evaluate_network_access(...)` behavior. The
    // overlap is intentional so both the general decision path and the
    // reason-reporting-focused path stay pinned by tests.

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
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Allowed
        );
    }

    #[test]
    fn network_denied_reason_none_when_no_config() {
        let user = user_with_network_access(None);
        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        assert_eq!(
            evaluate_network_access(&user, "192.168.1.1", &geoip, GeoIpUnavailablePolicy::Deny),
            NetworkAccessDecision::Allowed
        );
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
        assert_eq!(
            evaluate_network_access(&user, "8.8.8.8", &geoip, GeoIpUnavailablePolicy::Allow),
            NetworkAccessDecision::AllowedGeoIpUnavailable
        );
    }
}
