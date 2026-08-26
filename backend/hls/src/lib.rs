//! The HLS proxy.
//!
//! The proxy reads the running server through [`hls_ctx::HlsCtx`] - the
//! configuration, itself, provider allocation, connection admission and session
//! accounting - and names nothing else about it.
//!
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

// The `test-support` surface is compiled for *other* crates' tests. From inside
// this crate nothing calls it, so `dead_code` fires on every helper; the lint is
// relaxed only while that feature is on.
#![cfg_attr(feature = "test-support", allow(dead_code))]
// Auto-trait resolution for this crate's deeply nested async call chains
// exceeds the default 128-step recursion limit. Without this, rustc emits
// `recursion_depth_exceeding_limit`, which is on its way to becoming a hard
// error (rust-lang/rust#159228).
#![recursion_limit = "256"]

// The transient-manifest rewriter and the initial-strip pass moved here from
// `processing::parser`: unlike the rest of that parser they are written in terms
// of this module's transient-resource types.
mod availability;
mod availability_reevaluation;
mod backpressure;
mod cache;
mod critical_handoff;
mod cutover;
mod deadline;
mod deterministic_conflict;
mod hls_ctx;
pub mod initial_strip;
mod playback;
mod transient_manifest;

/// Redirect-following cap shared by manifest, resource and endpoint fetchers.
pub const MAX_MANUAL_REDIRECTS: usize = 10;
mod gc;
pub mod header_policy;
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
mod map;
mod map_fetcher;
mod master_playlist;
mod media_reserve;
mod observability;
mod origin;
mod origin_progress;
mod paths;
mod post_refresh_availability;
mod prefetch;
mod prepared_terminal_bundle;
mod qos;
mod recovery_timing;
mod refresh;
mod renderer;
mod resource_fetch;
mod resource_identity;
mod response;
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

