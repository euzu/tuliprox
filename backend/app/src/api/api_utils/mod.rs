pub use crate::repository::{
    evaluate_network_access, log_network_access_allowed_geoip_unavailable, log_network_access_denied,
    NetworkAccessDecision, NetworkAccessDenyReason,
};
use crate::{
    api::{
        endpoints::xtream_api::ApiStreamContext,
        model::{AppState, PlaybackLeaseRef},
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
};
use axum::http::HeaderMap;
use shared::model::{PlaylistEntry, StreamChannel, TargetType, XtreamCluster};
use std::{
    net::SocketAddr,
    sync::{Arc, LazyLock},
    time::Duration,
};

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

pub use internal_server_error;
use shared::model::{ConnectFailureReason, FailureStage};
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
    stream_options::{get_stream_options, StreamOptions, StreamResponseMode},
};

static PROCESS_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

// Response-compression opt-out moved to `tuliprox_core::utils`; re-exported so
// api call sites keep their names.
pub(crate) use tuliprox_core::utils::response_compression::{
    mark_response_as_uncompressed, should_compress_response_extensions,
};

pub struct ForceStreamRequestContext<'a> {
    pub req_headers: &'a HeaderMap,
    pub input: &'a Arc<ConfigInput>,
    pub user: &'a ProxyUserCredentials,
    pub session_reservation_ttl_secs: u64,
    pub(crate) content_representation: crate::api::model::ProviderContentRepresentationMode,
}

/// Upper bound a manifest refresh waits for its pinned account to free a slot. Parallel
/// playbacks on one account free their slots within seconds; a 503 would stall the player.
pub(crate) const HLS_MANIFEST_CAPACITY_WAIT: Duration = Duration::from_secs(5);

/// Upper bound a media object (segment, init, key) waits. It stays well below a typical
/// segment duration, so the player keeps buffer to retry after a 503.
const HLS_MEDIA_CAPACITY_WAIT: Duration = Duration::from_secs(2);

/// Lower bound between two acquisition attempts that only wait for a lease to expire.
const LEASE_EXPIRY_RECHECK_FLOOR: Duration = Duration::from_millis(10);

/// Acquisition of a slot on exactly one provider account for a playback lease.
pub(crate) struct ExactProviderAcquire<'a> {
    pub provider: &'a Arc<str>,
    pub addr: &'a SocketAddr,
    pub allow_grace: bool,
    pub priority: i8,
    pub kind: crate::api::model::ConnectionKind,
    pub lease: Option<PlaybackLeaseRef<'a>>,
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

/// Upper bound of redirect hops a resource route follows. Every hop is classified, so the number of
/// requests a provider or EPG entry can trigger through one resource link stays bounded.
const RESOURCE_REDIRECT_LIMIT: u8 = 5;

/// How the client-visible answer of a resource route is shaped.
///
/// The variant mirrors the classification of the destination the route serves: a [`Public`] destination
/// may be handed to the client as it was fetched, a [`NonPublic`] one is relayed with a sanitized
/// answer. Which client performs a hop is decided per hop, not by this policy.
///
/// [`Public`]: ResourceFetchPolicy::Public
/// [`NonPublic`]: ResourceFetchPolicy::NonPublic
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceFetchPolicy {
    Public,
    NonPublic,
}

const API_STREAM_CHUNK_SIZE: usize = 64 * 1024;

#[cfg(test)]
mod tests;

mod activation;
mod auth;
mod catchup;
mod clock;
mod local_files;
mod media_server;
mod metering;
mod provider_acquire;
mod provider_urls;
mod resources;
mod response;
mod serialization;
mod session_identity;
mod stalker;
mod strategy;
mod streaming;

