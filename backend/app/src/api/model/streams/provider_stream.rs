use crate::{
    api::model::{
        stream::{BoxedProviderStream, ProviderStreamResponse},
        CustomVideoStream, ProvisioningStream, ThrottledStream, TimedClientStream, TransportStreamBuffer,
    },
    model::AppConfig,
};
use axum::response::IntoResponse;
use log::trace;
use reqwest::StatusCode;
use shared::model::{CustomVideoStreamType, PlaylistItemType, HEADER_TULIPROX_FALLBACK, HEADER_TULIPROX_REJECTION};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    try_unwrap_body,
    utils::{request_headers::HeaderFilter, response_compression::mark_response_as_uncompressed},
};
use tuliprox_session::stream_ctx::ProviderStreamCtx;

fn prepare_video_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .filter(|(key, _)| {
            !(key.eq_ignore_ascii_case("content-type")
                || key.eq_ignore_ascii_case("content-length")
                || key.eq_ignore_ascii_case("range")
                || key.eq_ignore_ascii_case("content-range")
                || key.eq_ignore_ascii_case("accept-ranges")
                || key.eq_ignore_ascii_case("transfer-encoding")
                || key.eq_ignore_ascii_case("cache-control")
                || key.eq_ignore_ascii_case(HEADER_TULIPROX_FALLBACK))
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
// Moved to `tuliprox_core::model`: the HLS proxy reads it too.
pub(crate) use tuliprox_core::model::is_custom_video_stream_enabled;

/// Returns the HTTP status code the custom-video factories should produce when
/// `custom_stream_response_enabled` is `false`. The configured value is validated
/// as a 4xx/5xx in `ConfigDto::prepare`, so the only fallbacks here are
/// defensive.
pub(crate) fn get_custom_stream_response_error_status(cfg: &AppConfig) -> StatusCode {
    let raw = cfg.config.load().custom_stream_response_error_status;
    StatusCode::from_u16(raw).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// The headers every fallback response carries: the reason, and no caching (a player or proxy must never
/// replay a fallback for a later request).
pub(crate) const fn fallback_headers(reason: CustomVideoStreamType) -> [(&'static str, &'static str); 2] {
    [("cache-control", "no-store"), (HEADER_TULIPROX_FALLBACK, reason.as_str())]
}

/// A fallback answered with a status and no body (no custom video configured or allowed).
pub(crate) fn fallback_status_response(status: StatusCode, reason: CustomVideoStreamType) -> axum::response::Response {
    fallback_response(status, reason, None, axum::body::Body::empty())
}

/// A fallback answered with an HLS manifest that plays the custom video.
pub(crate) fn fallback_manifest_response(
    manifest: impl Into<axum::body::Body>,
    reason: CustomVideoStreamType,
) -> axum::response::Response {
    fallback_response(StatusCode::OK, reason, Some("application/vnd.apple.mpegurl"), manifest.into())
}

fn fallback_response(
    status: StatusCode,
    reason: CustomVideoStreamType,
    content_type: Option<&'static str>,
    body: axum::body::Body,
) -> axum::response::Response {
    let mut builder = axum::response::Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    }
    for (key, value) in fallback_headers(reason) {
        builder = builder.header(key, value);
    }
    builder.body(body).unwrap_or_else(|_| status.into_response())
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
        let stream = apply_custom_stream_timeout(
            cfg,
            Box::pin(ThrottledStream::new(CustomVideoStream::new(video.clone()), 8000)),
        );
        let mut prepared = prepare_video_headers(headers);
        prepared.extend(fallback_headers(stream_type).into_iter().map(|(k, v)| (k.to_string(), v.to_string())));
        (Some(stream), Some((prepared, status, None, Some(stream_type))))
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
        pub fn $fn_name(cfg: &AppConfig, headers: &[(String, String)]) -> ProviderStreamResponse {
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

ok_custom_stream_factory!(
    create_maintenance_stream,
    maintenance,
    CustomVideoStreamType::Maintenance,
    "Streaming response input maintenance"
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
        let mut prepared = prepare_video_headers(headers);
        prepared.extend(
            fallback_headers(CustomVideoStreamType::Provisioning)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        (Some(stream), Some((prepared, StatusCode::OK, None, Some(CustomVideoStreamType::Provisioning))))
    } else {
        (None, None)
    }
}

pub fn create_custom_video_stream_response(
    ctx: &ProviderStreamCtx,
    _addr: &SocketAddr,
    video_response: CustomVideoStreamType,
) -> impl axum::response::IntoResponse + Send {
    let config = &ctx.app_config;
    if let (Some(stream), Some((headers, status_code, _, _))) = match video_response {
        CustomVideoStreamType::ChannelUnavailable => create_channel_unavailable_stream(config, &[], StatusCode::OK),
        CustomVideoStreamType::UserConnectionsExhausted => create_user_connections_exhausted_stream(config, &[]),
        CustomVideoStreamType::ProviderConnectionsExhausted => {
            create_provider_connections_exhausted_stream(config, &[])
        }
        CustomVideoStreamType::LowPriorityPreempted => create_low_priority_preempted_stream(config, &[]),
        CustomVideoStreamType::UserAccountExpired => create_user_account_expired_stream(config, &[]),
        CustomVideoStreamType::Provisioning => create_panel_api_provisioning_stream(config, &[]),
        CustomVideoStreamType::HlsSessionOrLeaseExpired => create_hls_session_or_lease_expired_stream(config, &[]),
        CustomVideoStreamType::Maintenance => create_maintenance_stream(config, &[]),
    } {
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
    let status = get_custom_stream_response_error_status(config);
    let mut builder = axum::response::Response::builder().status(status);
    if matches!(
        video_response,
        CustomVideoStreamType::UserConnectionsExhausted
            | CustomVideoStreamType::ProviderConnectionsExhausted
            | CustomVideoStreamType::LowPriorityPreempted
            | CustomVideoStreamType::UserAccountExpired
    ) {
        builder = builder.header(HEADER_TULIPROX_REJECTION, "admission_rejected");
    }
    for (key, value) in fallback_headers(video_response) {
        builder = builder.header(key, value);
    }
    builder.body(axum::body::Body::empty()).unwrap_or_else(|_| status.into_response())
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
        create_channel_unavailable_stream, get_custom_stream_response_error_status, is_custom_video_stream_enabled,
        prepare_video_headers, CustomVideoStreamType, ProviderStreamResponse, HEADER_TULIPROX_FALLBACK,
        HEADER_TULIPROX_REJECTION,
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
            config: Arc::new(ArcSwap::from_pointee(Config {
                custom_stream_response_enabled: true,
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            plugins: Arc::default(),
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
                failover_file_path: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
            failover: Arc::default(),
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
            maintenance: None,
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

    #[tokio::test]
    async fn custom_video_headers_drop_transfer_encoding() {
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let (_stream, info) = create_channel_unavailable_stream(
            &app_cfg,
            &[
                ("Transfer-Encoding".to_string(), "chunked".to_string()),
                ("Cache-Control".to_string(), "max-age=60".to_string()),
                ("X-Tuliprox-Fallback".to_string(), "x".to_string()),
            ],
            StatusCode::OK,
        );
        let (headers, _status, _url, _type) = info.expect("channel unavailable custom stream should exist");

        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("transfer-encoding")));
        let cache_control: Vec<_> = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("cache-control")).collect();
        assert_eq!(cache_control.len(), 1);
        assert_eq!(cache_control[0].1, "no-store");

        let fallback: Vec<_> =
            headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(HEADER_TULIPROX_FALLBACK)).collect();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].1, "channel_unavailable");
    }

    #[test]
    fn test_prepare_video_headers_drops_cache_control_and_fallback() {
        let headers = prepare_video_headers(&[
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Cache-Control".to_string(), "max-age=60".to_string()),
            ("X-Tuliprox-Fallback".to_string(), "x".to_string()),
        ]);

        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("transfer-encoding")));
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("cache-control")));
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case(HEADER_TULIPROX_FALLBACK)));
        assert!(headers.iter().any(|(name, value)| name == "content-type" && value == "video/mp2t"));
    }

    #[tokio::test]
    async fn test_all_custom_video_factories_set_fallback_and_cache_control_headers() {
        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        let buffer = TransportStreamBuffer::new(ts_packet);
        let app_cfg = create_test_app_config_with_channel_unavailable();
        app_cfg.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
            channel_unavailable: Some(buffer.clone()),
            user_connections_exhausted: Some(buffer.clone()),
            provider_connections_exhausted: Some(buffer.clone()),
            low_priority_preempted: Some(buffer.clone()),
            user_account_expired: Some(buffer.clone()),
            panel_api_provisioning: Some(buffer.clone()),
            hls_session_or_lease_expired: Some(buffer.clone()),
            maintenance: Some(buffer),
            panel_api_provisioning_hls_segments: Vec::new(),
        })));

        let cases: Vec<(ProviderStreamResponse, CustomVideoStreamType)> = vec![
            (
                create_channel_unavailable_stream(&app_cfg, &[], StatusCode::OK),
                CustomVideoStreamType::ChannelUnavailable,
            ),
            (
                super::create_user_connections_exhausted_stream(&app_cfg, &[]),
                CustomVideoStreamType::UserConnectionsExhausted,
            ),
            (
                super::create_provider_connections_exhausted_stream(&app_cfg, &[]),
                CustomVideoStreamType::ProviderConnectionsExhausted,
            ),
            (super::create_low_priority_preempted_stream(&app_cfg, &[]), CustomVideoStreamType::LowPriorityPreempted),
            (super::create_user_account_expired_stream(&app_cfg, &[]), CustomVideoStreamType::UserAccountExpired),
            (super::create_panel_api_provisioning_stream(&app_cfg, &[]), CustomVideoStreamType::Provisioning),
            (
                super::create_hls_session_or_lease_expired_stream(&app_cfg, &[]),
                CustomVideoStreamType::HlsSessionOrLeaseExpired,
            ),
            (super::create_maintenance_stream(&app_cfg, &[]), CustomVideoStreamType::Maintenance),
        ];

        for ((stream, info), expected_type) in cases {
            assert!(stream.is_some(), "stream must exist for {expected_type}");
            let (headers, _status, _url, stream_type) = info.expect("stream info must exist");
            assert_eq!(stream_type, Some(expected_type));

            let fallback_headers: Vec<_> =
                headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(HEADER_TULIPROX_FALLBACK)).collect();
            assert_eq!(fallback_headers.len(), 1, "exactly one fallback header for {expected_type}");
            assert_eq!(fallback_headers[0].1, expected_type.to_string());

            let cache_headers: Vec<_> =
                headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("cache-control")).collect();
            assert_eq!(cache_headers.len(), 1, "exactly one cache-control header for {expected_type}");
            assert_eq!(cache_headers[0].1, "no-store");
        }
    }

    #[tokio::test]
    async fn test_path_without_video_sets_status_and_both_headers() {
        use super::create_custom_video_stream_response;
        use axum::response::IntoResponse;
        use std::net::SocketAddr;

        let cfg = Config {
            custom_stream_response_enabled: false,
            custom_stream_response_error_status: 503,
            ..Config::default()
        };
        let app_state = crate::api::model::create_test_app_state(cfg);
        let ctx = app_state.provider_stream_ctx();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        let all_types = [
            CustomVideoStreamType::ChannelUnavailable,
            CustomVideoStreamType::UserConnectionsExhausted,
            CustomVideoStreamType::ProviderConnectionsExhausted,
            CustomVideoStreamType::LowPriorityPreempted,
            CustomVideoStreamType::UserAccountExpired,
            CustomVideoStreamType::Provisioning,
            CustomVideoStreamType::HlsSessionOrLeaseExpired,
            CustomVideoStreamType::Maintenance,
        ];

        for stream_type in all_types {
            let response = create_custom_video_stream_response(&ctx, &addr, stream_type).into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

            let fallback = response.headers().get(HEADER_TULIPROX_FALLBACK).expect("fallback header");
            assert_eq!(fallback.to_str().unwrap(), stream_type.to_string());

            let cache_control = response.headers().get("cache-control").expect("cache-control header");
            assert_eq!(cache_control.to_str().unwrap(), "no-store");

            let is_rejection_type = matches!(
                stream_type,
                CustomVideoStreamType::UserConnectionsExhausted
                    | CustomVideoStreamType::ProviderConnectionsExhausted
                    | CustomVideoStreamType::LowPriorityPreempted
                    | CustomVideoStreamType::UserAccountExpired
            );

            if is_rejection_type {
                let rejection = response.headers().get(HEADER_TULIPROX_REJECTION).expect("rejection header");
                assert_eq!(rejection.to_str().unwrap(), "admission_rejected");
            } else {
                assert!(
                    response.headers().get(HEADER_TULIPROX_REJECTION).is_none(),
                    "no rejection header expected for {stream_type}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_channel_unavailable_preserves_supplied_status_code() {
        let app_cfg = create_test_app_config_with_channel_unavailable();

        let (_stream, info) = create_channel_unavailable_stream(&app_cfg, &[], StatusCode::SERVICE_UNAVAILABLE);
        let (_headers, status, _url, stream_type) = info.expect("channel unavailable custom stream should exist");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(matches!(stream_type, Some(CustomVideoStreamType::ChannelUnavailable)));
    }

    #[tokio::test]
    async fn maintenance_factory_serves_its_own_clip() {
        use super::create_maintenance_stream;
        use crate::api::model::TransportStreamBuffer;

        let mut ts_packet = vec![0_u8; 188];
        ts_packet[0] = 0x47;
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let mut current = (*app_cfg.custom_stream_response.load_full().expect("custom response set")).clone();
        current.maintenance = Some(TransportStreamBuffer::new(ts_packet));
        app_cfg.custom_stream_response.store(Some(Arc::new(current)));

        let (_stream, info) = create_maintenance_stream(&app_cfg, &[]);
        let (_headers, status, _url, stream_type) = info.expect("input maintenance clip should exist");
        assert_eq!(status, StatusCode::OK);
        assert!(matches!(stream_type, Some(CustomVideoStreamType::Maintenance)));
    }

    #[tokio::test]
    async fn maintenance_without_clip_has_no_stream() {
        let app_cfg = create_test_app_config_with_channel_unavailable();
        let (stream, info) = super::create_maintenance_stream(&app_cfg, &[]);
        assert!(stream.is_none() && info.is_none());
    }

    /// The `ok_custom_stream_factory!` macro generates a public factory for
    /// each (field, stream type, description) tuple. Pick one representative
    /// factory — `create_user_connections_exhausted_stream` — and verify the
    /// macro forwards the correct `CustomVideoStreamType` to the stream info.
    #[tokio::test]
    async fn test_ok_custom_stream_factory_macro_forwards_video_type() {
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
    #[tokio::test]
    async fn test_displaying_custom_video_streams_serves_video_with_supplied_status() {
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
