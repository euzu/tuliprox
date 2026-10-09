#![allow(clippy::wildcard_imports)]

use super::*;

pub(super) struct HlsCachedTransientManifestRead {
    pub(super) body: Arc<str>,
    pub(super) template: Arc<HlsTransientManifestTemplate>,
    pub(super) source_commit_identity: HlsManifestCommitIdentity,
    pub(super) window_policy: HlsManifestWindowPolicy,
    pub(super) finalized_manifest_generation: Option<TransientManifestGeneration>,
    published_resource_ids: HlsPublishedTransientResourceIds,
}

mod access;
mod admission;
mod cached;
mod canonical_owner;
mod provisioning;
mod publication;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use access::{
    hls_cache_configured, hls_cache_enabled_for_target, hls_cache_enabled_for_user,
    hls_manifest_preflight_refresh_ordering, hls_proxy_manifest, mark_hls_authorized_manifest_access,
    mark_hls_authorized_media_access, mark_successful_canonical_manifest_activity,
    resolve_hls_playback_manifest_request_context, HlsAccessManifestRequestContext,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use admission::{
    apply_hls_user_agent_stream_index, hls_initial_manifest_decision_wait_timeout, hls_initial_manifest_wait_timeout,
    hls_manifest_wait_timeout_for_requirement, prepare_hls_canonical_manifest_origin_runtime,
    touch_initial_manifest_access_lease_window, try_hls_cache_canonical_manifest_response,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use cached::{
    hls_bandwidth_persistence_outcome, hls_cached_manifest_temporarily_unavailable,
    observe_hls_lease_manifest_snapshot_derivation, read_hls_cached_manifest, spawn_hls_runtime_bandwidth_persistence,
    try_hls_cached_manifest_response, wait_for_hls_startup_evidence, HlsCachedManifestRead,
    HlsCachedManifestViewContext, HlsRuntimeBandwidthLearningContext,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use canonical_owner::{
    evaluate_hls_canonical_owner_handoff, finalize_hls_canonical_owner_handoff,
    hls_availability_reevaluation_registration_failure_response, hls_canonical_manifest_path,
    hls_canonical_owner_failed, hls_canonical_owner_lease_deadline_ms, hls_canonical_owner_registration,
    hls_canonical_owner_request_deadline_ms, hls_canonical_retry_after_response, hls_canonical_status_response,
    hls_direct_refresh_follow_up, join_hls_canonical_manifest_owner, trigger_hls_canonical_manifest_refresh,
    HlsCanonicalOwnerEvaluation, HlsCanonicalOwnerFailureReason, HlsCanonicalOwnerHandoffContext,
    HlsCanonicalOwnerPending, HlsCanonicalOwnerRegistration, HlsCanonicalOwnerRegistrationFailure,
    HlsCanonicalOwnerRegistrationKind, HlsCanonicalOwnerResolution, HlsManifestRefreshOrdering,
};
pub(in crate::api) use provisioning::hls_panel_provisioning_poll_manifest_response;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use provisioning::{
    clear_hls_provisioning_handoff_consumer, commit_shared_hls_provisioning_segments,
    ensure_shared_hls_provisioning_handoff_gap, hls_panel_provisioning_or_status_response,
    hls_panel_provisioning_poll_response, hls_provider_connections_exhausted_manifest_resolution,
    hls_shared_provisioning_or_provider_exhausted_response, hls_shared_provisioning_timeline_manifest_response,
    latest_shared_hls_manifest_rendered_at_ms, mark_hls_provisioning_handoff_discontinuity,
    mark_hls_provisioning_handoff_discontinuity_for_session,
    mark_hls_provisioning_handoff_discontinuity_once_for_session,
    maybe_mark_hls_provisioning_handoff_for_canonical_manifest, shared_hls_provisioning_segment_entry,
    shared_hls_provisioning_segment_plans, try_reserve_hls_entry_origin_account_for_redirect,
    try_reserve_hls_virtual_entry_origin_account_for_redirect, HlsEntryOriginAccountReservation,
    HlsProviderExhaustedResolution, HlsProvisioningPollResponseKind, SharedHlsProvisioningLocalSegmentKind,
    SharedHlsProvisioningSegmentPlan,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(super) use publication::{
    apply_hls_proxy_public_path_prefix, hls_access_manifest_uses_startup_view,
    hls_custom_video_type_for_failure_reason, hls_entry_master_playlist_response,
    hls_initial_strip_publication_diagnostic, materialize_hls_access_manifest, materialize_shared_hls_access_manifest,
    normalize_hls_proxy_public_path_prefix, split_hls_line_ending, HlsInitialStripLeaseSkipReason,
    HlsInitialStripPublicationDiagnostic, HlsInitialStripPublicationStatus, HlsMaterializedSharedManifest,
    HlsProxyTerminalSegmentPathParams,
};
pub(crate) use publication::{hls_admission_failure_manifest_response, hls_custom_video_manifest_response};
