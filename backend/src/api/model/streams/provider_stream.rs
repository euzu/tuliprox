use crate::{
    api::{
        api_utils::{mark_response_as_uncompressed, try_unwrap_body, HeaderFilter},
        model::{
            stream::{BoxedProviderStream, ProviderStreamResponse},
            AppState, CleanupEvent, CustomVideoStream, ProvisioningStream, ThrottledStream, TimedClientStream,
            TransportStreamBuffer,
        },
    },
    model::AppConfig,
};
use axum::response::IntoResponse;
use log::trace;
use reqwest::StatusCode;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use shared::error::TuliproxError;
use shared::model::PlaylistItemType;
use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
pub enum CustomVideoStreamType {
    ChannelUnavailable,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    LowPriorityPreempted,
    UserAccountExpired,
    Provisioning,
    HlsSessionOrLeaseExpired,
}

impl fmt::Display for CustomVideoStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CustomVideoStreamType::ChannelUnavailable => "channel_unavailable",
            CustomVideoStreamType::UserConnectionsExhausted => "user_connections_exhausted",
            CustomVideoStreamType::ProviderConnectionsExhausted => "provider_connections_exhausted",
            CustomVideoStreamType::LowPriorityPreempted => "low_priority_preempted",
            CustomVideoStreamType::UserAccountExpired => "user_account_expired",
            CustomVideoStreamType::Provisioning => "provisioning",
            CustomVideoStreamType::HlsSessionOrLeaseExpired => "hls_session_or_lease_expired",
        };
        write!(f, "{s}")
    }
}

impl FromStr for CustomVideoStreamType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "channel_unavailable" => Ok(Self::ChannelUnavailable),
            "user_connections_exhausted" => Ok(Self::UserConnectionsExhausted),
            "provider_connections_exhausted" => Ok(Self::ProviderConnectionsExhausted),
            "low_priority_preempted" => Ok(Self::LowPriorityPreempted),
            "user_account_expired" => Ok(Self::UserAccountExpired),
            "provisioning" => Ok(Self::Provisioning),
            "hls_session_or_lease_expired" => Ok(Self::HlsSessionOrLeaseExpired),
            _ => Err(TuliproxError::Config(format!("Unknown stream type: {s}"))),
        }
    }
}

impl Serialize for CustomVideoStreamType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for CustomVideoStreamType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

