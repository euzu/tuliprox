use crate::{
    api::{
        api_utils::{create_api_proxy_user, mark_response_as_uncompressed},
        endpoints::hls_api::{
            build_virtual_hls_entry_path, hls_panel_provisioning_poll_manifest_response,
            hls_shared_panel_provisioning_poll_manifest_response, resolve_hls_virtual_input_for_target,
        },
        model::{
            create_custom_video_stream_response, hls_custom_video_manifest_response_with_virtual_id,
            parse_hls_panel_provisioning_segment_route_name, AppState, CustomVideoStreamType,
            TransportStreamBuffer,
        },
    },
    auth::{check_network_access_only, resolve_api_user_context, verify_access_token, Fingerprint},
    model::{ConfigTarget, ProxyUserCredentials},
};
use axum::{
    body::Body,
    http::{
        header::{self, HeaderName},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use std::{str::FromStr, sync::Arc};
use url::form_urlencoded;

const HLS_CVS_CONTENT_TYPE: &str = "video/mp2t";
const HLS_CVS_MEDIA_EXTENSIONS: &[&str] = &["ts", "mp4", "m4s", "m4v"];
const ACCEPT_RANGES_VALUE: &str = "bytes";
const HLS_CVS_CACHE_CONTROL: &str = "no-store";

#[derive(Debug, Deserialize)]
struct ProvisioningManifestQuery {
    id: u32,
}

type CvsUserContext = (Arc<ProxyUserCredentials>, Arc<ConfigTarget>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CvsRouteKind {
    Hls,
    Ts,
}

#[derive(Clone, Copy)]
struct CvsApiResponseContext<'a> {
    fingerprint: &'a Fingerprint,
    username: &'a str,
    password: &'a str,
    stream_type: &'a str,
    route_kind: CvsRouteKind,
    request_headers: &'a HeaderMap,
    raw_query: Option<&'a str>,
    app_state: &'a Arc<AppState>,
}

fn resolve_cvs_user_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    username: &str,
    password: &str,
) -> Result<CvsUserContext, Box<Response>> {
    let Some((user, target)) = app_state.app_config.get_target_for_user(username, password) else {
        return Err(Box::new(app_state.app_config.get_auth_error_status().into_response()));
    };

    if let Err(e) = resolve_api_user_context(user.clone(), target.clone(), fingerprint.clone(), app_state) {
        return Err(Box::new(e.into_player_response(app_state.app_config.get_auth_error_status())));
    }

    Ok((user, target))
}

fn resolve_hls_cvs_user_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    username: &str,
    password: &str,
) -> Result<CvsUserContext, Box<Response>> {
    let Some((user, target)) = app_state.app_config.get_target_for_user(username, password) else {
        return Err(Box::new(app_state.app_config.get_auth_error_status().into_response()));
    };

    if let Err(e) = check_network_access_only(&user, fingerprint, app_state) {
        return Err(Box::new(e.into_player_response(app_state.app_config.get_auth_error_status())));
    }

    Ok((user, target))
}

