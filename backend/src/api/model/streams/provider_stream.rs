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
use shared::model::PlaylistItemType;
use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc};
use tokio_util::sync::CancellationToken;
use shared::error::TuliproxError;

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
pub enum CustomVideoStreamType {
    ChannelUnavailable,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    LowPriorityPreempted,
    UserAccountExpired,
    Provisioning,
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

fn create_video_stream(
    cfg: &AppConfig,
    stream_type: CustomVideoStreamType,
    video_buffer: Option<&TransportStreamBuffer>,
    headers: &[(String, String)],
    status: StatusCode,
    log_message: &str,
) -> ProviderStreamResponse {
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

pub fn create_panel_api_provisioning_stream_with_stop(
    cfg: &AppConfig,
    headers: &[(String, String)],
    stop_signal: CancellationToken,
) -> ProviderStreamResponse {
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
            create_channel_unavailable_stream(config, &[], StatusCode::BAD_REQUEST)
        }
        CustomVideoStreamType::UserConnectionsExhausted => create_user_connections_exhausted_stream(config, &[]),
        CustomVideoStreamType::ProviderConnectionsExhausted => {
            create_provider_connections_exhausted_stream(config, &[])
        }
        CustomVideoStreamType::LowPriorityPreempted => create_low_priority_preempted_stream(config, &[]),
        CustomVideoStreamType::UserAccountExpired => create_user_account_expired_stream(config, &[]),
        CustomVideoStreamType::Provisioning => create_panel_api_provisioning_stream(config, &[]),
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
    axum::http::StatusCode::FORBIDDEN.into_response()
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
    use super::{create_channel_unavailable_stream, CustomVideoStreamType};
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
}
