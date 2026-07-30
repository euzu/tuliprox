//! HLS shared-session cache: origin fetcher, segment/map/transient/manifest stores,
//! access-lease protocol, GC, observability, and origin-request header policy.
//!
//! # Subsystem map (29 flat files → 7 logical groups)
//!
//! The flat layout below predates the cache state machine's growth; many files
//! span concerns (e.g. `paths.rs`/`deadline.rs`/`ids.rs` are pure infra).
//! The natural cohesion boundaries are:
//!
//! | Group        | Files (current name → natural home)                                                |
//! |--------------|------------------------------------------------------------------------------------|
//! | `session`    | `session`, `session_store`, `lifecycle`, `ids` (session-token helpers)             |
//! | `segment`    | `segment_fetcher`, `segment_repair`, `segment_watchdog`                            |
//! | `map`        | `map`, `map_fetcher`                                                               |
//! | `manifest`   | `manifest_commit`, `manifest_fetch`, `transient`, `transient_fetcher`              |
//! | `lease`      | `lease`                                                                            |
//! | `gc`         | `gc`                                                                               |
//! | `infra`      | `ids` (token types), `deadline`, `paths`, `headers` (now via `proxy::header_policy`), `backpressure`, `observability`, `timeline`, `qos`, `cache`, `manager`, `refresh`, `renderer`, `response`, `prefetch`, `origin`, `playback`, `resource_fetch` |
//!
//! `proxy/header_policy` already exists as a cross-proxy module (see
//! `api::model::proxy::header_policy::HopByHopHeader`); `headers.rs` is now a
//! thin delegator. The remaining 28 files are intentionally left in place
//! because the cost of moving them (imports, mod.rs churn, public-API re-exports)
//! outweighs the discoverability gain at this commit. Each subsystem
//! migration is a self-contained follow-up PR.
//!
//! Migration order (lowest risk first):
//!   1. `infra` (only `proxy::header_policy` already done; rest stay flat)
//!   2. `lease`, `gc` (each one self-contained today)
//!   3. `map` (one fetcher, one store)
//!   4. `manifest` + `segment` (share transient types; do together)
//!   5. `session` last (largest blast radius; touches lifecycle, store, ids)
//!
//! Until the move lands, treat the table above as the canonical "where do I
//! put this?" map. New files should land in the natural group, not the flat
//! layout.

mod availability;
mod availability_reevaluation;
mod backpressure;
mod cache;
mod critical_handoff;
mod cutover;
mod deadline;
mod deterministic_conflict;
mod gc;
mod headers;
mod ids;
mod lease;
mod lifecycle;
mod manager;
mod manifest_acceptance;
mod manifest_commit;
mod manifest_fetch;
mod manifest_origin_binding;
mod manifest_snapshot;
mod master_playlist;
mod map;
mod map_fetcher;
mod media_reserve;
mod observability;
mod origin;
mod origin_progress;
mod paths;
mod playback;
mod post_refresh_availability;
mod prefetch;
mod prepared_terminal_bundle;
mod qos;
mod recovery_timing;
mod refresh;
mod renderer;
mod resource_fetch;
mod response;
mod resource_identity;
mod runtime_custom_tail;
mod segment_fetcher;
mod segment_repair;
mod segment_watchdog;
mod session;
mod session_store;
mod startup_observability;
mod terminal_commit;
mod terminal_pending;
mod terminal_tail;
mod timeline;
mod transient;
mod transient_fetcher;
mod ts_inspector;

