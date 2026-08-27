use crate::{
    api::{
        api_utils::create_api_proxy_user,
        endpoints::hls_api::{
            build_virtual_hls_entry_path, hls_panel_provisioning_poll_manifest_response,
            resolve_hls_virtual_source_for_target,
        },
        model::{
            create_custom_video_stream_response, hls_custom_video_manifest_response_with_virtual_id,
            parse_hls_panel_provisioning_segment_route_name, AppState, CustomVideoStreamType, TransportStreamBuffer,
        },
    },
    auth::{check_network_access_only, resolve_api_user_context, verify_access_token, Fingerprint},
    model::{ConfigTarget, ProxyUserCredentials},
};
use axum::{
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{str::FromStr, sync::Arc};
use tuliprox_hls::api::{
    finite_hls_immutable_media_response, resolve_hls_standalone_custom_segment, HlsAccessLeaseId,
    HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError,
};
use url::form_urlencoded;

const HLS_CVS_CONTENT_TYPE: &str = "video/mp2t";
const HLS_CVS_MEDIA_EXTENSIONS: &[&str] = &["ts", "mp4", "m4s", "m4v"];
const HLS_CVS_PROVISIONING_CACHE_CONTROL: &str = "no-store";
const HLS_CVS_IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

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

    if let Err(e) = resolve_api_user_context(
        user.clone(),
        target.clone(),
        fingerprint.clone(),
        &app_state.app_config,
        &app_state.geoip,
    ) {
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

    if let Err(e) = check_network_access_only(&user, fingerprint, &app_state.app_config, &app_state.geoip) {
        return Err(Box::new(e.into_player_response(app_state.app_config.get_auth_error_status())));
    }

    Ok((user, target))
}

fn resolve_hls_cvs_access_user(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    username: &str,
) -> Result<CvsUserContext, Box<Response>> {
    let Some((user, target)) = app_state.app_config.get_target_for_username(username) else {
        return Err(Box::new(app_state.app_config.get_auth_error_status().into_response()));
    };

    if let Err(e) = check_network_access_only(&user, fingerprint, &app_state.app_config, &app_state.geoip) {
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
    method: Method,
    headers: HeaderMap,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    if route_kind == "hls" && parse_cvs_standalone_hls_segment_file(&stream_type).is_some() {
        return cvs_standalone_hls_segment_response(
            &fingerprint,
            &username,
            &password,
            &stream_type,
            method,
            &headers,
            &app_state,
        )
        .await;
    }
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
    .await
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
    .await
}

async fn cvs_api_response(context: CvsApiResponseContext<'_>) -> Response {
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
            )
            .await;
        }
    }

    let cvs_type = strip_hls_custom_video_media_extension(stream_type);

    let Ok(custom_video_type) = CustomVideoStreamType::from_str(cvs_type) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };

    if route_kind == CvsRouteKind::Ts {
        let api_proxy_user = create_api_proxy_user(app_state);
        if username == api_proxy_user.username
            && crate::auth::constant_time_eq(password.as_bytes(), api_proxy_user.password.as_bytes())
        {
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
            return create_custom_video_stream_response(
                &app_state.provider_stream_ctx(),
                &fingerprint.addr,
                custom_video_type,
            )
            .into_response();
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
        CvsRouteKind::Hls => StatusCode::NOT_FOUND.into_response(),
        CvsRouteKind::Ts => {
            create_custom_video_stream_response(&app_state.provider_stream_ctx(), &fingerprint.addr, custom_video_type)
                .into_response()
        }
    }
}

