use super::{
    super::{
        header, hls_api_register, AppState, Arc, Body, BodyExt, Config, ConnectInfo, CustomStreamResponse,
        HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsSessionHandle, Method, Request,
        Response, ServiceExt, StatusCode, StripConfig,
    },
    grant_hls_proxy_lease, publish_test_transient_resource_membership, single_variant_master_playlist, test_addr,
    test_custom_video_buffer,
};

pub(in crate::api::endpoints::hls_api::tests) fn disable_custom_stream_response(app_state: &Arc<AppState>) {
    app_state.app_config.config.store(Arc::new(Config { custom_stream_response_enabled: false, ..Default::default() }));
}

pub(in crate::api::endpoints::hls_api::tests) fn enable_channel_unavailable_custom_response(app_state: &Arc<AppState>) {
    let config = app_state.app_config.config.load();
    app_state
        .app_config
        .config
        .store(Arc::new(Config { custom_stream_response_enabled: true, ..config.as_ref().clone() }));
    app_state.app_config.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
        channel_unavailable: Some(test_custom_video_buffer()),
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
}

pub(in crate::api::endpoints::hls_api::tests) async fn try_test_hls_cached_manifest_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &StripConfig,
    server_path: Option<&str>,
    options: super::super::HlsCachedManifestOptions,
) -> Option<axum::response::Response> {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let now_ms = super::super::super::current_time_millis();
    if app_state.hls.proxy.access_lease_response_snapshot(access_lease_id, &proxy_session_id, now_ms).await.is_none() {
        app_state
            .hls
            .proxy
            .prepare_access_lease(HlsAccessLease::pending(
                access_lease_id.clone(),
                HlsPlaybackFamilyKey::new("test-user", "manifest-test-client"),
                proxy_session_id,
                "test-user".to_string(),
                "manifest-test-session".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                60_000,
            ))
            .await;
    }
    super::super::super::try_hls_cached_manifest_response(
        app_state,
        session,
        access_lease_id,
        access_lease_state,
        strip,
        server_path,
        options,
        super::super::super::HlsRuntimeBandwidthLearningContext::Disabled,
    )
    .await
}

pub(in crate::api::endpoints::hls_api::tests) async fn hls_proxy_uri(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
    suffix: &str,
) -> String {
    let access_lease_id = grant_hls_proxy_lease(app_state, proxy_session_id).await;
    let uri = format!("/hls/shared/live/{proxy_session_id}/{access_lease_id}/{suffix}");
    if suffix.starts_with("r/") {
        publish_test_transient_resource_membership(app_state, proxy_session_id, &access_lease_id, &uri).await;
    }
    uri
}

pub(in crate::api::endpoints::hls_api::tests) async fn get_response(
    app_state: Arc<AppState>,
    uri: &str,
    range: Option<&str>,
) -> Response<Body> {
    request_response(app_state, Method::GET, uri, range).await
}

pub(in crate::api::endpoints::hls_api::tests) async fn request_response(
    app_state: Arc<AppState>,
    method: Method,
    uri: &str,
    range: Option<&str>,
) -> Response<Body> {
    let router = hls_api_register().with_state(app_state);
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    let mut request = request.body(Body::empty()).expect("request should build");
    request.extensions_mut().insert(ConnectInfo(test_addr()));
    router.oneshot(request).await.expect("response")
}

pub(in crate::api::endpoints::hls_api::tests) async fn get_status(app_state: Arc<AppState>, uri: &str) -> StatusCode {
    get_response(app_state, uri, None).await.status()
}

pub(in crate::api::endpoints::hls_api::tests) async fn response_body(response: Response<Body>) -> bytes::Bytes {
    response.into_body().collect().await.expect("body should collect").to_bytes()
}

pub(in crate::api::endpoints::hls_api::tests) async fn single_variant_uri(response: Response<Body>) -> String {
    single_variant_master_playlist(response).await.1
}

pub(in crate::api::endpoints::hls_api::tests) fn access_lease_id_from_variant_uri(uri: &str) -> &str {
    uri.trim_end_matches("/manifest.m3u8").rsplit('/').next().expect("access lease id in variant URI")
}

pub(in crate::api::endpoints::hls_api::tests) fn proxy_session_id_from_variant_uri(uri: &str) -> &str {
    let mut parts = uri.trim_end_matches("/manifest.m3u8").rsplit('/');
    let _access_lease_id = parts.next().expect("access lease id in variant URI");
    parts.next().expect("proxy session id in variant URI")
}
