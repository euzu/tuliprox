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
    repository::load_input_m3u_stream_url,
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
use log::{debug, error, info, log_enabled, trace, warn};
use serde::Serialize;
use shared::{
    concat_string,
    defaults::{DASH_EXT, HLS_EXT},
    model::{
        ConfigTargetOptions, InputFetchMethod, InputType, PlaylistEntry, PlaylistItemType, ProxyType,
        StalkerStreamKind, StreamChannel, StreamInfo, TargetType, UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        bin_serialize, current_time_secs, extract_extension_from_url, get_credentials_from_url, human_readable_kbps,
        is_account_query_key, is_sanitize_sensitive_info_enabled, replace_url_extension, sanitize_sensitive_info,
        trim_slash, Internable, CONTENT_TYPE_CBOR, CONTENT_TYPE_JSON,
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
    sync::RwLock,
};
use tokio_util::io::ReaderStream;
use tuliprox_hls::api::MAX_HLS_MANIFEST_BYTES;
use url::Url;

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
        resolve_playback_request_admission, AdmissionRequest, EvictionReentryGuard, PlaybackRequestClass,
        PlaybackRequestFacts,
    },
    stream_options::{get_stream_options, StreamOptions},
};

pub fn get_server_time() -> String {
    chrono::offset::Local::now().with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %Z").to_string()
}

static PROCESS_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// Anchors the uptime clock; call once at process startup.
pub fn init_uptime_clock() { let _ = *PROCESS_START; }

pub fn get_uptime_secs() -> u64 { PROCESS_START.elapsed().as_secs() }

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
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some(session_token),
            activate_unbound_session: true,
            eviction_reentry_guard: if socket_bound {
                EvictionReentryGuard::SocketPlayback { virtual_id }
            } else {
                EvictionReentryGuard::Session(session_token)
            },
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
    if let Some(source_config_url) = find_input_account_by_query_signature(stream_url, input) {
        return rewrite_account_query_fields(stream_url, source_config_url, &alias_input.url);
    }

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

fn find_input_account_by_query_signature<'a>(stream_url: &str, input: &'a ConfigInput) -> Option<&'a str> {
    if stream_url_account_query_matches(stream_url, &input.url) {
        return Some(&input.url);
    }

    input.aliases.as_ref().and_then(|aliases| {
        aliases
            .iter()
            .find(|alias| stream_url_account_query_matches(stream_url, &alias.url))
            .map(|alias| alias.url.as_str())
    })
}

fn stream_url_account_query_matches(stream_url: &str, config_url: &str) -> bool {
    let (Ok(stream_url), Ok(config_url)) = (Url::parse(stream_url), Url::parse(config_url)) else {
        return false;
    };

    config_url.query_pairs().any(|(config_key, config_value)| {
        is_account_query_key(&config_key)
            && stream_url.query_pairs().any(|(stream_key, stream_value)| {
                stream_key.eq_ignore_ascii_case(&config_key) && stream_value == config_value
            })
    })
}

