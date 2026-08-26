mod app_state;
mod app_state_view;
mod hls_provisioning;
mod proxy;
mod streams;

#[cfg(test)]
pub(in crate::api) use self::hls_provisioning::{
    build_hls_custom_video_manifest_body, hls_panel_provisioning_manifest_path,
};
pub(in crate::api) use self::hls_provisioning::{
    hls_custom_video_manifest_response_for_access_lease, hls_custom_video_manifest_response_with_virtual_id,
    hls_provisioning_discontinuity_sequence, hls_virtual_entry_redirect_response,
    parse_hls_panel_provisioning_segment_route_name, start_hls_panel_provisioning_once,
    try_hls_panel_provisioning_manifest_response, HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus,
};
pub(crate) use self::streams::*;
pub use self::{app_state::*, app_state_view::*, hls_provisioning::HlsProvisioningState, proxy::*};
// Provider value types moved to `model`; re-exported so `api` keeps its names.
pub use crate::model::provider::*;
// Update semaphores moved to `model`; re-exported so `api` keeps its names.
pub use crate::model::update_guard::*;
pub use crate::model::{stream_error::*, update_task::*};
// In-memory playlist storage moved to `repository`; re-exported so `api` call
// sites keep their names.
pub use crate::repository::playlist_mem_cache::*;
// Dependency-free model types moved to `tuliprox-core`, and the provider
// response-header helpers to `tuliprox-session` beside the header types they
// operate on. Re-exported so `api` call sites keep their names.
pub use tuliprox_core::model::{batch_result_collector::*, user_api_request::*, xtream_response::*};
// HTTP range parsing moved to `tuliprox_core::utils`; re-exported so `api` call
// sites keep their names.
pub use tuliprox_core::utils::byte_range::{resolve_single_byte_range, SingleByteRange};
// The recording queue and the DVR moved to `tuliprox-dvr`; re-exported so `api`
// call sites keep their names, module paths included.
pub use tuliprox_dvr::{download, recording};
pub use tuliprox_dvr::{download::*, recording::*};
// Keep the crate alias while call sites migrate to its explicit `api` facade.
pub use tuliprox_hls as hls_cache;
// The HLS proxy symbols this crate's own tests reach through `api::model`.
// Explicit rather than a glob so the list stays a statement of what the tests
// actually need, and so pruning `tuliprox_hls::api` shows up here as a
// compile error instead of silently widening.
#[cfg(test)]
pub use tuliprox_hls::api::{
    begin_hls_origin_account_io, build_hls_origin_session_owner, build_hls_standalone_custom_plan,
    build_proxy_session_id, build_transient_resource_id, commit_terminal_tail_if_lease_reserve_requires_cutover,
    finish_hls_origin_account_io, hls_manifest_acceptance_directive_for_session, is_hls_provisioning_segment,
    is_hls_transient_full_object_cacheable_request, scrub_hls_origin_headers, trigger_origin_refresh_sync,
    CacheAccessState, HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsAvailabilityReevaluationRegistration,
    HlsEffectiveOriginAcquirePolicy, HlsFreshManifestRequiredReason, HlsLeasePlaybackMode, HlsLifecycleEvent,
    HlsLifecycleEventKey, HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome,
    HlsManifestCommitRequirement, HlsOriginAccountBinding, HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason,
    HlsOriginIoContext, HlsOriginSource, HlsOriginSourceKind, HlsPlaybackFamilyKey, HlsProxyManager,
    HlsRuntimeCustomTailReason, HlsSegmentFile, HlsSession, HlsSessionHandle, HlsSessionKey, HlsSessionMode,
    HlsSessionStoreOutcome, HlsStandaloneCustomAccess, HlsTerminalFailedClosedReason, HlsTerminalResolution,
    HlsTerminalSegmentPath, HlsTerminalTailPlan, HlsTerminalTailProtection, LiveHlsOriginEntry, MapCacheStatus,
    MapEntry, OriginMapKey, OriginRefreshRequest, OriginSegmentFetchRef, OriginSegmentKey, ProxyMapId, ProxySessionId,
    RenderedManifest, RetryPolicy, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPriority,
    TransientObjectCacheKey, TransientObjectCacheStatus, TransientPassthroughReason, TransientResourceId,
    TransientResourceKind, TransientResourceRef, HLS_ACCESS_LEASE_ID_PLACEHOLDER,
};
#[cfg(test)]
pub use tuliprox_hls::{
    build_terminal_tail_plan, prepare_terminal_base_evidence, prepared_terminal_bundle_key,
    snapshot_terminal_media_asset, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput,
    HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode, HlsBandwidthPersistenceState,
    HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestAcceptanceExhaustionReason,
    HlsManifestAcceptanceTrigger, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker, HlsMapSignature,
    HlsMediaContainer, HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsOriginPathCondition,
    HlsPreparedTerminalBundleState, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload,
    HlsRuntimeCustomTailAssetIdentity, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
    HlsTerminalBaseSegmentAvailability, HlsTerminalMediaAsset, HlsTerminalMediaPreparationState,
    HlsTerminalTailBuildInput, HlsTerminalTailCompatibility, HlsTerminalTailGeneration, HlsTransitionMarginMs,
    HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
// Background metadata resolution moved to `tuliprox-metadata`; re-exported so
// `api` call sites keep their names.
pub use tuliprox_metadata::{ctx::MetadataUpdateCtx, manager::*};
// Background workers moved to the layers they read: provider DNS and QoS
// aggregation to `tuliprox-session`, the playlist cache loader to
// `tuliprox-repository`. Re-exported so `api` call sites keep their names.
pub use tuliprox_repository::playlist_cache_loader::*;
// Provider allocation and the streaming-session runtime moved to
// `tuliprox-session`; re-exported so `api` call sites keep their names, module
// paths included.
pub use tuliprox_session::{
    active_provider_manager, active_user_manager, admission_strategy, connection_manager, event_manager, meter,
    provider_lineup_manager, stream,
};
pub use tuliprox_session::{
    active_provider_manager::*, active_user_manager::*, admission_strategy::*, connection_manager::*, event_manager::*,
    meter::*, provider_dns_manager::*, provider_lineup_manager::*, qos_aggregation_manager::*, response_headers::*,
    stream::*, streams::*,
};