#[cfg(test)]
pub(crate) use self::recovery_timing::{
    HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsObservedRecoveryLatency, HlsOperationTimeoutMs,
    HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsTerminalMediaPreparationState,
    HlsTransitionMarginMs,
};
#[cfg(test)]
pub(crate) use self::terminal_tail::HlsTerminalTailGeneration;
#[cfg(test)]
pub(crate) use self::lease::HlsAccessLeaseDenialMode;
#[cfg(test)]
pub(crate) use self::availability_reevaluation::{
    HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode,
};
pub(crate) use self::{
    availability::{
        commit_terminal_tail_if_lease_reserve_requires_cutover, hls_manifest_acceptance_directive_for_session,
        hls_recovery_timing_policy, hls_startup_admission_allows_snapshot,
        register_hls_availability_reevaluation, HlsManifestAcceptanceDirective,
        HlsManifestAcceptanceEvaluationOutcome, HlsTerminalFailedClosedReason, HlsTerminalResolution,
        HLS_PLAYBACK_RATE_GUARD_MILLI,
    },
    availability_reevaluation::{
        HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationRegistration,
    },
    lease::{HlsLeaseStartupAdmissionState, HlsMediaLeaseIdentity},
    manager::{HlsCriticalHandoffStateAccess, HlsMediaActivityCommitOutcome},
    manifest_fetch::MAX_HLS_MANIFEST_BYTES,
    manifest_snapshot::{derive_hls_lease_manifest_snapshot, HlsLeaseManifestSnapshotInput},
    master_playlist::{
        HlsBandwidthPersistenceOutcome, HlsMasterBandwidth, HlsMasterBandwidthSelection,
        HlsSingleVariantMasterPlaylist,
    },
    media_reserve::{
        evaluate_lease_reserve, HlsLeaseReserveInput, HlsPlaybackCompletionOutcome, HlsPlaybackRequestToken,
    },
    observability::{
        hls_manifest_recovery_log_fields, log_hls_origin_content_coding, HlsLogIdentity,
        HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource, HlsRecoveryTriggerDiagnostic,
        HlsRecoveryTriggerSource,
    },
    origin_progress::publication_late_after_ms,
    response::{
        finite_hls_immutable_media_response, finite_hls_media_head_response, finite_hls_media_response,
        finite_hls_terminal_key_response,
    },
    startup_observability::{HlsStartupBodyObservation, HlsStartupObservability},
    playback::{validate_hls_access_lease, HlsAccessLeaseValidationError},
    refresh::{maybe_trigger_origin_refresh_with_outcome, HlsOriginRefreshTriggerOutcome, HlsPostRefreshRuntime},
    runtime_custom_tail::{
        build_hls_standalone_custom_plan, commit_hls_runtime_custom_tail,
        resolve_hls_standalone_custom_segment, HlsRuntimeCustomTailOutcome,
        HlsRuntimeCustomTailReason, HlsRuntimeCustomTailRequest, HlsStandaloneCustomAccess,
        HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError,
    },
    session::{HlsTerminalTailProtection, HlsTerminalTailProtectionInstall, HlsTerminalTailProtectionRemoval},
    terminal_tail::{
        terminal_tail_manifest_body, HlsLeasePlaybackMode, HlsTerminalSegmentPath, HlsTerminalTailPlan,
    },
    timeline::TimelineMapError,
    ts_inspector::{
        evaluate_mpeg_ts_splice_boundary, hls_aes128_cbc_iv, inspect_mpeg_ts, inspect_mpeg_ts_async,
        inspect_mpeg_ts_media_evidence_async, HlsTrackEvidenceResolution, HlsTsMediaEvidence,
        HlsTsProbeBudget, HlsTsProbeOutcome, HlsTsProbeProtection, HlsTsProtectionReason,
        HlsTsSpliceBoundaryIncompatibility, HlsTsSpliceEvidence, HlsTsSpliceIncompatibility,
        HlsTsTrackSignature,
    },
};
pub use self::{
    backpressure::{classify_hls_backpressure, HlsBackpressureState},
    cache::{
        CacheInvalidationOutcome, CachedSegmentMetadata, HlsCacheObjectKey, HlsSegmentCache, MapCacheKey,
        SegmentCacheKey, StagedCacheObject, TransientObjectCacheKey, DEFAULT_HLS_CACHE_DURATION_SECS,
        DEFAULT_HLS_CACHE_PATH,
    },
    deadline::{hls_client_body_send_deadline, hls_object_body_deadline, refresh_hls_client_body_send_deadline},
    gc::{
        build_rewrite_secret_fingerprint, exec_hls_cache_gc, GarbageCollectionPolicy, GarbageCollectionReport,
        HlsGarbageCollector, ProtectedSet,
    },
    headers::{
        append_hls_provider_session_headers, extract_hls_provider_session_header_map,
        extract_hls_provider_session_headers, force_identity_without_range, hls_origin_headers_with_provider_session,
        sanitized_hls_origin_headers, scrub_hls_origin_headers, should_remove_hls_origin_header,
    },
    ids::{build_proxy_session_id, HlsSessionKey, ProxySessionId},
    lease::{
        new_hls_access_lease_id, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeaseIdleRelease,
        HlsAccessLeaseLifecycleSnapshot, HlsAccessLeasePendingDeadline, HlsAccessLeaseSessionSnapshot,
        HlsAccessLeaseState, HlsAccessLeaseStore, HlsAccessLeaseTiming, HlsAccessLeaseTouch,
        HlsFreshManifestRequiredReason, HlsPlaybackFamilyKey,
    },
    lifecycle::{HlsLifecycleEvent, HlsLifecycleEventKey, HlsLifecycleManager},
    manager::{exec_hls_lifecycle, HlsProxyManager},
    manifest_commit::{
        hls_cached_manifest_options_for_requirement, hls_committed_manifest_body_for_request,
        hls_manifest_commit_requirement, hls_should_wait_for_initial_manifest_commit, HlsCachedManifestOptions,
        HlsCommittedManifestBody,
    },
    manifest_fetch::{classify_origin_manifest_status, LiveHlsOriginEntry, OriginManifestStatusClass, RetryPolicy},
    map::{MapCacheStatus, MapEntry, OriginMapFetchRef, OriginMapKey, ProxyMapId},
    map_fetcher::{HlsMapWorkerPool, MapFetchContext},
    observability::{
        hls_origin_log_value, safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key,
        safe_user_session_token, HlsCacheMetrics, HlsCacheMetricsSnapshot,
    },
    origin::{
        acquire_bound_hls_origin_account_handle, begin_hls_origin_account_io, begin_hls_origin_account_io_bounded,
        build_hls_origin_session_owner, classify_account_binding_protection, finish_hls_origin_account_io,
        finish_hls_origin_io, hls_origin_account_status, origin_account_binding_from_allocation, safe_hls_origin_owner,
        HlsAccountBindingProtection, HlsAccountOverlapTiming, HlsBoundAccountAcquireErrorKind,
        HlsEffectiveOriginAcquirePolicy, HlsEffectiveOriginAcquirePolicyState, HlsOriginAccountBinding,
        HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason, HlsOriginAccountIoLease,
        HlsOriginAccountIoLeaseGuard, HlsOriginAccountRebindState, HlsOriginAccountStatus, HlsOriginIoContext,
        HlsOriginSource, HlsOriginSourceKind, HlsOriginWorkClass,
    },
    paths::{HlsMapFile, HlsSegmentFile, TransientResourceFile},
    playback::{HlsAccessAdmissionMode, HlsAccessContext, HLS_ACCESS_LEASE_ID_PLACEHOLDER},
    prefetch::{ManifestFetchQueueReport, SegmentFetchPriority, SegmentPrefetchQueue},
    qos::{HlsQosMeterInit, HlsQosRegistration, HlsQosRegistry, HlsQosRuntimeConfig},
    refresh::{
        cold_start_retry_after_seconds, maybe_trigger_origin_refresh, trigger_origin_refresh_sync,
        HlsManifestCommitRequirement, OriginRefreshRequest, OriginRefreshState,
    },
    renderer::{
        renderer_candidate_window_proxy_seqs, HlsManifestRenderer, RenderError, RenderPolicy, RenderedManifest,
        RenderedManifestStoreOutcome, RenderedManifestStoreRejectReason,
    },
    resource_fetch::{
        build_hls_origin_resource_headers, build_hls_origin_resource_headers_with_client_range,
        retry_after_secs_from_ms, run_hls_origin_resource_retry_loop_with_attempt_prepare,
        HlsOriginByteRangeExpectation, HlsOriginResourceBodyDeadline, HlsOriginResourceClients,
        HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsResourceFetchAttempt, HlsResourceFetchKind,
        HlsResourceFetchSource,
    },
    response::{
        serve_hls_map_cache_outcome, serve_hls_segment_cache_outcome, serve_hls_transient_object_cache_outcome,
        serve_hls_transient_object_cache_response, HlsCacheResponseContext, HlsMediaActivityMarker,
        HlsResourceServeFailure, HlsResourceServeOutcome,
    },
    segment_fetcher::{HlsSegmentWorkerPool, SegmentDemandFetchOutcome, SegmentFetchContext, SegmentFetchPolicy},
    segment_repair::{
        parse_ffmpeg_warnings, HlsRepairRenderedObjectId, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
        HlsSegmentRepairSource, WarningCounters,
    },
    session::{
        HlsSegmentFailureObject, HlsSegmentFailureTracker, HlsSegmentFailureTransition, HlsSession, HlsSessionActivity,
        HlsSessionMode, TransientPassthroughReason,
    },
    session_store::{
        HlsExpiredSessionMarker, HlsExpiredSessionReason, HlsSessionHandle, HlsSessionStore, HlsSessionStoreOutcome,
    },
    timeline::{
        default_content_type_for_segment_ext, is_hls_provisioning_gap_segment, is_hls_provisioning_segment,
        CacheAccessState, HlsSegmentEncryption, OriginSegmentFetchRef, OriginSegmentKey, SegmentCacheStatus,
        SegmentEntry, HLS_PROVISIONING_GAP_ORIGIN_EPOCH, HLS_PROVISIONING_ORIGIN_EPOCH,
        HLS_PROVISIONING_SEGMENT_DURATION_MS, HLS_PROVISIONING_TARGET_DURATION_SECS,
    },
    transient::{
        build_transient_resource_id, TransientObjectCacheEntry, TransientObjectCacheStatus,
        TransientObjectFetchDecision, TransientObjectFetchToken, TransientObjectRemoval,
        TransientObjectUnavailableState, TransientPassthroughState, TransientResourceId, TransientResourceKind,
        TransientResourceRef, TransientResourceStore,
    },
    transient_fetcher::{
        fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
        fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
        hls_transient_origin_response, hls_transient_resource_fetch_kind,
        is_hls_transient_full_object_cacheable_request, record_successful_transient_segment_fetch,
        record_temporary_transient_segment_fetch_failure, resolve_hls_transient_object_cache_action,
        HlsTransientCacheCommitContext, HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext,
        HlsTransientObjectCacheAction, HlsTransientObjectCacheResolution, HlsTransientObjectFetchFailure,
        HlsTransientObjectFetchFinalizer, HlsTransientOriginCacheFetchRequest, HlsTransientOriginFetchRequest,
        HlsTransientOriginIoGuard,
    },
};

pub(crate) use self::cache::{
    HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest, HlsCacheCapacityReclaimer,
    HlsCacheCapacityRevision,
};
#[cfg(test)]
pub(crate) use self::{
    manifest_acceptance::{HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceTrigger},
    master_playlist::HlsBandwidthPersistenceState,
    media_reserve::{
        HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeaseReserveAvailabilityBasis, HlsLeaseReserveSnapshot,
        HlsManifestDeliveryMode, HlsManifestSourceRenderMarker,
    },
    origin_progress::HlsOriginPathCondition,
    prepared_terminal_bundle::{prepared_terminal_bundle_key, HlsPreparedTerminalBundleState},
    runtime_custom_tail::HlsRuntimeCustomTailAssetIdentity,
    terminal_tail::{
        build_terminal_tail_plan, prepare_terminal_base_evidence, snapshot_terminal_media_asset, HlsMapSignature,
        HlsMediaContainer, HlsTerminalAssetIdentity, HlsTerminalBaseMediaState, HlsTerminalBaseProtection,
        HlsTerminalBaseSegmentAvailability, HlsTerminalMediaAsset, HlsTerminalTailBuildInput,
        HlsTerminalTailCompatibility, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
};
