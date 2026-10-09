#![allow(clippy::large_futures)]

// Route shared Xtream URL helpers through the one-way `xtream_url` boundary so
// this module does not import a sibling endpoint directly.
use super::{
    hls_terminal_response::{
        hls_manifest_terminal_preflight, hls_response, hls_temporary_resource_unavailable_response,
        hls_terminal_failed_closed_response, hls_terminal_playback_response, resolve_hls_terminal_manifest_state,
        terminal_segment_get_response, terminal_segment_head_response, terminal_segment_immutable_replay_response,
        terminal_tail_plan_for_current_route, HlsManifestTerminalPreflight,
    },
    xtream_url::{get_query_path, get_xtream_player_api_stream_url, ApiStreamContext},
};
use crate::{
    api::{
        api_utils::{
            connection_priority_for_kind, create_playback_session_fingerprint, force_hls_resource_response,
            get_hls_session_ttl_secs, get_stream_alternative_url, resolve_playback_request_admission,
            select_provider_stream_url, EvictionReentryGuard,
        },
        model::{
            AppState, CustomVideoStreamType, GraceMode, PlaybackLeaseRef, ProviderAllocation,
            ProviderConfig as RuntimeProviderConfig, ProviderHandle, UserSession,
        },
    },
    auth::{check_network_access_only, Fingerprint},
    model::{ConfigInput, ConfigProvider, ConfigTarget, PlaybackKind, ProxyUserCredentials},
    processing::parser::hls::origin_manifest::HlsManifestWindowPolicy,
    repository::{load_input_live_bitrate_bps, m3u_get_item_for_stream_id, xtream_get_item_for_stream_id},
    utils::debug_if_enabled,
};
use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use log::{debug, warn};
use serde::Deserialize;
use shared::{
    model::{
        ConnectFailureReason, InputType, PlaylistEntry, PlaylistItemType, StreamChannel, StreamProperties, TargetType,
        UserConnectionPermission, XtreamCluster,
    },
    utils::{is_m3u_catchup_session_token, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::{
    api::{
        begin_hls_origin_account_io_bounded, build_hls_origin_session_owner, build_proxy_session_id,
        cold_start_retry_after_seconds, hls_object_body_deadline, hls_origin_account_status, new_hls_access_lease_id,
        origin_account_binding_from_allocation, safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key,
        safe_user_session_token, HlsAccessContext, HlsAccessLease, HlsAccessLeaseState, HlsAccountBindingProtection,
        HlsEffectiveOriginAcquirePolicy, HlsLogIdentity, HlsManifestCommitIdentity, HlsMasterBandwidth,
        HlsMasterBandwidthSelection, HlsMediaLeaseIdentity, HlsOriginAccountBinding, HlsOriginAccountBindingMode,
        HlsOriginAccountDetachedReason, HlsOriginAccountStatus, HlsOriginIoContext, HlsOriginSource,
        HlsOriginSourceKind, HlsOriginWorkClass, HlsPlaybackFamilyKey, HlsPublishedTransientResourceIds,
        HlsSegmentFile, HlsSessionHandle, HlsSessionMode, HlsSingleVariantMasterPlaylist, HlsTransientManifestTemplate,
        HlsTransientOriginIoGuard, ProxySessionId, SegmentCacheStatus, TransientManifestGeneration,
        TransientResourceFile,
    },
    HlsCtx,
};
use url::Url;

pub(super) const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS: u64 = 1;

pub(super) const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS: u64 = HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS * 1_000;

/// Poll interval while waiting for a canonical manifest commit. Lower values
/// reduce time-to-first-manifest at the cost of more wakeups per waiting client.
pub(super) const HLS_MANIFEST_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Deserialize)]
pub(super) struct HlsApiPathParams {
    pub(super) username: String,
    pub(super) password: String,
    pub(super) target_id: u16,
    pub(super) input_id: u16,
    pub(super) stream_id: u32,
    /// Single obfuscated token, or a leaked relative origin path (`dvr-YYYY/...ts`).
    pub(super) token: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct HlsProxySegmentPathParams {
    pub(super) proxy_session_id: String,
    pub(super) hls_access_lease_id: String,
    pub(super) segment_file: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct HlsProxyManifestPathParams {
    pub(super) proxy_session_id: String,
    pub(super) hls_access_lease_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct HlsProxyMapPathParams {
    pub(super) proxy_session_id: String,
    pub(super) hls_access_lease_id: String,
    pub(super) map_file: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct HlsProxyResourcePathParams {
    pub(super) proxy_session_id: String,
    pub(super) hls_access_lease_id: String,
    pub(super) resource_file: String,
}

mod catchup;
mod manifest;
mod owner_token;
mod segment;
mod session;

pub(in crate::api) use catchup::*;
pub(in crate::api) use manifest::*;
pub(in crate::api) use owner_token::*;
pub(in crate::api) use segment::*;
pub(in crate::api) use session::*;

pub fn hls_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/manifest.m3u8",
            axum::routing::get(hls_proxy_manifest),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/terminal/{generation}/{terminal_file}",
            axum::routing::get(hls_proxy_terminal_segment),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/{segment_file}",
            axum::routing::get(hls_proxy_segment),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/map/{map_file}",
            axum::routing::get(hls_proxy_map),
        )
        .route(
            "/hls/shared/live/{proxy_session_id}/{hls_access_lease_id}/r/{resource_file}",
            axum::routing::get(hls_proxy_resource),
        )
        .route(
            "/hls/{username}/{password}/{target_id}/{input_id}/{stream_id}/{*token}",
            axum::routing::get(hls_api_stream),
        )
    //cfg.service(web::resource("/hls/{token}/{stream}").route(web::get().to(xtream_player_api_hls_stream)));
    //cfg.service(web::resource("/play/{token}/{type}").route(web::get().to(xtream_player_api_play_stream)));
}

#[cfg(test)]
mod tests;