#[cfg(test)]
use self::metering::resolve_stream_config_u64;
#[cfg(test)]
use self::provider_urls::find_input_account_by_signature;
#[cfg(test)]
use self::provider_urls::stream_url_matches_provider;
#[cfg(test)]
use self::response::admission_failure_video_type;
#[cfg(test)]
use self::stalker::needs_initial_stalker_resolution;
#[cfg(test)]
use self::strategy::resolve_streaming_strategy;
use self::{
    activation::{
        activate_session_before_stream_open, cleanup_forced_reopen_addrs, CurrentSessionGuard, SessionActivationRequest,
    },
    catchup::{
        cleanup_failed_detected_catchup_hls, detected_catchup_hls_response, probe_catchup_payload, CatchupPayload,
        DetectedCatchupHlsResponseParams,
    },
    media_server::{
        is_media_server_playback_url, is_media_server_stream_ref_url, media_server_image_error_status,
        open_media_server_image_resource, open_media_server_stream_for_input,
    },
    metering::{
        get_stream_config_u64, get_stream_throttle, is_stream_metrics_enabled, is_throttled_stream,
        prepare_stream_metering, StreamMeteringConfig,
    },
    provider_acquire::{
        allows_provider_pool_failover, resolve_streaming_strategy_with_provider_handle, split_provider_stream_open,
    },
    provider_urls::{get_redirect_alternative_url, resolve_xtream_vod_provider_url},
    resources::hls_resource_failure_status,
    response::stream_admission_rejected_response,
    session_identity::session_reacquire_cleanup_addrs,
    stalker::{re_resolve_stalker_url_singleflight, should_refresh_stalker_playback, stalker_stream_kind},
    strategy::{get_grace_period_millis, should_defer_provider_open_for_grace_hold},
    streaming::{
        create_stream_response_details, force_stream_response, is_hop_by_hop_response_header,
        no_custom_video_fallback_status, prepare_body_stream, try_shared_stream_response_if_any,
        StreamingAcquireOptions,
    },
};
pub use self::{
    auth::{
        create_api_proxy_user, create_recording_proxy_user, get_user_target, get_user_target_by_credentials,
        get_user_target_by_username, get_username_from_auth_header,
    },
    catchup::{create_catchup_session_key, create_m3u_catchup_session_key},
    clock::{get_build_time, get_server_time, get_uptime_secs, init_uptime_clock},
    local_files::serve_file,
    provider_urls::get_stream_alternative_url,
    resources::{resource_input_for_url, resource_proxy_response, resource_redirect_or_proxy, resource_response},
    response::{
        bin_response, empty_json_list_response, empty_json_response_as_array, empty_json_response_as_object,
        json_or_bin_response, json_response, redirect, reentry_suppressed_response, separate_number_and_remainder,
    },
    serialization::{
        stream_bin_array, stream_bin_array_stream, stream_json_array, stream_json_array_stream,
        stream_json_or_bin_response, stream_json_or_bin_response_stream, stream_json_or_bin_response_try_stream,
    },
    session_identity::create_session_fingerprint,
    strategy::redirect_response,
    streaming::{
        force_provider_stream_response, is_hls_stream_share_enabled, is_seek_request, is_seekable_media_request,
        is_stream_share_enabled,
    },
};
pub(crate) use self::{
    auth::{input_for_user, with_recording_headers},
    catchup::get_catchup_session_ttl_secs,
    local_files::local_stream_response,
    provider_acquire::{acquire_exact_provider_handle, stream_response_with_provider_handle},
    provider_urls::select_provider_stream_url,
    resources::force_hls_resource_response,
    response::admission_failure_response,
    serialization::coalesce_byte_stream,
    session_identity::{
        create_playback_session_fingerprint, get_hls_session_ttl_secs, get_session_reservation_ttl_secs,
        is_session_based_playback, is_socket_bound_playback_session, should_pin_provider_for_session,
    },
    stalker::resolve_initial_stalker_playback_url,
    strategy::resolve_redirect_location,
    streaming::{
        get_hls_playback_ttl_secs, record_connect_failed_attempt, resolve_request_url_for_logging,
        resolve_stream_user_agent_index, should_allow_exhausted_shared_reconnect, stream_response,
    },
};
