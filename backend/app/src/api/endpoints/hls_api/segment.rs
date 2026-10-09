#![allow(clippy::wildcard_imports)]

use super::*;

pub(super) struct HlsResourceEndpointContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) lease_identity: HlsMediaLeaseIdentity,
    published_resource_ids: HlsPublishedTransientResourceIds,
    pub(super) resource_file: TransientResourceFile,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) now_ms: u64,
}

mod access;
mod entry;
mod fallback;
mod legacy;
mod registration;
mod resources;
mod routing;
mod segments;
mod transient_cache;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use access::{
    create_hls_cache_user_session_token, current_hls_resource_lease, duration_to_millis_saturating,
    hls_access_lease_timing_for_session, hls_access_lease_ttl_ms, hls_cache_response_context,
    hls_lease_allows_cached_segment, hls_lease_allows_live_origin_work, hls_live_lease_identity_is_current,
    hls_pending_bootstrap_window_ms, hls_qos_meter_init, is_hls_media_activity_status,
    log_hls_initial_strip_publication, prepare_hls_resource_access,
    register_hls_cache_stream_for_successful_media_response, stripped_tail_segments,
    touch_pending_manifest_follow_up_window, validate_hls_proxy_access_context, validate_hls_proxy_access_request,
    HlsResourceAccess,
};
pub(in crate::api) use entry::{build_virtual_hls_entry_path, handle_hls_stream_request};
pub(super) use entry::{get_stream_channel, HlsCacheManifestOrigin, HlsCacheOriginResolution};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use fallback::{
    hls_custom_video_manifest_response_for_lease, hls_custom_video_manifest_response_for_username,
    hls_manifest_access_context_and_state, hls_manifest_access_denial_runtime_response,
    hls_manifest_access_lease_validation_response, hls_manifest_channel_unavailable_response_for_username,
    hls_origin_runtime_resource_failure_response, hls_resource_access_lease_validation_response,
    hls_resource_channel_unavailable_response, hls_resource_serve_outcome_response, hls_runtime_custom_tail_response,
    hls_runtime_or_standalone_custom_tail_response, hls_transient_object_unavailable_response,
    hls_unpublished_lease_channel_unavailable_response,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use legacy::{
    build_hls_manifest_request_headers, download_legacy_hls_manifest, ensure_hls_manifest_extension,
    normalize_xtream_live_hls_url, release_prepared_hls_manifest_session, terminate_failed_hls_manifest_session,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use registration::{
    build_hls_cache_stream_channel, ensure_hls_cache_stream_registered, fallback_hls_cache_stream_channel,
    hls_cache_shared_joined_existing, hls_cache_shared_stream_id, hls_cache_stats_provider, hls_cache_stream_stats_url,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use resources::{
    fetch_or_passthrough_transient_resource, hls_proxy_map, hls_proxy_resource, serve_hls_live_transient_resource,
    serve_hls_terminal_key_resource, serve_hls_transient_passthrough_result, HlsTransientPassthroughContext,
};
pub(in crate::api) use routing::resolve_hls_virtual_source_for_target;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use routing::{
    hls_api_stream, hls_api_stream_resolved, hls_entry_user_session_token, hls_playback_kind,
    hls_stream_context_or_unavailable, resolve_hls_origin_playlist_url, resolve_stream_channel,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use segments::{
    build_hls_segment_fetch_context, demand_fetch_hls_live_segment, demand_fetch_hls_segment_if_needed,
    hls_effective_origin_acquire_policy, hls_origin_account_reservation_ttl_secs_fallback,
    hls_origin_account_reservation_ttl_secs_for_session, hls_proxy_segment, hls_proxy_terminal_segment,
    serve_hls_segment_for_current_lease, validate_hls_segment_entry,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use transient_cache::{
    fetch_and_cache_transient_origin_response, fetch_transient_origin_response_with_provider_io,
    hls_transient_origin_prepare_closure, safe_transient_resource_id, serve_transient_object_cache_response_and_mark,
    serve_transient_object_cache_response_and_mark_or_unavailable, wait_for_transient_object_cache_fetch,
    HlsTransientEndpointCacheFetchContext, HlsTransientEndpointOriginFetchRequest, HlsTransientOriginFetchResult,
    TransientObjectCacheServeContext, TransientObjectWaitContext,
};