fn prepare_video_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .filter(|(key, _)| {
            !(key.eq_ignore_ascii_case("content-type")
                || key.eq_ignore_ascii_case("content-length")
                || key.eq_ignore_ascii_case("range")
                || key.eq_ignore_ascii_case("content-range")
                || key.eq_ignore_ascii_case("accept-ranges"))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    h.push(("content-type".to_string(), "video/mp2t".to_string()));
    h
}

fn get_custom_stream_response_timeout_secs(cfg: &AppConfig) -> u32 {
    cfg.config.load().custom_stream_response_timeout_secs
}

fn apply_custom_stream_timeout(cfg: &AppConfig, stream: BoxedProviderStream) -> BoxedProviderStream {
    let timeout_secs = get_custom_stream_response_timeout_secs(cfg);
    if timeout_secs == 0 {
        stream
    } else {
        Box::pin(TimedClientStream::new_without_kick(stream, timeout_secs))
    }
}

/// Returns the value of `custom_stream_response_enabled` from the main config.
/// When `false`, the custom-video factories (`channel_unavailable`,
/// `user_connections_exhausted`, `provider_connections_exhausted`,
/// `low_priority_preempted`, `user_account_expired`, `panel_api_provisioning`,
/// `hls_session_or_lease_expired`) skip the configured MPEG-TS video and the call sites return
/// `custom_stream_response_error_status` instead. This allows a downstream Nginx
/// with `proxy_intercept_errors on;` to sever the socket instead of seeing an
/// infinite 200 OK loop.
pub(crate) fn is_custom_video_stream_enabled(cfg: &AppConfig) -> bool {
    cfg.config.load().custom_stream_response_enabled
}

/// Returns the HTTP status code the custom-video factories should produce when
/// `custom_stream_response_enabled` is `false`. The configured value is validated
/// as a 4xx/5xx in `ConfigDto::prepare`, so the only fallbacks here are
/// defensive.
pub(crate) fn get_custom_stream_response_error_status(cfg: &AppConfig) -> StatusCode {
    let raw = cfg.config.load().custom_stream_response_error_status;
    StatusCode::from_u16(raw).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn create_video_stream(
    cfg: &AppConfig,
    stream_type: CustomVideoStreamType,
    video_buffer: Option<&TransportStreamBuffer>,
    headers: &[(String, String)],
    status: StatusCode,
    log_message: &str,
) -> ProviderStreamResponse {
    // Honour `custom_stream_response_enabled: false` even when the operator left a
    // custom video configured. The factory returns the same `(None, None)` shape it
    // already uses when no video is configured, so existing call sites that fall
    // through to an error status on `(None, None)` keep working unchanged.
    if !is_custom_video_stream_enabled(cfg) {
        return (None, None);
    }
    if let Some(video) = video_buffer {
        trace!("{log_message}");
        let stream =
            apply_custom_stream_timeout(cfg, Box::pin(ThrottledStream::new(CustomVideoStream::new(video.clone()), 8000)));
        (
            Some(stream),
            Some((prepare_video_headers(headers), status, None, Some(stream_type))),
        )
    } else {
        (None, None)
    }
}

fn create_ok_video_stream(
    cfg: &AppConfig,
    stream_type: CustomVideoStreamType,
    video_buffer: Option<&TransportStreamBuffer>,
    headers: &[(String, String)],
    log_message: &str,
) -> ProviderStreamResponse {
    create_video_stream(cfg, stream_type, video_buffer, headers, StatusCode::OK, log_message)
}

pub fn create_channel_unavailable_stream(
    cfg: &AppConfig,
    headers: &[(String, String)],
    status: StatusCode,
) -> ProviderStreamResponse {
    let custom_stream_response = cfg.custom_stream_response.load();
    let video = custom_stream_response.as_ref().and_then(|c| c.channel_unavailable.as_ref());
    create_video_stream(
        cfg,
        CustomVideoStreamType::ChannelUnavailable,
        video,
        headers,
        status,
        &format!("Streaming response channel unavailable for status {status}"),
    )
}

/// Generate a public factory function that loads the optional custom
/// fallback video for a given `CustomStreamResponse` field and forwards it to
/// `create_ok_video_stream` with the matching `CustomVideoStreamType` variant
/// and human-readable description.
macro_rules! ok_custom_stream_factory {
    ($fn_name:ident, $field:ident, $stream_type:expr, $description:expr) => {
        pub fn $fn_name(
            cfg: &AppConfig,
            headers: &[(String, String)],
        ) -> ProviderStreamResponse {
            let custom_stream_response = cfg.custom_stream_response.load();
            let video = custom_stream_response.as_ref().and_then(|c| c.$field.as_ref());
            create_ok_video_stream(cfg, $stream_type, video, headers, $description)
        }
    };
}

ok_custom_stream_factory!(
    create_user_connections_exhausted_stream,
    user_connections_exhausted,
    CustomVideoStreamType::UserConnectionsExhausted,
    "Streaming response user connections exhausted"
);

ok_custom_stream_factory!(
    create_provider_connections_exhausted_stream,
    provider_connections_exhausted,
    CustomVideoStreamType::ProviderConnectionsExhausted,
    "Streaming response provider connections exhausted"
);

ok_custom_stream_factory!(
    create_low_priority_preempted_stream,
    low_priority_preempted,
    CustomVideoStreamType::LowPriorityPreempted,
    "Streaming response low-priority preempted"
);

ok_custom_stream_factory!(
    create_user_account_expired_stream,
    user_account_expired,
    CustomVideoStreamType::UserAccountExpired,
    "Streaming response user account expired"
);

ok_custom_stream_factory!(
    create_panel_api_provisioning_stream,
    panel_api_provisioning,
    CustomVideoStreamType::Provisioning,
    "Streaming response panel api provisioning"
);

ok_custom_stream_factory!(
    create_hls_session_or_lease_expired_stream,
    hls_session_or_lease_expired,
    CustomVideoStreamType::HlsSessionOrLeaseExpired,
    "Streaming response hls session or lease expired"
);

pub fn create_panel_api_provisioning_stream_with_stop(
    cfg: &AppConfig,
    headers: &[(String, String)],
    stop_signal: CancellationToken,
) -> ProviderStreamResponse {
    if !is_custom_video_stream_enabled(cfg) {
        return (None, None);
    }
    let custom_stream_response = cfg.custom_stream_response.load();
    let video = custom_stream_response.as_ref().and_then(|c| c.panel_api_provisioning.as_ref());
    if let Some(video) = video {
        trace!("Streaming response panel api provisioning");
        let stream = ProvisioningStream::new(video.clone(), stop_signal);
        let stream = apply_custom_stream_timeout(cfg, Box::pin(ThrottledStream::new(stream, 8000)));
        (
            Some(stream),
            Some((prepare_video_headers(headers), StatusCode::OK, None, Some(CustomVideoStreamType::Provisioning))),
        )
    } else {
        (None, None)
    }
}

pub fn create_custom_video_stream_response(
    app_state: &Arc<AppState>,
    addr: &SocketAddr,
    video_response: CustomVideoStreamType,
) -> impl axum::response::IntoResponse + Send {
    let config = &app_state.app_config;
    if let (Some(stream), Some((headers, status_code, _, _))) = match video_response {
        CustomVideoStreamType::ChannelUnavailable => {
            create_channel_unavailable_stream(config, &[], StatusCode::OK)
        }
        CustomVideoStreamType::UserConnectionsExhausted => create_user_connections_exhausted_stream(config, &[]),
        CustomVideoStreamType::ProviderConnectionsExhausted => {
            create_provider_connections_exhausted_stream(config, &[])
        }
        CustomVideoStreamType::LowPriorityPreempted => create_low_priority_preempted_stream(config, &[]),
        CustomVideoStreamType::UserAccountExpired => create_user_account_expired_stream(config, &[]),
        CustomVideoStreamType::Provisioning => create_panel_api_provisioning_stream(config, &[]),
        CustomVideoStreamType::HlsSessionOrLeaseExpired => create_hls_session_or_lease_expired_stream(config, &[]),
    } {
        app_state.connection_manager.send_cleanup(CleanupEvent::UpdateDetailAndReleaseProviderConnection {
            addr: *addr,
            video_type: video_response,
        });
        let mut builder = axum::response::Response::builder().status(status_code);
        for (key, value) in headers {
            builder = builder.header(key, value);
        }
        let mut response = try_unwrap_body!(builder.body(axum::body::Body::from_stream(stream)));
        mark_response_as_uncompressed(&mut response);
        return response;
    }
    // No custom video is configured, or the operator set
    // `custom_stream_response_enabled: false`. In both cases we surface the
    // configured `custom_stream_response_error_status` (default 502) instead of a
    // hard-coded 403, so a reverse proxy with `proxy_intercept_errors on;` can sever
    // the socket.
    get_custom_stream_response_error_status(config).into_response()
}
pub fn get_header_filter_for_item_type(item_type: PlaylistItemType) -> HeaderFilter {
    match item_type {
        PlaylistItemType::Live /*| PlaylistItemType::LiveHls | PlaylistItemType::LiveDash */| PlaylistItemType::LiveUnknown => {
            Some(Box::new(|key| key != "accept-ranges" && key != "range" && key != "content-range"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        create_channel_unavailable_stream, is_custom_video_stream_enabled, get_custom_stream_response_error_status,
        CustomVideoStreamType,
    };
    use crate::{
        api::model::TransportStreamBuffer,
        model::{AppConfig, Config, ConfigInput, CustomStreamResponse, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use reqwest::StatusCode;
    use shared::{
        model::{ConfigPaths, InputFetchMethod, InputType},
        utils::Internable,
    };
    use std::{collections::HashMap, str::FromStr, sync::Arc};

    fn create_test_app_config_with_channel_unavailable() -> AppConfig {
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

        let app_cfg = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config { custom_stream_response_enabled: true, ..Config::default()})),
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
        };

        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        app_cfg.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: Some(TransportStreamBuffer::new(ts_packet)),
            user_connections_exhausted: None,
            provider_connections_exhausted: None,
            low_priority_preempted: None,
            user_account_expired: None,
            panel_api_provisioning: None,
            hls_session_or_lease_expired: None,
            panel_api_provisioning_hls_segments: Vec::new(),
        })));
        app_cfg
    }

    #[test]
    fn test_low_priority_preempted_custom_video_type_roundtrip() {
        let parsed = CustomVideoStreamType::from_str("low_priority_preempted")
            .expect("low_priority_preempted should parse as custom video type");
        assert_eq!(parsed.to_string(), "low_priority_preempted");
    }

    #[test]
    fn test_hls_session_or_lease_expired_custom_video_type_roundtrip() {
        let parsed = CustomVideoStreamType::from_str("hls_session_or_lease_expired")
            .expect("hls_session_or_lease_expired should parse as custom video type");
        assert_eq!(parsed.to_string(), "hls_session_or_lease_expired");
    }

    #[test]
    fn test_channel_unavailable_preserves_supplied_status_code() {
        let app_cfg = create_test_app_config_with_channel_unavailable();

        let (_stream, info) = create_channel_unavailable_stream(&app_cfg, &[], StatusCode::SERVICE_UNAVAILABLE);
        let (_headers, status, _url, stream_type) = info.expect("channel unavailable custom stream should exist");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(stream_type, Some(CustomVideoStreamType::ChannelUnavailable)));
    }

    /// The `ok_custom_stream_factory!` macro generates a public factory for
    /// each (field, stream type, description) tuple. Pick one representative
    /// factory — `create_user_connections_exhausted_stream` — and verify the
    /// macro forwards the correct `CustomVideoStreamType` to the stream info.
    #[test]
    fn test_ok_custom_stream_factory_macro_forwards_video_type() {
        use super::create_user_connections_exhausted_stream;
        use crate::api::model::TransportStreamBuffer;

        // Build a config with all 5 ok-stream custom video fields populated so
        // the macro-generated factory can actually return a stream.
        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        let buffer = TransportStreamBuffer::new(ts_packet);
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let current_arc = app_cfg.custom_stream_response.load().clone().expect("custom response set");
        let mut current = (*current_arc).clone();
        current.user_connections_exhausted = Some(buffer.clone());
        current.provider_connections_exhausted = Some(buffer.clone());
        current.low_priority_preempted = Some(buffer.clone());
        current.user_account_expired = Some(buffer.clone());
        current.panel_api_provisioning = Some(buffer);
        app_cfg.custom_stream_response.store(Some(Arc::new(current)));

        let (_stream, info) = create_user_connections_exhausted_stream(&app_cfg, &[]);
        let (_headers, _status, _url, stream_type) =
            info.expect("user connections exhausted custom stream should exist");

        assert!(
            matches!(stream_type, Some(CustomVideoStreamType::UserConnectionsExhausted)),
            "macro-generated factory must tag the stream with the matching CustomVideoStreamType"
        );
    }

    /// Build a config with `custom_stream_response_enabled: false` and a configured
    /// `channel_unavailable` buffer. Used by the hide-flag tests.
    fn create_test_app_config_with_hiding_custom_video_streams() -> AppConfig {
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let cfg = Config {
            custom_stream_response_enabled: false,
            custom_stream_response_error_status: 503,
            ..Config::default()
        };
        app_cfg.config.store(Arc::new(cfg));
        app_cfg
    }

    /// `custom_stream_response_enabled` should report the flag set in the config.
    #[test]
    fn test_display_custom_video_streams_reflects_config() {
        let hidden = create_test_app_config_with_hiding_custom_video_streams();
        assert!(!is_custom_video_stream_enabled(&hidden));

        let visible = create_test_app_config_with_channel_unavailable();
        assert!(is_custom_video_stream_enabled(&visible));
    }

    /// `get_custom_stream_response_error_status` should return the configured 4xx/5xx code.
    #[test]
    fn test_get_custom_stream_response_error_status_uses_configured_value() {
        let cfg = create_test_app_config_with_hiding_custom_video_streams();
        assert_eq!(get_custom_stream_response_error_status(&cfg), StatusCode::SERVICE_UNAVAILABLE);

        // Default (no main-config override) falls back to 502 Bad Gateway.
        let default_cfg = create_test_app_config_with_channel_unavailable();
        assert_eq!(get_custom_stream_response_error_status(&default_cfg), StatusCode::BAD_GATEWAY);
    }

    /// With `custom_stream_response_enabled: false`, the factory must return `(None, None)`
    /// so the response builder falls through to the configured error status instead
    /// of serving the configured infinite MPEG-TS video with 200 OK.
    #[test]
    fn test_hiding_custom_video_streams_returns_none_tuple() {
        let app_cfg = create_test_app_config_with_hiding_custom_video_streams();
        let (stream, info) = create_channel_unavailable_stream(&app_cfg, &[], StatusCode::SERVICE_UNAVAILABLE);
        assert!(stream.is_none(), "stream must be None when custom_stream_response_enabled is false");
        assert!(info.is_none(), "info must be None when custom_stream_response_enabled is false");
    }

    /// The centralisation in `create_video_stream` must apply to the macro-generated
    /// factories as well (`user_connections_exhausted`, `provider_connections_exhausted`,
    /// `low_priority_preempted`, `user_account_expired`, `panel_api_provisioning`,
    /// `hls_session_or_lease_expired`).
    #[test]
    fn test_hiding_custom_video_streams_applies_to_macro_factory() {
        use super::create_user_connections_exhausted_stream;
        use crate::api::model::TransportStreamBuffer;

        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        let buffer = TransportStreamBuffer::new(ts_packet);
        let app_cfg = create_test_app_config_with_hiding_custom_video_streams();
        let current_arc = app_cfg.custom_stream_response.load().clone().expect("custom response set");
        let mut current = (*current_arc).clone();
        current.user_connections_exhausted = Some(buffer);
        app_cfg.custom_stream_response.store(Some(Arc::new(current)));

        let (stream, info) = create_user_connections_exhausted_stream(&app_cfg, &[]);
        assert!(stream.is_none(), "macro factory stream must be None when custom_stream_response_enabled is false");
        assert!(info.is_none(), "macro factory info must be None when custom_stream_response_enabled is false");
    }

    /// Regression guard: when `custom_stream_response_enabled` is true (the default), the
    /// factory must still return the configured video with the supplied status code.
    #[test]
    fn test_displaying_custom_video_streams_serves_video_with_supplied_status() {
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let (stream, info) = create_channel_unavailable_stream(&app_cfg, &[], StatusCode::SERVICE_UNAVAILABLE);
        assert!(stream.is_some(), "stream must be produced when custom_stream_response_enabled is true");
        let (headers, status, _url, stream_type) = info.expect("info must be present");
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(stream_type, Some(CustomVideoStreamType::ChannelUnavailable)));
        assert!(
            headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")),
            "prepared headers must include content-type"
        );
    }
}