fn rewrite_account_query_fields(stream_url: &str, source_config_url: &str, target_config_url: &str) -> Option<String> {
    let mut stream_url = Url::parse(stream_url).ok()?;
    let source_config_url = Url::parse(source_config_url).ok()?;
    let target_config_url = Url::parse(target_config_url).ok()?;
    let source_pairs: Vec<_> = source_config_url
        .query_pairs()
        .filter(|(key, _)| is_account_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let target_pairs: Vec<_> = target_config_url
        .query_pairs()
        .filter(|(key, _)| is_account_query_key(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let unambiguous_cross_key_target =
        if source_pairs.len() == 1 && target_pairs.len() == 1 { target_pairs.first() } else { None };
    let present_source_pairs: Vec<_> = source_pairs
        .iter()
        .filter(|(source_key, source_value)| {
            stream_url.query_pairs().any(|(stream_key, stream_value)| {
                stream_key.eq_ignore_ascii_case(source_key) && stream_value == source_value.as_str()
            })
        })
        .collect();
    let replacements: Vec<_> = present_source_pairs
        .iter()
        .filter_map(|(source_key, source_value)| {
            target_pairs
                .iter()
                .find(|(target_key, _)| target_key.eq_ignore_ascii_case(source_key))
                .or(unambiguous_cross_key_target)
                .map(|(target_key, target_value)| {
                    (source_key.clone(), source_value.clone(), target_key.clone(), target_value.clone())
                })
        })
        .collect();

    if replacements.len() != present_source_pairs.len() || replacements.is_empty() {
        return None;
    }

    let mut replaced = false;
    let stream_pairs: Vec<(String, String)> = stream_url
        .query_pairs()
        .map(|(key, value)| {
            if let Some((_, _, target_key, target_value)) =
                replacements.iter().find(|(source_key, source_value, _, _)| {
                    key.eq_ignore_ascii_case(source_key) && value == source_value.as_str()
                })
            {
                replaced = true;
                (target_key.clone(), target_value.clone())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect();

    if !replaced {
        return None;
    }
    stream_url
        .query_pairs_mut()
        .clear()
        .extend_pairs(stream_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())));
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
    let account_query_matches =
        provider_cfg.input_type.is_m3u() && stream_url_account_query_matches(stream_url, &provider_cfg.url);
    let Some(user_info) = provider_cfg.get_user_info() else {
        return account_query_matches;
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

pub(crate) async fn select_provider_stream_url(
    stream_url: &str,
    input: &ConfigInput,
    provider_cfg: &Arc<ProviderConfig>,
    accept_requested_stream_url: bool,
    app_config: &Arc<AppConfig>,
) -> Option<(Arc<str>, String)> {
    if accept_requested_stream_url {
        return Some((provider_cfg.name.clone(), stream_url.to_string()));
    }

    if provider_cfg.input_type.is_m3u() {
        match load_input_m3u_stream_url(app_config, &provider_cfg.name, stream_url).await {
            Ok(Some(provider_stream_url)) => {
                debug_if_enabled!(
                    "M3U alias URL lookup: provider={} requested_url={} index_match=true resolved_url={}",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref()),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, &provider_stream_url).as_ref())
                );
                return Some((provider_cfg.name.clone(), provider_stream_url.to_string()));
            }
            Ok(None) => {
                debug_if_enabled!(
                    "M3U alias URL lookup: provider={} requested_url={} index_match=false",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(resolve_request_url_for_logging(input, stream_url).as_ref())
                );
            }
            Err(err) => {
                debug_if_enabled!(
                    "Failed to resolve M3U stream URL for provider {}: {}",
                    sanitize_sensitive_info(&provider_cfg.name),
                    sanitize_sensitive_info(&err.to_string())
                );
            }
        }
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
                if let Some((selected_provider_name, url)) = select_provider_stream_url(
                    stream_url,
                    input,
                    provider_cfg,
                    accept_requested_stream_url,
                    &app_state.app_config,
                )
                .await
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
    let entry_lock = app_state.stalker_resolve_coordinator.guard_for(input.id, provider_id).await;
    let _flight = entry_lock.lock().await;
    let client = app_state.http_client.load().as_ref().clone();
    re_resolve_stalker_url(&app_state.app_config, &client, input, provider_id, kind, force_refresh).await
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
    cache_key: Option<&str>,
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
        if let Some(cache_key) = cache_key {
            debug!("Caching eligible resource stream {sanitized_resource_url}");
            let cache_resource_path = if let Some(cache) = app_state.cache.load().as_ref() {
                Some(cache.write().await.store_path(cache_key, mime_type.as_deref()))
            } else {
                None
            };
            if let Some(resource_path) = cache_resource_path {
                match create_new_file_for_write(&resource_path).await {
                    Ok(file) => {
                        debug!("Persisting resource stream {sanitized_resource_url} to {}", resource_path.display());
                        let writer = async_file_writer(file);
                        let add_cache_content = get_add_cache_content(cache_key, mime_type, &app_state.cache);
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
                debug!(
                    "Resource cache unavailable; streaming response for {sanitized_resource_url} without persistence"
                );
            }
        }
    }

    try_unwrap_body!(response_builder.body(axum::body::Body::from_stream(byte_stream)))
}

async fn fetch_resource_with_retry(
    app_state: &Arc<AppState>,
    http_client: &reqwest::Client,
    url: &Url,
    cache_key: Option<&str>,
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
                http_client,
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
        return Some(build_resource_stream_response(app_state, cache_key, resource_url, response).await);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceFetchPolicy {
    Standard,
    PublicNoRedirect,
}

impl ResourceFetchPolicy {
    const fn cache_key(self, resource_url: &str) -> Option<&str> {
        match self {
            Self::Standard => Some(resource_url),
            Self::PublicNoRedirect => None,
        }
    }

    const fn requires_public_destination(self) -> bool { matches!(self, Self::PublicNoRedirect) }
}

async fn validate_public_resource_destination(url: &Url) -> std::io::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsupported resource URL scheme"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "resource URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "resource URL has no port"))?;
    tuliprox_core::utils::network::request::resolve_public_socket_addrs(host, port).await?;
    Ok(())
}

/// # Panics
pub async fn resource_response(
    app_state: &Arc<AppState>,
    fetch_policy: ResourceFetchPolicy,
    resource_url: &str,
    req_headers: &HeaderMap,
    input: Option<&ConfigInput>,
) -> impl IntoResponse + Send {
    if resource_url.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let validated_url = if fetch_policy.requires_public_destination() {
        let Ok(url) = Url::parse(resource_url) else {
            error!("Url is malformed {}", sanitize_sensitive_info(resource_url));
            return StatusCode::BAD_REQUEST.into_response();
        };
        if let Err(err) = validate_public_resource_destination(&url).await {
            debug!("Rejected non-public resource destination {}: {err}", sanitize_sensitive_info(resource_url));
            return StatusCode::BAD_GATEWAY.into_response();
        }
        Some(url)
    } else {
        None
    };

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
    let cache_key = fetch_policy.cache_key(resource_url);
    if let (Some(cache_key), Some(cache)) = (cache_key, app_state.cache.load().as_ref()) {
        let cache_hit = {
            let mut guard = cache.write().await;
            guard.get_content(cache_key)
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
    if let Ok(url) = validated_url.map_or_else(|| Url::parse(resource_url), Ok) {
        let http_client = match fetch_policy {
            ResourceFetchPolicy::Standard => app_state.http_client.load(),
            ResourceFetchPolicy::PublicNoRedirect => app_state.public_http_client_no_redirect.load(),
        };
        if let Some(resp) =
            fetch_resource_with_retry(app_state, &http_client, &url, cache_key, resource_url, &req_headers, input).await
        {
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
    let config = app_state.app_config.config.load();
    let web_auth_config = config.web_ui.as_ref()?.auth.as_ref()?;
    // This hand-rolled its own `decode` with a bare `Validation::new`, which
    // checks `exp` and nothing else - no issuer.
    crate::auth::verify_token(token, web_auth_config.secret.as_bytes(), &web_auth_config.issuer)
        .map(|token_data| token_data.claims.username)
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
mod tests;