async fn cvs_typed_api(
    fingerprint: Fingerprint,
    axum::extract::Path((route_kind, username, password, stream_type)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: HeaderMap,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let route_kind = match route_kind.as_str() {
        "hls" => CvsRouteKind::Hls,
        "ts" => CvsRouteKind::Ts,
        _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };
    cvs_api_response(CvsApiResponseContext {
        fingerprint: &fingerprint,
        username: &username,
        password: &password,
        stream_type: &stream_type,
        route_kind,
        request_headers: &headers,
        raw_query: raw_query.as_deref(),
        app_state: &app_state,
    })
}

async fn cvs_api(
    fingerprint: Fingerprint,
    axum::extract::Path((username, password, stream_type)): axum::extract::Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    headers: HeaderMap,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    cvs_api_response(CvsApiResponseContext {
        fingerprint: &fingerprint,
        username: &username,
        password: &password,
        stream_type: &stream_type,
        route_kind: CvsRouteKind::Ts,
        request_headers: &headers,
        raw_query: raw_query.as_deref(),
        app_state: &app_state,
    })
}

fn cvs_api_response(context: CvsApiResponseContext<'_>) -> Response {
    let CvsApiResponseContext {
        fingerprint,
        username,
        password,
        stream_type,
        route_kind,
        request_headers,
        raw_query,
        app_state,
    } = context;

    if route_kind == CvsRouteKind::Hls {
        if let Some(index) = parse_hls_panel_provisioning_segment_route_name(stream_type) {
            if let Err(response) = resolve_cvs_user_context(app_state, fingerprint, username, password) {
                return *response;
            }
            return create_hls_provisioning_segment_response(app_state, request_headers, index);
        }
    }

    if route_kind == CvsRouteKind::Hls {
        if let Some(cvs_type) = stream_type.strip_suffix(".m3u8") {
            let Ok(custom_video_type) = CustomVideoStreamType::from_str(cvs_type) else {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            };
            let (user, _) = match resolve_hls_cvs_user_context(app_state, fingerprint, username, password) {
                Ok(context) => context,
                Err(response) => return *response,
            };
            return hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                &user,
                custom_video_type,
                StatusCode::NOT_FOUND,
                None,
            );
        }
    }

    let cvs_type = strip_hls_custom_video_media_extension(stream_type);

    let Ok(custom_video_type) = CustomVideoStreamType::from_str(cvs_type) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    if route_kind == CvsRouteKind::Ts {
        let api_proxy_user = create_api_proxy_user(app_state);
        if username == api_proxy_user.username && password == api_proxy_user.password {
            let token = raw_query.and_then(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .find_map(|(key, value)| (key == "token").then(|| value.into_owned()))
            });
            let Some(token) = token.as_deref() else {
                return app_state.app_config.get_auth_error_status().into_response();
            };
            if !verify_access_token(token, &app_state.app_config.access_token_secret) {
                return app_state.app_config.get_auth_error_status().into_response();
            }
            return create_custom_video_stream_response(app_state, &fingerprint.addr, custom_video_type).into_response();
        }
    }

    let auth_result = match route_kind {
        CvsRouteKind::Hls => resolve_hls_cvs_user_context(app_state, fingerprint, username, password),
        CvsRouteKind::Ts => resolve_cvs_user_context(app_state, fingerprint, username, password),
    };
    if let Err(response) = auth_result {
        return *response;
    }

    match route_kind {
        CvsRouteKind::Hls => create_hls_custom_video_segment_response(app_state, request_headers, custom_video_type),
        CvsRouteKind::Ts => create_custom_video_stream_response(app_state, &fingerprint.addr, custom_video_type)
            .into_response(),
    }
}

fn strip_hls_custom_video_media_extension(stream_type: &str) -> &str {
    stream_type
        .rsplit_once('.')
        .filter(|(_, extension)| HLS_CVS_MEDIA_EXTENSIONS.contains(extension))
        .map_or(stream_type, |(raw, _)| raw)
}

fn hls_provisioning_segment_buffer(app_state: &Arc<AppState>, index: usize) -> Option<TransportStreamBuffer> {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    custom_stream_response
        .as_ref()
        .and_then(|response| response.panel_api_provisioning_hls_segments.get(index).cloned())
}

