use super::{AppState, HlsOriginIoContext, HlsSessionHandle};
use axum::http::HeaderMap;
use std::sync::Arc;
use tuliprox_hls::api::{
    HlsAccessLeaseId, HlsManifestAcceptanceDirective, HlsManifestCommitRequirement, HlsPostRefreshRuntime,
    HlsPreparedOriginAccount, OriginRefreshRequest, RetryPolicy,
};

pub(super) struct HlsOriginRefreshOptions {
    pub headers: HeaderMap,
    pub origin_io: HlsOriginIoContext,
    pub manifest_commit_requirement: HlsManifestCommitRequirement,
    pub acceptance_directive: HlsManifestAcceptanceDirective,
    pub access_lease_id: Option<HlsAccessLeaseId>,
    pub rewrite_secret: Vec<u8>,
    pub now_ms: u64,
}

pub(super) fn hls_origin_refresh_request(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    prepared: HlsPreparedOriginAccount,
    options: HlsOriginRefreshOptions,
) -> OriginRefreshRequest {
    let mut origin_io = options.origin_io;
    let origin_provider_session_headers = prepared.snapshot.provider_session_headers.clone();
    origin_io.prepared_account = Some(prepared.snapshot);
    OriginRefreshRequest {
        work_owner: None,
        attempt: None,
        app_config: Arc::clone(&app_state.app_config),
        session: Arc::clone(session),
        origin_entry: prepared.entry,
        headers: options.headers,
        origin_provider_session_headers,
        disabled_headers: app_state.get_disabled_headers(),
        client: app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: app_state.http_clients.no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        segment_cache: Arc::clone(app_state.hls.proxy.segment_cache()),
        hls_proxy: Arc::clone(&app_state.hls.proxy),
        segment_repair: Arc::clone(app_state.hls.proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls.proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls.proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls.proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: app_state.hls.proxy.manifest_recovery_burst(),
        strip: app_state.hls.proxy.strip().clone(),
        retry_policy: RetryPolicy::default(),
        reverse_proxy_rewrite_secret: options.rewrite_secret,
        transient_resource_ttl_ms: app_state.hls.proxy.transient_resource_ttl_ms(),
        manifest_commit_requirement: options.manifest_commit_requirement,
        fresh_manifest_requirement_generation: None,
        refresh_identity: None,
        acceptance_directive: options.acceptance_directive,
        access_lease_id: options.access_lease_id,
        now_ms: options.now_ms,
        origin_io: Some(origin_io),
        post_refresh_runtime: Some(HlsPostRefreshRuntime { ctx: app_state.hls_ctx().downgrade() }),
    }
}
