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
            connection_priority_for_kind, create_api_proxy_user, create_m3u_catchup_session_key,
            create_playback_session_fingerprint, create_session_fingerprint, force_provider_stream_response,
            get_headers_from_request, get_hls_session_ttl_secs, get_stream_alternative_url,
            is_hls_stream_share_enabled, is_seekable_media_request, local_stream_response,
            record_connect_failed_attempt, resolve_playback_request_admission, try_option_bad_request, try_unwrap_body,
            ConnectFailedAttempt, EvictionReentryGuard, HeaderFilter,
        },
        model::{
            hls_cache::initial_strip::{
                materialize_initial_hls_strip_view, HlsInitialStripOutcome, HlsInitialStripSkipReason,
            },
            hls_custom_video_manifest_response_for_access_lease, hls_custom_video_manifest_response_with_virtual_id,
            hls_provisioning_discontinuity_sequence, hls_virtual_entry_redirect_response,
            is_custom_video_stream_enabled, start_hls_panel_provisioning_once,
            try_hls_panel_provisioning_manifest_response, AppState, ConnectionHistoryMode, CustomVideoStreamType,
            GraceMode, HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus, ProviderAllocation,
            ProviderConfig as RuntimeProviderConfig, ProviderHandle, StreamMeterHandle, TransportStreamBuffer,
            UserSession,
        },
        panel_api::can_provision_on_exhausted,
    },
    auth::{check_network_access_only, Fingerprint},
    model::{
        ConfigInput, ConfigInputFlags, ConfigProvider, ConfigTarget, InputSource, ProxyUserCredentials,
        ReverseProxyDisabledHeaderConfig,
    },
    processing::parser::hls::{
        get_hls_session_token_and_url_from_token, origin_manifest::HlsManifestWindowPolicy, rewrite_hls,
        RewriteHlsProps,
    },
    repository::{
        load_input_live_bitrate_bps, m3u_get_item_for_stream_id, persist_input_live_bitrate_bps, storage_const,
        xtream_get_item_for_stream_id, LiveBitratePersistenceOutcome,
    },
    utils::{content_coding::OutboundContentCodingPolicy, debug_if_enabled, request, request::is_file_url},
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
};
use futures::FutureExt;
use log::{debug, error, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::{
    defaults::HLS_EXT,
    model::{
        ConnectFailureReason, FailureStage, InputType, PlaylistEntry, PlaylistItemType, StreamChannel, StreamInfo,
        StreamProperties, TargetType, UserConnectionPermission, VirtualId, XtreamCluster,
    },
    utils::{
        extract_extension_from_url, generate_random_string, is_hls_url, is_m3u_catchup_session_token,
        replace_url_extension, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX,
    },
};
use std::{borrow::Cow, collections::HashMap, sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::{
    api::{
        begin_hls_origin_account_io_bounded, build_hls_origin_session_owner, build_proxy_session_id,
        cold_start_retry_after_seconds, commit_hls_runtime_custom_tail, derive_hls_lease_manifest_snapshot,
        extract_hls_provider_session_headers, fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
        fetch_hls_transient_origin_response_with_attempt_prepare, finite_hls_terminal_key_response,
        force_identity_without_range, hls_cached_manifest_options_for_requirement,
        hls_committed_manifest_body_for_request, hls_manifest_acceptance_directive_for_session,
        hls_manifest_commit_requirement, hls_object_body_deadline, hls_origin_account_status,
        hls_should_wait_for_initial_manifest_commit, hls_startup_admission_allows_snapshot,
        hls_transient_object_fetch_failure, hls_transient_origin_response, is_hls_provisioning_gap_segment,
        is_hls_provisioning_segment, maybe_trigger_origin_refresh_with_outcome, new_hls_access_lease_id,
        origin_account_binding_from_allocation, record_successful_transient_segment_fetch,
        record_temporary_transient_segment_fetch_failure, register_hls_availability_reevaluation,
        resolve_hls_transient_object_cache_action, safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key,
        safe_user_session_token, scrub_hls_origin_headers, serve_hls_map_cache_outcome,
        serve_hls_segment_cache_outcome, serve_hls_transient_object_cache_outcome,
        serve_hls_transient_object_cache_response, should_remove_hls_origin_header, trigger_origin_refresh_sync,
        validate_hls_access_lease, CacheAccessState, HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLease,
        HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeasePendingDeadline, HlsAccessLeaseState,
        HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsAccountBindingProtection,
        HlsAccountOverlapTiming, HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationRegistration,
        HlsBandwidthPersistenceOutcome, HlsBoundAccountAcquireErrorKind, HlsCacheResponseContext,
        HlsCachedManifestOptions, HlsCommittedManifestBody, HlsEffectiveOriginAcquirePolicy, HlsLeaseManifestSnapshot,
        HlsLeaseManifestSnapshotInput, HlsLeaseManifestUriMaterialization, HlsLeasePlaybackMode,
        HlsLeaseStartupAdmissionState, HlsLogIdentity, HlsManifestAcceptanceDirective,
        HlsManifestAcceptanceEvaluationOutcome, HlsManifestCommitIdentity, HlsManifestCommitRequirement,
        HlsManifestLimitViolation, HlsMapFile, HlsMasterBandwidth, HlsMasterBandwidthSelection,
        HlsMediaActivityCommitOutcome, HlsMediaActivityMarker, HlsMediaLeaseIdentity, HlsOriginAccountBinding,
        HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason, HlsOriginAccountStatus, HlsOriginIoContext,
        HlsOriginRefreshTriggerOutcome, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginSource,
        HlsOriginSourceKind, HlsOriginWorkClass, HlsPlaybackFamilyKey, HlsPostRefreshRuntime,
        HlsPublishedTransientResourceIds, HlsQosMeterInit, HlsQosRuntimeConfig, HlsResourceFetchAttempt,
        HlsResourceServeFailure, HlsResourceServeOutcome, HlsRuntimeCustomTailOutcome, HlsRuntimeCustomTailReason,
        HlsRuntimeCustomTailRequest, HlsSegmentFile, HlsSession, HlsSessionHandle, HlsSessionKey, HlsSessionMode,
        HlsSessionStoreOutcome, HlsSingleVariantMasterPlaylist, HlsTerminalFailedClosedReason, HlsTerminalSegmentPath,
        HlsTransientCacheCommitContext, HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext,
        HlsTransientManifestTemplate, HlsTransientObjectCacheAction, HlsTransientObjectFetchFailure,
        HlsTransientObjectFetchFinalizer, HlsTransientOriginCacheFetchRequest, HlsTransientOriginFetchRequest,
        HlsTransientOriginIoGuard, HlsTransientResourceLeaseContext, LiveHlsOriginEntry, OriginRefreshRequest,
        OriginSegmentKey, ProxySessionId, RetryPolicy, SegmentCacheKey, SegmentCacheStatus, SegmentDemandFetchOutcome,
        SegmentEntry, SegmentFetchContext, SegmentFetchPolicy, TransientManifestGeneration, TransientObjectFetchToken,
        TransientObjectUnavailableState, TransientPassthroughState, TransientResourceFile, TransientResourceId,
        TransientResourceRef, HLS_ACCESS_LEASE_ID_PLACEHOLDER, HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
        HLS_PROVISIONING_ORIGIN_EPOCH, HLS_PROVISIONING_SEGMENT_DURATION_MS, HLS_PROVISIONING_TARGET_DURATION_SECS,
        MAX_HLS_MANIFEST_BYTES,
    },
    HlsCtx, MAX_MANUAL_REDIRECTS,
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
mod segment;
mod session;

pub(in crate::api) use catchup::*;
pub(in crate::api) use manifest::*;
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