fn create_hls_provisioning_segment_response(
    app_state: &Arc<AppState>,
    request_headers: &HeaderMap,
    index: usize,
) -> Response {
    let Some(video) = hls_provisioning_segment_buffer(app_state, index) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    build_hls_cvs_response_from_buffer(&video, request_headers.get(header::RANGE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsCvsRange {
    Full,
    Partial { start: usize, end: usize },
    Unsatisfiable,
}

fn insert_static_header(headers: &mut HeaderMap, name: HeaderName, value: &'static str) {
    headers.insert(name, HeaderValue::from_static(value));
}

fn insert_usize_header(headers: &mut HeaderMap, name: HeaderName, value: usize) -> bool {
    match HeaderValue::from_str(&value.to_string()) {
        Ok(header_value) => {
            headers.insert(name, header_value);
            true
        }
        Err(_) => false,
    }
}

fn insert_string_header(headers: &mut HeaderMap, name: HeaderName, value: &str) -> bool {
    match HeaderValue::from_str(value) {
        Ok(header_value) => {
            headers.insert(name, header_value);
            true
        }
        Err(_) => false,
    }
}

fn resolve_hls_cvs_range(range_header: Option<&HeaderValue>, full_size: usize) -> HlsCvsRange {
    let Some(range_header) = range_header.and_then(|value| value.to_str().ok()) else {
        return HlsCvsRange::Full;
    };
    let Some(range) = range_header.strip_prefix("bytes=") else {
        return HlsCvsRange::Unsatisfiable;
    };
    if full_size == 0 || range.contains(',') {
        return HlsCvsRange::Unsatisfiable;
    }
    let Some((start_raw, end_raw)) = range.split_once('-') else {
        return HlsCvsRange::Unsatisfiable;
    };
    if start_raw.is_empty() {
        let Ok(suffix_len) = end_raw.parse::<usize>() else {
            return HlsCvsRange::Unsatisfiable;
        };
        if suffix_len == 0 {
            return HlsCvsRange::Unsatisfiable;
        }
        let start = full_size.saturating_sub(suffix_len);
        return HlsCvsRange::Partial { start, end: full_size - 1 };
    }
    let Ok(start) = start_raw.parse::<usize>() else {
        return HlsCvsRange::Unsatisfiable;
    };
    if start >= full_size {
        return HlsCvsRange::Unsatisfiable;
    }
    let end = if end_raw.is_empty() {
        full_size - 1
    } else {
        let Ok(end) = end_raw.parse::<usize>() else {
            return HlsCvsRange::Unsatisfiable;
        };
        if end < start {
            return HlsCvsRange::Unsatisfiable;
        }
        end.min(full_size - 1)
    };
    HlsCvsRange::Partial { start, end }
}

fn build_hls_cvs_response_from_buffer(video: &TransportStreamBuffer, range_header: Option<&HeaderValue>) -> Response {
    let bytes = video.as_bytes();
    let full_size = bytes.len();
    let range = resolve_hls_cvs_range(range_header, full_size);
    let mut response = match range {
        HlsCvsRange::Full => {
            let mut builder = Response::builder().status(StatusCode::OK);
            let headers = builder.headers_mut().expect("response builder headers should be available");
            insert_static_header(headers, header::CONTENT_TYPE, HLS_CVS_CONTENT_TYPE);
            insert_static_header(headers, header::ACCEPT_RANGES, ACCEPT_RANGES_VALUE);
            insert_static_header(headers, header::CACHE_CONTROL, HLS_CVS_CACHE_CONTROL);
            if !insert_usize_header(headers, header::CONTENT_LENGTH, full_size) {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            builder
                .body(Body::from(Bytes::copy_from_slice(bytes)))
                .map_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response(), IntoResponse::into_response)
        }
        HlsCvsRange::Partial { start, end } => {
            let mut builder = Response::builder().status(StatusCode::PARTIAL_CONTENT);
            let headers = builder.headers_mut().expect("response builder headers should be available");
            insert_static_header(headers, header::CONTENT_TYPE, HLS_CVS_CONTENT_TYPE);
            insert_static_header(headers, header::ACCEPT_RANGES, ACCEPT_RANGES_VALUE);
            insert_static_header(headers, header::CACHE_CONTROL, HLS_CVS_CACHE_CONTROL);
            if !insert_usize_header(headers, header::CONTENT_LENGTH, end - start + 1)
                || !insert_string_header(headers, header::CONTENT_RANGE, &format!("bytes {start}-{end}/{full_size}"))
            {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            builder
                .body(Body::from(Bytes::copy_from_slice(&bytes[start..=end])))
                .map_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response(), IntoResponse::into_response)
        }
        HlsCvsRange::Unsatisfiable => {
            let mut builder = Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE);
            let headers = builder.headers_mut().expect("response builder headers should be available");
            insert_static_header(headers, header::ACCEPT_RANGES, ACCEPT_RANGES_VALUE);
            insert_static_header(headers, header::CACHE_CONTROL, HLS_CVS_CACHE_CONTROL);
            insert_static_header(headers, header::CONTENT_LENGTH, "0");
            if !insert_string_header(headers, header::CONTENT_RANGE, &format!("bytes */{full_size}")) {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            builder
                .body(Body::empty())
                .map_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response(), IntoResponse::into_response)
        }
    };
    mark_response_as_uncompressed(&mut response);
    response
}

fn hls_custom_video_buffer(
    app_state: &Arc<AppState>,
    custom_video_type: CustomVideoStreamType,
) -> Option<TransportStreamBuffer> {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    let custom_stream_response = custom_stream_response.as_ref()?;
    match custom_video_type {
        CustomVideoStreamType::ChannelUnavailable => custom_stream_response.channel_unavailable.clone(),
        CustomVideoStreamType::UserConnectionsExhausted => custom_stream_response.user_connections_exhausted.clone(),
        CustomVideoStreamType::ProviderConnectionsExhausted => custom_stream_response.provider_connections_exhausted.clone(),
        CustomVideoStreamType::LowPriorityPreempted => custom_stream_response.low_priority_preempted.clone(),
        CustomVideoStreamType::UserAccountExpired => custom_stream_response.user_account_expired.clone(),
        CustomVideoStreamType::Provisioning => custom_stream_response.panel_api_provisioning.clone(),
        CustomVideoStreamType::HlsSessionOrLeaseExpired => {
            custom_stream_response.hls_session_or_lease_expired.clone()
        }
    }
}

fn create_hls_custom_video_segment_response(
    app_state: &Arc<AppState>,
    request_headers: &HeaderMap,
    custom_video_type: CustomVideoStreamType,
) -> Response {
    let Some(video) = hls_custom_video_buffer(app_state, custom_video_type) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    build_hls_cvs_response_from_buffer(&video, request_headers.get(header::RANGE))
}

async fn cvs_provisioning_manifest_api(
    fingerprint: Fingerprint,
    axum::extract::Path((username, password)): axum::extract::Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<ProvisioningManifestQuery>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let (user, target) = match resolve_cvs_user_context(&app_state, &fingerprint, &username, &password) {
        Ok(context) => context,
        Err(response) => return *response,
    };

    let Some(input) = resolve_hls_virtual_input_for_target(&app_state, &target, query.id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, query.id);
    let server_path = app_state.app_config.get_user_server_info(&user).and_then(|server| server.path);
    hls_panel_provisioning_poll_manifest_response(
        &app_state,
        &fingerprint,
        &user,
        &target,
        &input,
        query.id,
        &original_hls_entry_path,
        server_path.as_deref(),
    )
    .await
}

async fn cvs_shared_provisioning_manifest_api(
    fingerprint: Fingerprint,
    axum::extract::Path((proxy_session_id, access_lease_id)): axum::extract::Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<ProvisioningManifestQuery>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    hls_shared_panel_provisioning_poll_manifest_response(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &access_lease_id,
        query.id,
    )
    .await
}

async fn cvs_shared_provisioning_segment_api(
    _fingerprint: Fingerprint,
    axum::extract::Path((_proxy_session_id, _access_lease_id, _stream_type)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
    _headers: HeaderMap,
    axum::extract::State(_app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    StatusCode::NOT_FOUND.into_response()
}

pub fn cvs_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/cvs/hls/{proxy_session_id}/{hls_access_lease_id}/s/provisioning.m3u8",
            axum::routing::get(cvs_shared_provisioning_manifest_api),
        )
        .route(
            "/cvs/hls/{proxy_session_id}/{hls_access_lease_id}/s/{stream_type}",
            axum::routing::get(cvs_shared_provisioning_segment_api),
        )
        .route(
            "/cvs/hls/{username}/{password}/provisioning.m3u8",
            axum::routing::get(cvs_provisioning_manifest_api),
        )
        .route("/cvs/{route_kind}/{username}/{password}/{stream_type}", axum::routing::get(cvs_typed_api))
        .route("/cvs/{username}/{password}/{stream_type}", axum::routing::get(cvs_api))
}

#[cfg(test)]
mod hls_cvs_tests {
    use super::{
        build_hls_cvs_response_from_buffer, resolve_hls_cvs_range, strip_hls_custom_video_media_extension,
        HlsCvsRange,
    };
    use crate::api::model::TransportStreamBuffer;
    use axum::{
        body::to_bytes,
        http::{
            header::{self, HeaderValue},
            StatusCode,
        },
    };

    fn test_ts_bytes() -> Vec<u8> {
        let mut bytes = vec![0_u8; 188 * 2];
        bytes[0] = 0x47;
        bytes[188] = 0x47;
        bytes[1] = b'a';
        bytes[189] = b'b';
        bytes
    }

    fn test_buffer() -> TransportStreamBuffer { TransportStreamBuffer::new(test_ts_bytes()) }

    async fn response_body(response: axum::response::Response) -> bytes::Bytes {
        to_bytes(response.into_body(), usize::MAX).await.expect("body should collect")
    }

    #[test]
    fn hls_cvs_range_zero_open_resolves_to_full_partial_range() {
        let range = HeaderValue::from_static("bytes=0-");

        assert_eq!(
            resolve_hls_cvs_range(Some(&range), 376),
            HlsCvsRange::Partial { start: 0, end: 375 }
        );
    }

    #[test]
    fn hls_cvs_suffix_range_resolves_from_tail() {
        let range = HeaderValue::from_static("bytes=-10");

        assert_eq!(
            resolve_hls_cvs_range(Some(&range), 376),
            HlsCvsRange::Partial { start: 366, end: 375 }
        );
    }

    #[test]
    fn hls_cvs_multi_range_is_unsatisfiable() {
        let range = HeaderValue::from_static("bytes=0-1,4-5");

        assert_eq!(resolve_hls_cvs_range(Some(&range), 376), HlsCvsRange::Unsatisfiable);
    }

    #[test]
    fn hls_cvs_media_extension_parser_accepts_supported_segment_types() {
        for extension in ["ts", "mp4", "m4s", "m4v"] {
            assert_eq!(
                strip_hls_custom_video_media_extension(&format!("channel_unavailable.{extension}")),
                "channel_unavailable"
            );
        }
        assert_eq!(strip_hls_custom_video_media_extension("channel_unavailable.m4a"), "channel_unavailable.m4a");
        assert_eq!(strip_hls_custom_video_media_extension("channel_unavailable.m3u8"), "channel_unavailable.m3u8");
    }

    #[tokio::test]
    async fn hls_cvs_segment_without_range_returns_finite_ts_body() {
        let buffer = test_buffer();

        let response = build_hls_cvs_response_from_buffer(&buffer, None);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "376");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));
        assert_eq!(response_body(response).await, bytes::Bytes::from(test_ts_bytes()));
    }

    #[tokio::test]
    async fn hls_cvs_segment_with_range_zero_open_returns_partial_content() {
        let buffer = test_buffer();
        let range = HeaderValue::from_static("bytes=0-");

        let response = build_hls_cvs_response_from_buffer(&buffer, Some(&range));

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-375/376");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "376");
        assert_eq!(response_body(response).await, bytes::Bytes::from(test_ts_bytes()));
    }

    #[tokio::test]
    async fn hls_cvs_segment_with_invalid_range_returns_416() {
        let buffer = test_buffer();
        let range = HeaderValue::from_static("bytes=999-");

        let response = build_hls_cvs_response_from_buffer(&buffer, Some(&range));

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */376");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(response_body(response).await, bytes::Bytes::new());
    }
}