async fn cvs_standalone_hls_segment_response(
    fingerprint: &Fingerprint,
    access_lease: &str,
    asset_fingerprint: &str,
    segment_file: &str,
    method: Method,
    headers: &HeaderMap,
    app_state: &Arc<AppState>,
) -> Response {
    let Some(index) = parse_cvs_standalone_hls_segment_file(segment_file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let access_lease = HlsAccessLeaseId(access_lease.to_string());
    let now_ms = current_time_millis();
    let access = match resolve_hls_standalone_custom_segment(
        &app_state.hls_ctx(),
        &access_lease,
        asset_fingerprint,
        index,
        now_ms,
    ) {
        Ok(access) => access,
        Err(
            HlsStandaloneCustomSegmentError::InvalidIndex
            | HlsStandaloneCustomSegmentError::UnknownAccessLease
            | HlsStandaloneCustomSegmentError::StaleAssetFingerprint,
        ) => return StatusCode::NOT_FOUND.into_response(),
    };
    if let Err(response) =
        validate_hls_standalone_custom_access(app_state, fingerprint, &access_lease, &access, now_ms).await
    {
        return *response;
    }
    finite_hls_immutable_media_response(
        access.bytes,
        headers.get(header::RANGE),
        HLS_CVS_CONTENT_TYPE,
        HLS_CVS_IMMUTABLE_CACHE_CONTROL,
        method == Method::HEAD,
    )
}

async fn validate_hls_standalone_custom_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    access_lease_id: &HlsAccessLeaseId,
    access: &HlsStandaloneCustomSegmentAccess,
    now_ms: u64,
) -> Result<(), Box<Response>> {
    if let Some(shared_lease) = access.shared_lease.as_ref() {
        let lease = app_state
            .hls_proxy
            .access_lease_response_snapshot(access_lease_id, &shared_lease.proxy_session_id, now_ms)
            .await;
        if !lease.is_some_and(|lease| {
            lease.issued_at_ms == shared_lease.lease_issued_at_ms && lease.username == access.username.as_ref()
        }) {
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        }
    }
    resolve_hls_cvs_access_user(app_state, fingerprint, access.username.as_ref()).map(|_| ())
}

use tuliprox_core::utils::current_time_millis;

fn parse_cvs_standalone_hls_segment_file(segment_file: &str) -> Option<u16> {
    let index = segment_file.strip_suffix(".ts")?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
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

fn build_hls_cvs_response_from_buffer(video: &TransportStreamBuffer, range_header: Option<&HeaderValue>) -> Response {
    // `clone_bytes` returns a `Bytes` (refcount bump) instead of forcing
    // `Bytes::copy_from_slice` to memcpy the entire TS payload each response.
    build_hls_cvs_response_from_bytes(video.clone_bytes(), range_header, false)
}

fn build_hls_cvs_response_from_bytes(
    bytes_owned: bytes::Bytes,
    range_header: Option<&HeaderValue>,
    head_only: bool,
) -> Response {
    finite_hls_immutable_media_response(
        bytes_owned,
        range_header,
        HLS_CVS_CONTENT_TYPE,
        HLS_CVS_PROVISIONING_CACHE_CONTROL,
        head_only,
    )
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

    let source = match resolve_hls_virtual_source_for_target(&app_state, &target, query.id).await {
        Ok(source) => source,
        Err(status) => return status.into_response(),
    };

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &source.input, &user, query.id);
    let server_path = app_state.app_config.get_user_server_info(&user).and_then(|server| server.path);
    hls_panel_provisioning_poll_manifest_response(
        &app_state,
        &fingerprint,
        &user,
        &target,
        &source.input,
        source.stream_context.identity(),
        &original_hls_entry_path,
        server_path.as_deref(),
    )
    .await
}

pub fn cvs_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/cvs/hls/{username}/{password}/provisioning.m3u8", axum::routing::get(cvs_provisioning_manifest_api))
        .route("/cvs/{route_kind}/{username}/{password}/{stream_type}", axum::routing::get(cvs_typed_api))
        .route("/cvs/{username}/{password}/{stream_type}", axum::routing::get(cvs_api))
}