#[cfg(any(test, feature = "test-support"))]
pub use self::availability_reevaluation::{HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode};
#[cfg(any(test, feature = "test-support"))]
pub use self::lease::HlsAccessLeaseDenialMode;
#[cfg(any(test, feature = "test-support"))]
pub use self::recovery_timing::{
    HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsObservedRecoveryLatency, HlsOperationTimeoutMs,
    HlsRecoveryEtaMs, HlsRecoveryTimingPolicy, HlsRecoveryWorkload, HlsTerminalMediaPreparationState,
    HlsTransitionMarginMs,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::terminal_tail::HlsTerminalTailGeneration;
/// Public contract consumed by the server application.
///
/// Every name here has at least one call site in `tuliprox`; the list is grouped
/// by the module that defines it, so a subsystem can be pruned or moved without
/// reading the whole facade. Nothing is kept for a hypothetical future consumer.
pub mod api {
    pub use super::{
        availability::{
            commit_terminal_tail_if_lease_reserve_requires_cutover, hls_manifest_acceptance_directive_for_session,
            hls_startup_admission_allows_snapshot, register_hls_availability_reevaluation,
            HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome, HlsTerminalFailedClosedReason,
            HlsTerminalResolution,
        },
        availability_reevaluation::{HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationRegistration},
        cache::{SegmentCacheKey, TransientObjectCacheKey},
        deadline::hls_object_body_deadline,
        gc::exec_hls_cache_gc,
        headers::{
            extract_hls_provider_session_headers, force_identity_without_range, scrub_hls_origin_headers,
            should_remove_hls_origin_header,
        },
        ids::{build_proxy_session_id, HlsSessionKey, ProxySessionId, HLS_ACCESS_LEASE_ID_PLACEHOLDER},
        lease::{
            new_hls_access_lease_id, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId,
            HlsAccessLeasePendingDeadline, HlsAccessLeaseState, HlsAccessLeaseTiming, HlsAccessLeaseTouch,
            HlsFreshManifestRequiredReason, HlsLeaseStartupAdmissionState, HlsMediaLeaseIdentity, HlsPlaybackFamilyKey,
        },
        lifecycle::{HlsLifecycleEvent, HlsLifecycleEventKey},
        manager::{exec_hls_lifecycle, HlsMediaActivityCommitOutcome, HlsProxyManager},
        manifest_commit::{
            hls_cached_manifest_options_for_requirement, hls_committed_manifest_body_for_request,
            hls_manifest_commit_requirement, hls_should_wait_for_initial_manifest_commit, HlsCachedManifestOptions,
            HlsCommittedManifestBody,
        },
        manifest_fetch::{LiveHlsOriginEntry, RetryPolicy, MAX_HLS_MANIFEST_BYTES},
        manifest_snapshot::{derive_hls_lease_manifest_snapshot, HlsLeaseManifestSnapshotInput},
        map::{MapCacheStatus, MapEntry, OriginMapKey, ProxyMapId},
        master_playlist::{
            HlsBandwidthPersistenceOutcome, HlsMasterBandwidth, HlsMasterBandwidthSelection,
            HlsSingleVariantMasterPlaylist,
        },
        observability::{
            log_hls_origin_content_coding, safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key,
            safe_user_session_token, HlsLogIdentity, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource,
        },
        origin::{
            begin_hls_origin_account_io, begin_hls_origin_account_io_bounded, build_hls_origin_session_owner,
            finish_hls_origin_account_io, hls_origin_account_status, origin_account_binding_from_allocation,
            HlsAccountBindingProtection, HlsAccountOverlapTiming, HlsBoundAccountAcquireErrorKind,
            HlsEffectiveOriginAcquirePolicy, HlsOriginAccountBinding, HlsOriginAccountBindingMode,
            HlsOriginAccountDetachedReason, HlsOriginAccountStatus, HlsOriginIoContext, HlsOriginSource,
            HlsOriginSourceKind, HlsOriginWorkClass,
        },
        origin_progress::publication_late_after_ms,
        paths::{HlsMapFile, HlsSegmentFile, TransientResourceFile},
        playback::{
            validate_hls_access_lease, HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLeaseValidationError,
        },
        prefetch::SegmentFetchPriority,
        qos::{HlsQosMeterInit, HlsQosRuntimeConfig},
        refresh::{
            cold_start_retry_after_seconds, maybe_trigger_origin_refresh_with_outcome, trigger_origin_refresh_sync,
            HlsManifestCommitRequirement, HlsOriginRefreshTriggerOutcome, HlsPostRefreshRuntime, OriginRefreshRequest,
        },
        renderer::RenderedManifest,
        resource_fetch::{
            retry_after_secs_from_ms, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsResourceFetchAttempt,
        },
        response::{
            finite_hls_immutable_media_response, finite_hls_media_head_response, finite_hls_media_response,
            finite_hls_terminal_key_response, serve_hls_map_cache_outcome, serve_hls_segment_cache_outcome,
            serve_hls_transient_object_cache_outcome, serve_hls_transient_object_cache_response,
            HlsCacheResponseContext, HlsMediaActivityMarker, HlsResourceServeFailure, HlsResourceServeOutcome,
        },
        runtime_custom_tail::{
            build_hls_standalone_custom_plan, commit_hls_runtime_custom_tail, resolve_hls_standalone_custom_segment,
            HlsRuntimeCustomTailOutcome, HlsRuntimeCustomTailReason, HlsRuntimeCustomTailRequest,
            HlsStandaloneCustomAccess, HlsStandaloneCustomSegmentAccess, HlsStandaloneCustomSegmentError,
        },
        segment_fetcher::{SegmentDemandFetchOutcome, SegmentFetchContext, SegmentFetchPolicy},
        session::{HlsSession, HlsSessionMode, HlsTerminalTailProtection, TransientPassthroughReason},
        session_store::{HlsSessionHandle, HlsSessionStoreOutcome},
        terminal_tail::{
            terminal_tail_manifest_body, HlsLeasePlaybackMode, HlsTerminalSegmentPath, HlsTerminalTailPlan,
        },
        timeline::{
            is_hls_provisioning_gap_segment, is_hls_provisioning_segment, CacheAccessState, OriginSegmentFetchRef,
            OriginSegmentKey, SegmentCacheStatus, SegmentEntry, HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
            HLS_PROVISIONING_ORIGIN_EPOCH, HLS_PROVISIONING_SEGMENT_DURATION_MS, HLS_PROVISIONING_TARGET_DURATION_SECS,
        },
        transient::{
            build_transient_resource_id, TransientObjectCacheStatus, TransientObjectFetchToken,
            TransientObjectUnavailableState, TransientPassthroughState, TransientResourceId, TransientResourceKind,
            TransientResourceRef,
        },
        transient_fetcher::{
            fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
            fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
            hls_transient_origin_response, is_hls_transient_full_object_cacheable_request,
            record_successful_transient_segment_fetch, record_temporary_transient_segment_fetch_failure,
            resolve_hls_transient_object_cache_action, HlsTransientCacheCommitContext,
            HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext, HlsTransientObjectCacheAction,
            HlsTransientObjectFetchFailure, HlsTransientObjectFetchFinalizer, HlsTransientOriginCacheFetchRequest,
            HlsTransientOriginFetchRequest, HlsTransientOriginIoGuard,
        },
    };
}
// The crate-root names the modules in this crate use to reach each other.
//
// This is not the public API - that is `api` below, which re-exports from the
// same modules for the server application. Keeping the two lists apart means
// pruning one does not silently break the other, which a `pub(crate) use api::*`
// could not give us.
pub(crate) use self::{
    availability::{hls_recovery_timing_policy, HlsManifestAcceptanceDirective, HLS_PLAYBACK_RATE_GUARD_MILLI},
    availability_reevaluation::HlsAvailabilityReevaluationRegistration,
    backpressure::{classify_hls_backpressure, HlsBackpressureState},
    cache::{
        CacheInvalidationOutcome, CachedSegmentMetadata, HlsCacheCapacityReclaimOutcome,
        HlsCacheCapacityReclaimRequest, HlsCacheCapacityReclaimer, HlsCacheCapacityRevision, HlsCacheObjectKey,
        HlsSegmentCache, MapCacheKey, SegmentCacheKey, StagedCacheObject, TransientObjectCacheKey,
    },
    deadline::{hls_client_body_send_deadline, hls_object_body_deadline, refresh_hls_client_body_send_deadline},
    gc::{
        build_rewrite_secret_fingerprint, GarbageCollectionPolicy, GarbageCollectionReport, HlsGarbageCollector,
        ProtectedSet,
    },
    headers::{
        append_hls_provider_session_headers, extract_hls_provider_session_header_map, force_identity_without_range,
        hls_origin_headers_with_provider_session, sanitized_hls_origin_headers, scrub_hls_origin_headers,
    },
    ids::{build_proxy_session_id, HlsSessionKey, ProxySessionId, HLS_ACCESS_LEASE_ID_PLACEHOLDER},
    lease::{
        HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeaseLifecycleSnapshot,
        HlsAccessLeasePendingDeadline, HlsAccessLeaseSessionSnapshot, HlsAccessLeaseState, HlsAccessLeaseStore,
        HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsFreshManifestRequiredReason, HlsMediaLeaseIdentity,
        HlsPlaybackFamilyKey,
    },
    lifecycle::{HlsLifecycleEvent, HlsLifecycleEventKey, HlsLifecycleManager},
    manager::{HlsCriticalHandoffStateAccess, HlsMediaActivityCommitOutcome, HlsProxyManager},
    map::{MapCacheStatus, MapEntry, OriginMapFetchRef, OriginMapKey, ProxyMapId},
    map_fetcher::{HlsMapWorkerPool, MapFetchContext},
    media_reserve::{
        evaluate_lease_reserve, HlsLeaseReserveInput, HlsPlaybackCompletionOutcome, HlsPlaybackRequestToken,
    },
    observability::{
        hls_manifest_recovery_log_fields, hls_origin_log_value, log_hls_origin_content_coding,
        safe_hls_access_lease_id, safe_proxy_session_id, safe_session_key, safe_user_session_token, HlsCacheMetrics,
        HlsLogIdentity, HlsOriginContentCodingObjectKind, HlsOriginContentCodingSource, HlsRecoveryTriggerDiagnostic,
        HlsRecoveryTriggerSource,
    },
    origin::{
        begin_hls_origin_account_io, begin_hls_origin_account_io_bounded, classify_account_binding_protection,
        finish_hls_origin_account_io, HlsAccountBindingProtection, HlsAccountOverlapTiming,
        HlsBoundAccountAcquireErrorKind, HlsEffectiveOriginAcquirePolicy, HlsEffectiveOriginAcquirePolicyState,
        HlsOriginAccountBinding, HlsOriginAccountIoLease, HlsOriginAccountIoLeaseGuard, HlsOriginAccountRebindState,
        HlsOriginIoContext, HlsOriginSource, HlsOriginWorkClass,
    },
    paths::{HlsMapFile, HlsSegmentFile, TransientResourceFile},
    prefetch::{SegmentFetchPriority, SegmentPrefetchQueue},
    qos::HlsQosRegistry,
    refresh::{HlsManifestCommitRequirement, OriginRefreshState},
    renderer::{
        renderer_candidate_window_proxy_seqs, HlsManifestRenderer, RenderPolicy, RenderedManifest,
        RenderedManifestStoreOutcome, RenderedManifestStoreRejectReason,
    },
    resource_fetch::{
        build_hls_origin_resource_headers, build_hls_origin_resource_headers_with_client_range,
        retry_after_secs_from_ms, run_hls_origin_resource_retry_loop_with_attempt_prepare,
        HlsOriginByteRangeExpectation, HlsOriginResourceBodyDeadline, HlsOriginResourceClients,
        HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsResourceFetchAttempt, HlsResourceFetchKind,
        HlsResourceFetchSource,
    },
    segment_fetcher::{HlsSegmentWorkerPool, SegmentFetchContext, SegmentFetchPolicy},
    segment_repair::{
        HlsRepairRenderedObjectId, HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource,
    },
    session::{
        HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSession, HlsSessionMode, HlsTerminalTailProtection,
        HlsTerminalTailProtectionInstall, HlsTerminalTailProtectionRemoval, TransientPassthroughReason,
    },
    session_store::{
        HlsExpiredSessionMarker, HlsExpiredSessionReason, HlsSessionHandle, HlsSessionStore, HlsSessionStoreOutcome,
    },
    startup_observability::{HlsStartupBodyObservation, HlsStartupObservability},
    timeline::{
        is_hls_provisioning_gap_segment, is_hls_provisioning_segment, CacheAccessState, HlsSegmentEncryption,
        OriginSegmentFetchRef, OriginSegmentKey, SegmentCacheStatus, SegmentEntry, TimelineMapError,
        HLS_PROVISIONING_TARGET_DURATION_SECS,
    },
    transient::{
        TransientObjectCacheStatus, TransientObjectFetchDecision, TransientObjectFetchToken,
        TransientObjectUnavailableState, TransientPassthroughState, TransientResourceId, TransientResourceKind,
        TransientResourceRef, TransientResourceStore,
    },
    transient_fetcher::{
        fetch_hls_transient_origin_response_with_attempt_prepare, HlsTransientObjectFetchFinalizer,
        HlsTransientOriginFetchRequest,
    },
};
#[cfg(any(test, feature = "test-support"))]
pub use self::{
    manifest_acceptance::{HlsManifestAcceptanceExhaustionReason, HlsManifestAcceptanceTrigger},
    master_playlist::HlsBandwidthPersistenceState,
    media_reserve::{
        HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestDeliveryMode, HlsManifestSourceRenderMarker,
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
pub use hls_ctx::{HlsCtx, WeakHlsCtx};
pub use tuliprox_mpegts::ts_inspector::{
    evaluate_mpeg_ts_splice_boundary, hls_aes128_cbc_iv, inspect_mpeg_ts_async, inspect_mpeg_ts_media_evidence_async,
    HlsTrackEvidenceResolution, HlsTsMediaEvidence, HlsTsProbeBudget, HlsTsProbeProtection, HlsTsProtectionReason,
    HlsTsSpliceBoundaryIncompatibility, HlsTsSpliceEvidence, HlsTsSpliceIncompatibility, HlsTsTrackSignature,
};