#[cfg(test)]
mod tests {
    use super::cvs_api_register;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadQueue,
            EventManager, HlsProvisioningState, HlsProxyManager, MetadataUpdateManager, PlaylistStorageState,
            SharedStreamManager, TransportStreamBuffer, UpdateGuard,
        },
        model::{
            ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, ConfigSource, ConfigTarget,
            CustomStreamResponse, MediaToolCapabilities, ProxyUserCredentials, SourcesConfig, TargetOutput, TargetUser,
            XtreamTargetFlagsSet, XtreamTargetOutput,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
        response::IntoResponse,
        Router,
    };
    use crate::utils::{FileLockManager, GeoIp};
    use std::{collections::HashMap, sync::Arc};
    use tower::ServiceExt;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use shared::{foundation::Filter, model::{ConfigPaths, InputFetchMethod, InputType, ProcessingOrder}};

    fn test_fingerprint() -> crate::auth::Fingerprint {
        crate::auth::Fingerprint::new(
            "test-fingerprint".to_string(),
            "127.0.0.1".to_string(),
            "127.0.0.1:12345".parse().expect("socket addr"),
        )
    }

    fn create_test_app_config_with_channel_unavailable() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".into(),
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
        let target = Arc::new(ConfigTarget {
            id: 1,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![TargetOutput::Xtream(XtreamTargetOutput {
                flags: XtreamTargetFlagsSet::default(),
                trakt: None,
                filter: None,
            })],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = SourcesConfig {
            inputs: vec![Arc::clone(&input)],
            sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::clone(&target)] }],
            ..SourcesConfig::default()
        };
        let mut expired_user = ProxyUserCredentials::default();
        expired_user.username = "viewer".to_string();
        expired_user.password = "secret".to_string();
        expired_user.exp_date = Some(0);
        let expired_user = Arc::new(expired_user);
        let api_proxy = ApiProxyConfig {
            server: vec![ApiProxyServerInfo {
                name: "default".to_string(),
                protocol: "https".to_string(),
                host: "example.test".to_string(),
                port: None,
                timezone: "UTC".to_string(),
                message: String::new(),
                path: Some("iptv".to_string()),
            }],
            user: vec![TargetUser { target: target.name.clone(), credentials: vec![expired_user] }],
            ..ApiProxyConfig::default()
        };

        let app_cfg = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                user_access_control: true,
                custom_stream_response_enabled: true,
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::from_pointee(api_proxy)),
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

    fn create_test_app_state() -> Arc<AppState> {
        let app_cfg = Arc::new(create_test_app_config_with_channel_unavailable());
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
            qos_aggregation: CancellationToken::new(),
            downloads: CancellationToken::new(),
            hls_cache: CancellationToken::new(),
        };
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(crate::model::ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(HlsProxyManager::new()),
            hls_provisioning: Arc::new(HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    #[tokio::test]
    async fn custom_video_stream_response_returns_ok_for_channel_unavailable() {
        let app_state = create_test_app_state();
        let response = crate::api::model::create_custom_video_stream_response(
            &app_state,
            &test_fingerprint().addr,
            crate::api::model::CustomVideoStreamType::ChannelUnavailable,
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    }

    #[tokio::test]
    async fn cvs_api_rejects_internal_api_proxy_user_without_token() {
        let app_state = create_test_app_state();
        let router = cvs_api_register().with_state(app_state);
        let request = Request::builder()
            .method("GET")
            .uri("/cvs/api_user/api_user/channel_unavailable.ts")
            .body(Body::empty())
            .expect("request");

        let response = Router::into_service(router).oneshot(request).await.expect("response");

        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn hls_cvs_segment_allows_expired_user_custom_response() {
        let app_state = create_test_app_state();
        let headers = axum::http::HeaderMap::new();
        let fingerprint = test_fingerprint();

        for extension in ["ts", "mp4", "m4s", "m4v"] {
            let response = super::cvs_api_response(super::CvsApiResponseContext {
                fingerprint: &fingerprint,
                username: "viewer",
                password: "secret",
                stream_type: &format!("channel_unavailable.{extension}"),
                route_kind: super::CvsRouteKind::Hls,
                request_headers: &headers,
                raw_query: None,
                app_state: &app_state,
            });

            assert_eq!(response.status(), StatusCode::OK, "{extension} custom response should be served");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
        }
    }

    #[tokio::test]
    async fn hls_cvs_manifest_allows_expired_user_custom_response() {
        let app_state = create_test_app_state();
        let headers = axum::http::HeaderMap::new();
        let fingerprint = test_fingerprint();

        let response = super::cvs_api_response(super::CvsApiResponseContext {
            fingerprint: &fingerprint,
            username: "viewer",
            password: "secret",
            stream_type: "channel_unavailable.m3u8",
            route_kind: super::CvsRouteKind::Hls,
            request_headers: &headers,
            raw_query: None,
            app_state: &app_state,
        });

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body should collect");
        let body = String::from_utf8(body.to_vec()).expect("manifest is utf-8");
        assert!(body.contains("#EXTM3U"));
        assert!(body.contains("/cvs/hls/viewer/secret/channel_unavailable.ts"));
    }

    #[tokio::test]
    async fn hls_cvs_segment_rejects_unknown_credentials() {
        let app_state = create_test_app_state();
        let headers = axum::http::HeaderMap::new();
        let fingerprint = test_fingerprint();

        let response = super::cvs_api_response(super::CvsApiResponseContext {
            fingerprint: &fingerprint,
            username: "viewer",
            password: "wrong",
            stream_type: "channel_unavailable.ts",
            route_kind: super::CvsRouteKind::Hls,
            request_headers: &headers,
            raw_query: None,
            app_state: &app_state,
        });

        assert!(response.status().is_client_error());
    }
}