#[cfg(test)]
mod hls_cvs_tests {
    use super::{build_hls_cvs_response_from_buffer, strip_hls_custom_video_media_extension};
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
    use super::{cvs_api_register, parse_cvs_standalone_hls_segment_file};
    use crate::{
        api::model::{
            build_hls_standalone_custom_plan, hls_custom_video_manifest_response_for_access_lease,
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, CustomVideoStreamType,
            DownloadQueue, EventManager, HlsAccessLease, HlsAccessLeaseId, HlsPlaybackFamilyKey, HlsProvisioningState,
            HlsProxyManager, HlsRuntimeCustomTailReason, HlsStandaloneCustomAccess, MetadataUpdateManager,
            PlaylistStorageState, ProxySessionId, SharedStreamManager, TransportStreamBuffer, UpdateGuard,
        },
        model::{
            ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, ConfigSource, ConfigTarget,
            CustomStreamResponse, MediaToolCapabilities, ProxyUserCredentials, SourcesConfig, TargetOutput, TargetUser,
            XtreamTargetFlagsSet, XtreamTargetOutput,
        },
        repository::GeoIp,
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        body::Body,
        http::{header, Method, Request, StatusCode},
        response::IntoResponse,
        Router,
    };
    use shared::{
        foundation::Filter,
        model::{ConfigPaths, InputFetchMethod, InputType, ProcessingOrder},
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    fn test_fingerprint() -> crate::auth::Fingerprint {
        crate::auth::Fingerprint::new(
            "test-fingerprint".to_string(),
            "127.0.0.1".to_string(),
            "127.0.0.1:12345".parse().expect("socket addr"),
        )
    }

    fn test_custom_stream_response() -> CustomStreamResponse {
        CustomStreamResponse {
            channel_unavailable: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
                    .to_vec(),
            )),
            user_connections_exhausted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/user_connections_exhausted.ts"
                ))
                .to_vec(),
            )),
            provider_connections_exhausted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/provider_connections_exhausted.ts"
                ))
                .to_vec(),
            )),
            low_priority_preempted: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/low_priority_preempted.ts"
                ))
                .to_vec(),
            )),
            user_account_expired: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_account_expired.ts"))
                    .to_vec(),
            )),
            panel_api_provisioning: None,
            hls_session_or_lease_expired: Some(TransportStreamBuffer::new(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../test/fixtures/hls/hls_session_or_lease_expired.ts"
                ))
                .to_vec(),
            )),
            panel_api_provisioning_hls_segments: Vec::new(),
        }
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

        app_cfg.custom_stream_response.store(Some(Arc::new(test_custom_stream_response())));
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
            public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
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
            &app_state.provider_stream_ctx(),
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
    async fn hls_cvs_legacy_raw_segment_route_is_not_used_by_finite_manifests() {
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
            })
            .await;

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{extension} raw route must stay unavailable");
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
        })
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body should collect");
        let body = String::from_utf8(body.to_vec()).expect("manifest is utf-8");
        assert!(body.contains("#EXTM3U"));
        let urls = custom_segment_urls(&body);
        assert_eq!(urls.len(), 12);
        let (access_lease, asset_fingerprint, index) = standalone_route_parts(urls[0]);
        assert_eq!(access_lease.len(), 22);
        assert_eq!(asset_fingerprint.len(), 16);
        assert_eq!(index, 0);
        assert!(!body.contains("/viewer/"));
        assert!(!body.contains("/secret/"));
        assert!(!body.contains("/channel_unavailable/"));
    }

    async fn custom_manifest_body(app_state: &Arc<AppState>, video_type: CustomVideoStreamType) -> String {
        let user = app_state.app_config.get_user_credentials("viewer").expect("test user");
        let response = crate::api::model::hls_custom_video_manifest_response_with_virtual_id(
            app_state,
            &user,
            video_type,
            StatusCode::NOT_FOUND,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("custom manifest body").to_vec(),
        )
        .expect("custom manifest utf8")
    }

    fn custom_segment_urls(manifest: &str) -> Vec<&str> {
        manifest.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect()
    }

    fn standalone_route_parts(url: &str) -> (String, String, u16) {
        let parsed = url::Url::parse(url).expect("absolute custom segment URL");
        let components = parsed.path_segments().expect("custom path components").collect::<Vec<_>>();
        let count = components.len();
        let access_lease = components.get(count.saturating_sub(3)).expect("access lease").to_string();
        let asset_fingerprint = components.get(count.saturating_sub(2)).expect("asset fingerprint").to_string();
        let index = components
            .get(count.saturating_sub(1))
            .and_then(|file| file.strip_suffix(".ts"))
            .and_then(|value| value.parse::<u16>().ok())
            .expect("custom segment index");
        (access_lease, asset_fingerprint, index)
    }

    async fn standalone_segment_response(
        app_state: Arc<AppState>,
        url: &str,
        method: Method,
        range: Option<&'static str>,
    ) -> axum::response::Response {
        let parsed = url::Url::parse(url).expect("absolute custom segment URL");
        let route_start = parsed.path().rfind("/cvs/").expect("custom segment route");
        let route_path = &parsed.path()[route_start..];
        let request_uri =
            parsed.query().map_or_else(|| route_path.to_string(), |query| format!("{route_path}?{query}"));
        let mut request = Request::builder().method(method).uri(request_uri);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let mut request = request.body(Body::empty()).expect("standalone custom segment request");
        request.extensions_mut().insert(axum::extract::ConnectInfo(test_fingerprint().addr));
        let router = cvs_api_register().with_state(app_state);
        Router::into_service(router).oneshot(request).await.expect("standalone custom segment response")
    }

    fn configured_low_priority_buffer(app_state: &Arc<AppState>) -> TransportStreamBuffer {
        app_state
            .app_config
            .custom_stream_response
            .load_full()
            .and_then(|responses| responses.low_priority_preempted.clone())
            .expect("low-priority test buffer")
    }

    fn replace_low_priority_asset(app_state: &Arc<AppState>, bytes: Vec<u8>) {
        let responses =
            app_state.app_config.custom_stream_response.load_full().expect("custom responses before reload");
        let mut revised = responses.as_ref().clone();
        revised.low_priority_preempted = Some(TransportStreamBuffer::new(bytes));
        app_state.app_config.custom_stream_response.store(Some(Arc::new(revised)));
    }

    async fn assert_initial_policy_uses_standalone_finite_response(
        video_type: CustomVideoStreamType,
        reason: HlsRuntimeCustomTailReason,
    ) {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, video_type).await;
        let urls = custom_segment_urls(&body);
        assert_eq!(urls.len(), 12);
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
        assert!(urls.iter().all(|url| !url.contains(&format!("/{}/", reason.as_label()))));
        let (access_lease, asset_fingerprint, _) = standalone_route_parts(urls[0]);
        assert!(urls.iter().all(|url| {
            let (candidate_lease, candidate_fingerprint, _) = standalone_route_parts(url);
            candidate_lease == access_lease && candidate_fingerprint == asset_fingerprint
        }));
        assert_eq!(asset_fingerprint.len(), 16);
        assert_eq!(urls.iter().copied().collect::<std::collections::HashSet<_>>().len(), 12);
    }

    #[tokio::test]
    async fn cold_provider_exhaustion_uses_standalone_finite_response() {
        assert_initial_policy_uses_standalone_finite_response(
            CustomVideoStreamType::ProviderConnectionsExhausted,
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
        )
        .await;
    }

    #[tokio::test]
    async fn initial_user_exhaustion_uses_standalone_finite_response() {
        assert_initial_policy_uses_standalone_finite_response(
            CustomVideoStreamType::UserConnectionsExhausted,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        )
        .await;
    }

    #[tokio::test]
    async fn initial_user_account_expiry_uses_standalone_finite_response() {
        assert_initial_policy_uses_standalone_finite_response(
            CustomVideoStreamType::UserAccountExpired,
            HlsRuntimeCustomTailReason::UserAccountExpired,
        )
        .await;
    }

    #[tokio::test]
    async fn standalone_custom_manifest_uses_unique_segment_urls() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let urls = custom_segment_urls(&body);

        assert_eq!(urls.len(), 12);
        assert_eq!(urls.iter().copied().collect::<std::collections::HashSet<_>>().len(), urls.len());
        for (index, url) in urls.iter().enumerate() {
            assert!(url.ends_with(&format!("/{index}.ts")));
        }
    }

    #[test]
    fn standalone_custom_segment_file_requires_numeric_ts_name() {
        assert_eq!(parse_cvs_standalone_hls_segment_file("0.ts"), Some(0));
        assert_eq!(parse_cvs_standalone_hls_segment_file("65535.ts"), Some(u16::MAX));
        for invalid in ["", ".ts", "index.ts", "+1.ts", "65536.ts", "0.m4s", "0.ts.extra"] {
            assert_eq!(parse_cvs_standalone_hls_segment_file(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn standalone_custom_segment_route_serves_generated_ts_uri() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let url = custom_segment_urls(&body)[0];

        let response = standalone_segment_response(app_state, url, Method::GET, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    }

    #[tokio::test]
    async fn shared_standalone_custom_segment_requires_same_access_lease_incarnation() {
        let app_state = create_test_app_state();
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId("shared-access-lease".to_string());
        let proxy_session_id = ProxySessionId("shared-proxy-session".to_string());
        let lease = HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("viewer", "client"),
            proxy_session_id.clone(),
            "viewer".to_string(),
            "user-session".to_string(),
            1,
            "stream".to_string(),
            7,
            now_ms,
            60_000,
        );
        app_state.hls_proxy.prepare_access_lease(lease.clone()).await;
        let user = app_state.app_config.get_user_credentials("viewer").expect("test user");
        let response = hls_custom_video_manifest_response_for_access_lease(
            &app_state,
            &user,
            CustomVideoStreamType::ChannelUnavailable,
            StatusCode::NOT_FOUND,
            &lease,
        )
        .await;
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("shared custom manifest").to_vec(),
        )
        .expect("shared custom manifest utf8");
        let url = custom_segment_urls(&body)[0];
        assert_eq!(standalone_route_parts(url).0, lease_id.0);
        assert_eq!(
            standalone_segment_response(Arc::clone(&app_state), url, Method::GET, None).await.status(),
            StatusCode::OK
        );

        app_state.hls_proxy.access_leases().write().await.remove_access_lease(&lease_id);
        let replacement = HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("viewer", "client"),
            proxy_session_id,
            "viewer".to_string(),
            "replacement-session".to_string(),
            1,
            "stream".to_string(),
            7,
            now_ms.saturating_add(1),
            60_000,
        );
        app_state.hls_proxy.prepare_access_lease(replacement).await;

        assert_eq!(
            standalone_segment_response(app_state, url, Method::GET, None).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn standalone_custom_segment_route_rejects_non_ts_filename() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let url = custom_segment_urls(&body)[0];
        let invalid_url = format!("{}.m4s", url.strip_suffix(".ts").expect("generated TS segment URL"));

        let response = standalone_segment_response(app_state, &invalid_url, Method::GET, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn standalone_custom_segments_advance_by_exact_asset_duration() {
        let app_state = create_test_app_state();
        for reason in [
            HlsRuntimeCustomTailReason::ChannelUnavailable,
            HlsRuntimeCustomTailReason::LowPriorityPreempted,
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            HlsRuntimeCustomTailReason::UserAccountExpired,
            HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
        ] {
            let plan = build_hls_standalone_custom_plan(
                &app_state.hls_ctx(),
                "https://example.test/iptv",
                HlsStandaloneCustomAccess::for_user("viewer"),
                reason,
                super::current_time_millis(),
            )
            .await
            .expect("standalone custom plan");

            assert_eq!(plan.prepared_bundle.source_asset_duration_ticks_90khz, 902_400, "{reason:?}");
            assert_eq!(plan.prepared_bundle.source_asset_duration_ms, 10_027, "{reason:?}");
            assert!(plan.manifest_body.contains("#EXTINF:10.027,\n"), "{reason:?}");
            assert_eq!(plan.prepared_bundle.segments[0].timestamp_offset_ticks_90khz, 0);
            for pair in plan.prepared_bundle.segments.windows(2) {
                assert_eq!(
                    pair[1].timestamp_offset_ticks_90khz.saturating_sub(pair[0].timestamp_offset_ticks_90khz),
                    902_400,
                    "{reason:?}"
                );
                assert_ne!(pair[0].bytes, pair[1].bytes, "{reason:?}");
            }
        }
    }

    #[tokio::test]
    async fn standalone_custom_segment_route_keeps_immutable_plan_after_asset_reload() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let url = custom_segment_urls(&body)[0];
        let before = standalone_segment_response(Arc::clone(&app_state), url, Method::GET, None).await;
        assert_eq!(before.status(), StatusCode::OK);
        let before = axum::body::to_bytes(before.into_body(), usize::MAX).await.expect("original immutable body");
        let mut revised =
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/low_priority_preempted.ts"))
                .to_vec();
        *revised.last_mut().expect("non-empty low-priority asset") ^= 1;
        replace_low_priority_asset(&app_state, revised);

        let response = standalone_segment_response(app_state, url, Method::GET, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("replayed immutable body"),
            before
        );
    }

    #[tokio::test]
    async fn standalone_custom_segment_route_rejects_wrong_asset_fingerprint() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let url = custom_segment_urls(&body)[0];
        let (_, asset_fingerprint, _) = standalone_route_parts(url);
        let wrong_fingerprint =
            if asset_fingerprint == "0000000000000000" { "1111111111111111" } else { "0000000000000000" };
        let invalid_url = url.replacen(&asset_fingerprint, wrong_fingerprint, 1);

        let response = standalone_segment_response(app_state, &invalid_url, Method::GET, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn standalone_custom_range_requests_slice_prepared_immutable_bytes() {
        let app_state = create_test_app_state();
        let plan = build_hls_standalone_custom_plan(
            &app_state.hls_ctx(),
            "https://example.test/iptv",
            HlsStandaloneCustomAccess::for_user("viewer"),
            HlsRuntimeCustomTailReason::LowPriorityPreempted,
            super::current_time_millis(),
        )
        .await
        .expect("standalone custom plan");
        let url = custom_segment_urls(&plan.manifest_body)[0];
        let expected = plan.segment_bytes(0).expect("prepared segment zero");

        let response = standalone_segment_response(Arc::clone(&app_state), url, Method::GET, Some("bytes=0-187")).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "188");
        assert!(response.headers()[header::CACHE_CONTROL].to_str().is_ok_and(|value| value.contains("immutable")));
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("range body"),
            expected.slice(..188)
        );
        let head = standalone_segment_response(app_state, url, Method::HEAD, None).await;
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::CONTENT_LENGTH], expected.len().to_string());
        assert!(axum::body::to_bytes(head.into_body(), usize::MAX).await.expect("HEAD body").is_empty());
    }

    #[tokio::test]
    async fn standalone_custom_endpoint_performs_no_request_time_timestamp_rewrite() {
        let app_state = create_test_app_state();
        let buffer = configured_low_priority_buffer(&app_state);
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let urls = custom_segment_urls(&body);
        let render_count = buffer.finite_hls_render_count();
        let finalize_count = buffer.finite_hls_finalize_count();
        assert_eq!(render_count, urls.len());
        assert_eq!(finalize_count, 0);

        for url in urls.iter().take(2) {
            let response = standalone_segment_response(Arc::clone(&app_state), url, Method::GET, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("immutable segment body")
                .is_empty());
        }

        assert_eq!(buffer.finite_hls_render_count(), render_count);
        assert_eq!(buffer.finite_hls_finalize_count(), finalize_count);
    }

    #[tokio::test]
    async fn standalone_low_priority_preempted_manifest_has_no_repeated_raw_segment_uri() {
        let app_state = create_test_app_state();
        let body = custom_manifest_body(&app_state, CustomVideoStreamType::LowPriorityPreempted).await;
        let urls = custom_segment_urls(&body);

        assert!(!body.contains("/low_priority_preempted.ts\n"));
        assert_eq!(urls.iter().copied().collect::<std::collections::HashSet<_>>().len(), 12);
        assert!(urls.iter().all(|url| !url.contains("/low_priority_preempted/")));
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
        })
        .await;

        assert!(response.status().is_client_error());
    }
}
