//! Moving a session onto a different origin manifest without breaking playback.
//!
//! A switch is staged before it is committed: the candidate's media is fetched
//! into the cache under a staging generation, its content anchor is verified
//! against what the session already published, and only then is it committed.
//! Anything left behind by a switch that did not commit is removed here too.
//!
//! The critical-handoff path is the emergency form of the same thing - it
//! verifies an already-prepared handoff rather than staging a new one - so it
//! shares the readiness and compatibility checks and lives beside them.

use super::{
    commit::{materialize_normal_key_resources, HlsCommittedAcceptanceReadPin},
    current_time_millis,
    startup_revision::{complete_startup_revision, startup_revision_kind},
    OriginRefreshRequest,
};
use crate::{
    critical_handoff::{
        critical_handoff_snapshot_is_current as critical_handoff_snapshot_matches_generation,
        select_critical_handoff_lease as select_critical_handoff_lease_for_generation,
        terminal_alternative_compatibility_for_critical_lease, HlsCriticalHandoffSnapshot,
    },
    manifest_acceptance::{
        HlsManifestAcceptanceGeneration, HlsManifestAcceptanceState, HlsManifestRecoveryCandidateIdentity,
        HlsRecoveryWorkloadBindingUpdate, HlsTerminalAlternativeCompatibility,
    },
    manifest_fetch::{
        fetched_effective_manifest_host, FetchedOriginManifest, HlsManifestCommitError, HlsManifestRejectLogReason,
    },
    recovery_timing::{
        HlsRecoveryEncryptionReadiness, HlsRecoveryMediumReadiness, HlsRecoveryObjectReadiness, HlsRecoveryWorkload,
    },
    resource_identity::HlsMediaResourceIdentity,
    safe_session_key,
    segment_fetcher::{stage_hls_switch_resource, HlsSwitchResourceKind, SegmentFetchPolicy},
    sync_ext::MutexExt,
    terminal_tail::{
        pin_terminal_base_evidence, resolve_terminal_base_evidence, snapshot_terminal_media_asset,
        HlsTerminalBaseEvidence, HlsTerminalMediaAsset,
    },
    timeline::{effective_origin_host_id, HlsOriginHandoffPreview, HlsOriginHandoffPreviewError, HlsRetainedGapRepair},
    HlsSegmentCache, HlsSessionMode, HlsTrackEvidenceResolution, MapCacheStatus, MapEntry, SegmentCacheKey,
    SegmentCacheStatus, SegmentEntry, SegmentFetchContext, StagedCacheObject, TransientResourceKind,
    TransientResourceRef,
};
use log::{debug, warn};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::Instant,
};
use tuliprox_core::model::CustomStreamResponse;
use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HlsSwitchStagingGeneration {
    pub(super) acceptance_generation: HlsManifestAcceptanceGeneration,
    pub(super) candidate_identity: HlsManifestRecoveryCandidateIdentity,
    pub(super) progress_generation: u64,
    pub(super) pinned_host: Option<String>,
    pub(super) origin_epoch: u64,
    pub(super) origin_seq_highwater: Option<u64>,
    pub(super) proxy_next_seq: Option<u64>,
    origin_work_generation: u64,
    origin_account_generation: u64,
}

pub(super) struct HlsStagedSwitchCommit {
    pub(super) generation: HlsSwitchStagingGeneration,
    pub(super) effective_host_id: u64,
    pub(super) preview: HlsOriginHandoffPreview,
    pub(super) ready_segment_proxy_seq: u64,
    pub(super) ready_segment_content_length: u64,
    pub(super) ready_segment_origin_digest: [u8; 32],
    pub(super) content_anchor: Option<HlsVerifiedContentAnchor>,
    pub(super) ready_map: Option<(crate::ProxyMapId, u64)>,
    pub(super) ready_segment_revision: Option<crate::SegmentRevisionGuard>,
    pub(super) segment_cleanup: crate::gc::HlsSwitchCacheCleanupReservation,
    pub(super) map_cleanup: Option<crate::gc::HlsSwitchCacheCleanupReservation>,
    pub(super) critical_handoff: Option<HlsCriticalHandoffPreparation>,
}

impl HlsStagedSwitchCommit {
    pub(super) fn disarm_cleanup(&mut self) {
        self.segment_cleanup.disarm();
        if let Some(cleanup) = self.map_cleanup.as_mut() {
            cleanup.disarm();
        }
    }
}

pub(super) struct HlsCriticalHandoffPreparation {
    pub(super) generation: HlsSwitchStagingGeneration,
    lease: crate::HlsAccessLease,
    pub(super) snapshot: HlsCriticalHandoffSnapshot,
    pub(super) base_evidence: HlsTerminalBaseEvidence,
    pub(super) terminal_response: Option<Arc<CustomStreamResponse>>,
    terminal_asset: Option<Arc<HlsTerminalMediaAsset>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsStagedSwitchMediaCompatibility {
    Compatible,
    RequiresUnstagedEncryptionKey,
    CannotResetActiveMap,
}

pub(super) fn switch_staging_error(reason: HlsManifestRejectLogReason) -> HlsManifestCommitError {
    HlsManifestCommitError::TimelineRejected { reason }
}

pub(super) fn handoff_preview_error(error: HlsOriginHandoffPreviewError) -> HlsManifestCommitError {
    match error {
        HlsOriginHandoffPreviewError::Timeline(error) => switch_staging_error(error.into()),
        HlsOriginHandoffPreviewError::PreviewInconsistent => {
            switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated)
        }
    }
}

pub(super) fn switch_staging_generation(session: &crate::HlsSession) -> Option<HlsSwitchStagingGeneration> {
    let episode = session.origin_control.acceptance_episode.as_ref()?;
    if !episode.full_burst_completed || episode.state != HlsManifestAcceptanceState::StagingSwitchSegment {
        return None;
    }
    Some(HlsSwitchStagingGeneration {
        acceptance_generation: episode.generation,
        candidate_identity: episode.selected_candidate_identity()?,
        progress_generation: session.origin_control.progress_generation,
        pinned_host: session.origin_control.pinned_host.clone(),
        origin_epoch: session.origin_epoch,
        origin_seq_highwater: session.origin_seq_highwater,
        proxy_next_seq: session.proxy_next_seq,
        origin_work_generation: session.activity.origin_work_generation,
        origin_account_generation: session.transient.origin_account_generation(),
    })
}

pub(super) fn switch_staging_generation_matches(
    session: &crate::HlsSession,
    expected: &HlsSwitchStagingGeneration,
) -> bool {
    let Some(current) = switch_staging_generation(session) else {
        return false;
    };
    current == *expected
}

pub(super) fn staged_switch_media_compatibility(
    session: &crate::HlsSession,
    first_segment: &crate::SegmentEntry,
    candidate_key_resources: &[TransientResourceRef],
    now_ms: u64,
) -> HlsStagedSwitchMediaCompatibility {
    // The switch transaction owns segment/MAP objects. Encrypted media is safe only
    // while its generation-local 16-byte key remains READY in the transient cache.
    match handoff_key_readiness(session, first_segment, candidate_key_resources, now_ms) {
        HlsRecoveryEncryptionReadiness::Clear
        | HlsRecoveryEncryptionReadiness::Aes128 { key: HlsRecoveryObjectReadiness::Ready } => {}
        HlsRecoveryEncryptionReadiness::Aes128 {
            key: HlsRecoveryObjectReadiness::Fetch | HlsRecoveryObjectReadiness::Staged,
        } => return HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey,
    }
    if first_segment.map_ref.is_some() {
        return HlsStagedSwitchMediaCompatibility::Compatible;
    }
    let retained_tail_contains_map = session
        .publishable_origin_head_proxy_seq
        .zip(session.publishable_origin_tail_proxy_seq)
        .is_some_and(|(head, tail)| {
            head <= tail && session.segments.range(head..=tail).any(|(_, segment)| segment.map_ref.is_some())
        });
    if retained_tail_contains_map {
        HlsStagedSwitchMediaCompatibility::CannotResetActiveMap
    } else {
        HlsStagedSwitchMediaCompatibility::Compatible
    }
}

pub(super) fn ensure_staged_switch_media_compatible(
    session: &crate::HlsSession,
    first_segment: &crate::SegmentEntry,
    candidate_key_resources: &[TransientResourceRef],
    now_ms: u64,
) -> Result<(), HlsManifestCommitError> {
    match staged_switch_media_compatibility(session, first_segment, candidate_key_resources, now_ms) {
        HlsStagedSwitchMediaCompatibility::Compatible => Ok(()),
        HlsStagedSwitchMediaCompatibility::RequiresUnstagedEncryptionKey => {
            Err(switch_staging_error(HlsManifestRejectLogReason::SwitchEncryptionKeyNotReady))
        }
        HlsStagedSwitchMediaCompatibility::CannotResetActiveMap => {
            Err(switch_staging_error(HlsManifestRejectLogReason::SwitchMapResetUnsupported))
        }
    }
}

fn recovery_readiness_for_segment(status: &SegmentCacheStatus) -> HlsRecoveryObjectReadiness {
    match status {
        SegmentCacheStatus::Ready { .. } => HlsRecoveryObjectReadiness::Ready,
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::CapacityDeferred { .. }
        | SegmentCacheStatus::FailedRetryable { .. }
        | SegmentCacheStatus::FailedPermanent { .. }
        | SegmentCacheStatus::Expired => HlsRecoveryObjectReadiness::Fetch,
    }
}

fn recovery_readiness_for_map(status: &MapCacheStatus) -> HlsRecoveryObjectReadiness {
    match status {
        MapCacheStatus::Ready { .. } => HlsRecoveryObjectReadiness::Ready,
        MapCacheStatus::Discovered
        | MapCacheStatus::Queued { .. }
        | MapCacheStatus::Fetching { .. }
        | MapCacheStatus::FailedRetryable { .. }
        | MapCacheStatus::FailedPermanent { .. }
        | MapCacheStatus::Expired => HlsRecoveryObjectReadiness::Fetch,
    }
}

fn handoff_key_readiness(
    session: &crate::HlsSession,
    first_segment: &SegmentEntry,
    candidate_key_resources: &[TransientResourceRef],
    now_ms: u64,
) -> HlsRecoveryEncryptionReadiness {
    let Some(encryption) = first_segment.encryption.as_ref() else {
        return HlsRecoveryEncryptionReadiness::Clear;
    };
    let candidate_resource = candidate_key_resources.iter().find(|resource| {
        resource.id == encryption.resource_id
            && resource.kind == TransientResourceKind::Key
            && resource.file_ext_hint.as_deref() == Some(encryption.resource_extension.as_str())
    });
    let ready = candidate_resource.is_some_and(|candidate| {
        candidate.is_valid_at(now_ms)
            && session.transient.resources.get(&candidate.id).is_some_and(|current| {
                current.kind == candidate.kind
                    && current.resolved_origin_uri == candidate.resolved_origin_uri
                    && current.file_ext_hint == candidate.file_ext_hint
            })
            && session
                .transient
                .ready_key_object_valid_until_ms(
                    &session.proxy_session_id,
                    &encryption.resource_id,
                    &encryption.resource_extension,
                    now_ms,
                )
                .is_some()
    });
    HlsRecoveryEncryptionReadiness::Aes128 {
        key: if ready { HlsRecoveryObjectReadiness::Ready } else { HlsRecoveryObjectReadiness::Fetch },
    }
}

pub(super) fn handoff_preview_recovery_workload(
    session: &crate::HlsSession,
    first_segment: &SegmentEntry,
    required_map: Option<&MapEntry>,
    candidate_key_resources: &[TransientResourceRef],
    now_ms: u64,
) -> HlsRecoveryWorkload {
    HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
        segment: recovery_readiness_for_segment(&first_segment.status),
        map: required_map.map(|map| recovery_readiness_for_map(&map.status)),
        encryption: handoff_key_readiness(session, first_segment, candidate_key_resources, now_ms),
    })
}

async fn bind_alternative_switch_candidate_workload(
    request: &OriginRefreshRequest,
    generation: &HlsSwitchStagingGeneration,
    first_segment: &SegmentEntry,
    required_map: Option<&MapEntry>,
    candidate_key_resources: &[TransientResourceRef],
) -> Result<(), HlsManifestCommitError> {
    let mut session = request.session.write().await;
    if !switch_staging_generation_matches(&session, generation) {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    }
    let now_ms = current_time_millis();
    let workload =
        handoff_preview_recovery_workload(&session, first_segment, required_map, candidate_key_resources, now_ms);
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    };
    if episode.bind_selected_candidate(generation.acceptance_generation, generation.candidate_identity, workload)
        != HlsRecoveryWorkloadBindingUpdate::Applied
    {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    }
    ensure_staged_switch_media_compatible(&session, first_segment, candidate_key_resources, now_ms)
}

async fn advance_alternative_switch_candidate_workload(
    request: &OriginRefreshRequest,
    staged_switch: &HlsStagedSwitchCommit,
    first_segment: &SegmentEntry,
    candidate_key_resources: &[TransientResourceRef],
) -> Result<(), HlsManifestCommitError> {
    let mut session = request.session.write().await;
    let staged_workload = HlsRecoveryWorkload::from_recovery_medium(HlsRecoveryMediumReadiness {
        segment: HlsRecoveryObjectReadiness::Staged,
        map: staged_switch.ready_map.map(|_| HlsRecoveryObjectReadiness::Staged),
        encryption: handoff_key_readiness(&session, first_segment, candidate_key_resources, current_time_millis()),
    });
    let generation = &staged_switch.generation;
    let advanced = session.origin_control.acceptance_episode.as_mut().is_some_and(|episode| {
        episode.advance_bound_candidate(
            generation.acceptance_generation,
            generation.candidate_identity,
            staged_workload,
        ) == HlsRecoveryWorkloadBindingUpdate::Applied
    });
    if advanced {
        Ok(())
    } else {
        Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))
    }
}

/// Stages the first segment (and its required MAP) before an alternative host may mutate the shared timeline.
///
/// The manifest recovery singleflight remains in-flight for the entire operation, so no other refresh can claim the
/// previewed proxy sequence while cache files are atomically committed. The session generation and the complete
/// preview are nevertheless checked again immediately before the timeline commit.
pub(super) async fn stage_alternative_manifest_switch(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
) -> Result<HlsStagedSwitchCommit, HlsManifestCommitError> {
    let mut manifest = match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { .. } => {
            return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
        }
    };
    let effective_host = fetched_effective_manifest_host(fetched)
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
    let effective_host_id = effective_origin_host_id(&effective_host);
    let (generation, preview, candidate_key_resources) = {
        let session = request.session.read().await;
        let generation = switch_staging_generation(&session)
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        if !generation.candidate_identity.matches_candidate(Some(&effective_host), &fetched.body) {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        let candidate_key_resources = materialize_normal_key_resources(
            &mut manifest,
            &request.reverse_proxy_rewrite_secret,
            request.now_ms,
            request.transient_resource_ttl_ms,
            session.transient.origin_account_generation(),
        );
        let preview =
            session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).map_err(handoff_preview_error)?;
        (generation, preview, candidate_key_resources)
    };
    let first_segment = preview
        .segments
        .first()
        .cloned()
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    let required_map = first_segment
        .map_ref
        .map(|map_id| {
            preview
                .maps
                .iter()
                .find(|map| map.proxy_map_id == map_id)
                .cloned()
                .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))
        })
        .transpose()?;
    bind_alternative_switch_candidate_workload(
        request,
        &generation,
        &first_segment,
        required_map.as_ref(),
        &candidate_key_resources,
    )
    .await?;
    let fetch_context = alternative_switch_fetch_context(request, fetched);
    let policy = request.segment_worker_pool.policy();
    let staged_media =
        Box::pin(stage_alternative_switch_media(&fetch_context, &policy, &first_segment, required_map.as_ref()))
            .await?;
    let committed_media =
        Box::pin(commit_alternative_switch_media(request, &first_segment, required_map.as_ref(), staged_media)).await?;
    let mut staged_switch = HlsStagedSwitchCommit {
        generation,
        effective_host_id,
        preview,
        ready_segment_proxy_seq: first_segment.proxy_seq,
        ready_segment_content_length: committed_media.ready_segment_content_length,
        ready_segment_origin_digest: committed_media.origin_digest,
        content_anchor: None,
        ready_map: committed_media.ready_map,
        ready_segment_revision: None,
        segment_cleanup: committed_media.segment_cleanup,
        map_cleanup: committed_media.map_cleanup,
        critical_handoff: None,
    };
    match prepare_switch_startup_revision(request, &first_segment).await {
        Ok(revision) => staged_switch.ready_segment_revision = revision,
        Err(error) => {
            remove_uncommitted_staged_switch_files(request, &mut staged_switch).await;
            return Err(error);
        }
    }
    if let Err(error) =
        advance_alternative_switch_candidate_workload(request, &staged_switch, &first_segment, &candidate_key_resources)
            .await
    {
        remove_uncommitted_staged_switch_files(request, &mut staged_switch).await;
        return Err(error);
    }

    Ok(staged_switch)
}

/// Registers a complete startup revision for a committed switch segment.
///
/// Returns `Ok(None)` when the session has no startup store or the segment is
/// not fast-start eligible.
async fn prepare_switch_startup_revision(
    request: &OriginRefreshRequest,
    segment: &SegmentEntry,
) -> Result<Option<crate::SegmentRevisionGuard>, HlsManifestCommitError> {
    let startup = {
        let session = request.session.read().await;
        session
            .startup
            .as_ref()
            .filter(|_| segment.is_fast_start_eligible())
            .map(|startup| (Arc::clone(&startup.store), startup.config.mode))
    };
    let Some((store, mode)) = startup else {
        return Ok(None);
    };
    let target_duration = request.session.read().await.target_duration;
    let timeout_ms = request.segment_worker_pool.policy().segment_timeout_ms(target_duration).max(1);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let committed_path = request.segment_cache.object_path(&segment.cache_key);
    // The switch object was committed without segment repair, so this committed
    // cache file is already the complete segment body.
    let guard = store
        .create(segment.cache_key.proxy_session_id().clone(), segment.proxy_seq, startup_revision_kind(mode))
        .map_err(|_| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    if complete_startup_revision(&request.segment_cache, &guard, &committed_path, deadline).await.is_err() {
        let key = guard.revision().key.clone();
        drop(guard);
        if request.segment_cache.delete(&key).await.is_err() {
            warn!("Failed to remove uncommitted HLS switch revision; cleanup deferred");
        }
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    Ok(Some(guard))
}

pub(super) fn alternative_switch_fetch_context(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
) -> SegmentFetchContext {
    let mut provider_session_headers = request.origin_provider_session_headers.clone();
    provider_session_headers.extend(fetched.provider_session_headers.clone());
    SegmentFetchContext {
        session: Arc::clone(&request.session),
        segment_cache: Arc::clone(&request.segment_cache),
        segment_repair: Arc::clone(&request.segment_repair),
        repair_access_lease_id: request.access_lease_id.clone(),
        headers: request.headers.clone(),
        origin_provider_session_headers: provider_session_headers,
        client: request.client.clone(),
        no_redirect_client: request.no_redirect_client.clone(),
        use_manual_redirects: request.use_manual_redirects,
        // The surrounding manifest refresh already owns the provider-account lease. The shared resource runner is
        // used directly here so staging cannot recursively acquire a second account handle.
        origin_io: None,
    }
}

struct HlsStagedAlternativeSwitchMedia {
    segment: StagedCacheObject,
    map: Option<StagedCacheObject>,
}

struct HlsCommittedAlternativeSwitchMedia {
    origin_digest: [u8; 32],
    ready_segment_content_length: u64,
    ready_map: Option<(crate::ProxyMapId, u64)>,
    segment_cleanup: crate::gc::HlsSwitchCacheCleanupReservation,
    map_cleanup: Option<crate::gc::HlsSwitchCacheCleanupReservation>,
}

async fn stage_alternative_switch_media(
    fetch_context: &SegmentFetchContext,
    policy: &SegmentFetchPolicy,
    first_segment: &SegmentEntry,
    required_map: Option<&MapEntry>,
) -> Result<HlsStagedAlternativeSwitchMedia, HlsManifestCommitError> {
    let segment_fetch_ref = first_segment
        .origin_fetch_ref
        .clone()
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    let staged_map = if let Some(map) = required_map {
        let fetch_ref = map
            .origin_fetch_ref
            .as_ref()
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
        Some(
            Box::pin(stage_hls_switch_resource(
                fetch_context,
                policy,
                map.cache_key.clone(),
                fetch_ref.resolved_origin_url.clone(),
                fetch_ref.byte_range,
                HlsSwitchResourceKind::Map,
            ))
            .await
            .map_err(|_| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?,
        )
    } else {
        None
    };
    let staged_segment_result = Box::pin(stage_hls_switch_resource(
        fetch_context,
        policy,
        first_segment.cache_key.clone(),
        segment_fetch_ref.resolved_origin_url,
        segment_fetch_ref.byte_range,
        HlsSwitchResourceKind::Segment,
    ))
    .await;
    let Ok(staged_segment) = staged_segment_result else {
        if let Some(staged_map) = staged_map {
            if fetch_context.segment_cache.remove_staged(staged_map).await.is_err() {
                warn!("Failed to remove staged HLS acceptance map after segment staging failure");
            }
        }
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    };
    Ok(HlsStagedAlternativeSwitchMedia { segment: staged_segment, map: staged_map })
}

async fn commit_alternative_switch_media(
    request: &OriginRefreshRequest,
    first_segment: &SegmentEntry,
    required_map: Option<&MapEntry>,
    staged_media: HlsStagedAlternativeSwitchMedia,
) -> Result<HlsCommittedAlternativeSwitchMedia, HlsManifestCommitError> {
    let HlsStagedAlternativeSwitchMedia { segment: staged_segment, map: staged_map } = staged_media;
    let origin_digest = super::origin_probe::sha256_file(&staged_segment.path).await?;
    if request.hls_proxy.segment_cache().cache_path() != request.segment_cache.cache_path()
        || request
            .hls_proxy
            .has_pending_switch_cleanup(&first_segment.cache_key, required_map.map(|map| &map.cache_key))
    {
        if request.segment_cache.remove_staged(staged_segment).await.is_err() {
            warn!("Failed to remove staged HLS acceptance segment before rollback reservation");
        }
        if let Some(staged_map) = staged_map {
            if request.segment_cache.remove_staged(staged_map).await.is_err() {
                warn!("Failed to remove staged HLS acceptance map before rollback reservation");
            }
        }
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    let Some(mut segment_cleanup) = request.hls_proxy.reserve_switch_segment_cleanup(first_segment.cache_key.clone())
    else {
        if request.segment_cache.remove_staged(staged_segment).await.is_err() {
            warn!("Failed to remove staged HLS acceptance segment without rollback capacity");
        }
        if let Some(staged_map) = staged_map {
            if request.segment_cache.remove_staged(staged_map).await.is_err() {
                warn!("Failed to remove staged HLS acceptance map without rollback capacity");
            }
        }
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    };
    let mut map_cleanup = if let Some(map) = required_map {
        let Some(cleanup) = request.hls_proxy.reserve_switch_map_cleanup(map.cache_key.clone()) else {
            segment_cleanup.disarm();
            if request.segment_cache.remove_staged(staged_segment).await.is_err() {
                warn!("Failed to remove staged HLS acceptance segment without map rollback capacity");
            }
            if let Some(staged_map) = staged_map {
                if request.segment_cache.remove_staged(staged_map).await.is_err() {
                    warn!("Failed to remove staged HLS acceptance map without rollback capacity");
                }
            }
            return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
        };
        Some(cleanup)
    } else {
        None
    };

    let map_metadata = if let (Some(map), Some(staged_map)) = (required_map, staged_map) {
        let Some(cleanup) = map_cleanup.take() else {
            segment_cleanup.disarm();
            if request.segment_cache.remove_staged(staged_segment).await.is_err() {
                warn!("Failed to remove staged HLS acceptance segment after missing map rollback reservation");
            }
            return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
        };
        let map_commit = request.segment_cache.commit_staged_with_guard(&map.cache_key, staged_map, cleanup).await;
        let Ok((metadata, cleanup)) = map_commit else {
            segment_cleanup.disarm();
            if request.segment_cache.remove_staged(staged_segment).await.is_err() {
                warn!("Failed to remove staged HLS acceptance segment after map commit failure");
            }
            return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
        };
        map_cleanup = Some(cleanup);
        Some((map.proxy_map_id, metadata.size))
    } else {
        None
    };
    let segment_commit =
        request.segment_cache.commit_staged_with_guard(&first_segment.cache_key, staged_segment, segment_cleanup).await;
    let Ok((segment_metadata, segment_cleanup)) = segment_commit else {
        if let Some(map) = required_map {
            if request.segment_cache.delete(&map.cache_key).await.is_ok() {
                if let Some(cleanup) = map_cleanup.as_mut() {
                    cleanup.disarm();
                }
            } else {
                warn!("Failed to remove committed HLS acceptance map after segment commit failure");
            }
        }
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    };

    Ok(HlsCommittedAlternativeSwitchMedia {
        origin_digest,
        ready_segment_content_length: segment_metadata.size,
        ready_map: map_metadata,
        segment_cleanup,
        map_cleanup,
    })
}

/// Pins outlive the probe and continuation staging, through atomic publication.
pub(super) struct HlsVerifiedContentAnchor {
    generation: HlsSwitchStagingGeneration,
    anchor: super::origin_probe::HlsOriginContentAnchor,
    round_lifetime: tokio_util::sync::CancellationToken,
    _standalone_round: Option<super::anchor_probe::HlsAnchorProbeRoundGuard>,
    _read_pin: HlsCommittedAcceptanceReadPin,
    _file_pin: Arc<crate::HlsRevisionFilePin>,
    _revision: Option<crate::SegmentRevisionGuard>,
    repair: Option<HlsPreparedGapRepair>,
}

impl HlsVerifiedContentAnchor {
    pub(super) fn matches(&self, generation: &HlsSwitchStagingGeneration) -> bool {
        !self.round_lifetime.is_cancelled() && self.generation == *generation
    }

    pub(super) fn is_current(&self, session: &crate::HlsSession, generation: &HlsSwitchStagingGeneration) -> bool {
        self.matches(generation)
            && switch_staging_generation_matches(session, generation)
            && super::origin_probe::HlsOriginContentEvidence::latest_ready(session).as_ref() == Some(&self.anchor)
    }

    pub(super) fn has_gap_repair(&self) -> bool { self.repair.is_some() }

    pub(super) fn revalidate_gap_repair(
        &mut self,
        leases: &crate::HlsAccessLeaseStore,
        session: &crate::HlsSession,
        now_ms: u64,
    ) {
        if self.repair.as_ref().is_some_and(|repair| {
            leases.live_playback_lease(&repair.lease_id, &session.proxy_session_id, now_ms).is_none_or(|lease| {
                lease.state != crate::HlsAccessLeaseState::Activated
                    || lease.active_until_ms.is_none_or(|until| until <= now_ms)
                    || session.first_playback_blocker(lease) != Some(&repair.plan.slot)
            })
        }) {
            self.repair = None;
        }
    }

    /// The caller has revalidated the lease and applied the exact handoff
    /// draft with both locks held. The old slot is still fenced.
    pub(super) fn publish_gap_repair(
        &self,
        session: &mut crate::HlsSession,
        request: &OriginRefreshRequest,
        now_ms: u64,
    ) {
        let Some(repair) = &self.repair else { return };
        if now_ms >= repair.deadline_ms || request.segment_cache.object_path(&repair.key) != repair.metadata.path {
            return;
        }
        let seq = repair.plan.slot.proxy_seq;
        let Some(entry) = session.segments.get_mut(&seq).filter(|entry| **entry == repair.plan.slot) else { return };
        entry.cache_key = repair.key.clone();
        entry.origin_fetch_ref = Some(repair.plan.candidate.clone());
        entry.retained_resource_identity = None;
        entry.status = SegmentCacheStatus::Ready { content_length: repair.metadata.size, ready_at_ms: now_ms };
        session.origin_content.record(entry, session.activity.origin_work_generation, repair.digest);
        repair.cleanup.lock_unpoisoned().disarm();
        if let Some(notifier) = session.segment_fetch_notifiers.remove(&seq) {
            notifier.notify_waiters();
        }
        debug!("HLS retained gap repair published: proxy_seq={seq}");
    }
}

pub(super) async fn verify_content_anchor(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
) -> Result<HlsVerifiedContentAnchor, HlsManifestCommitError> {
    let deadline = Instant::now() + Duration::from_millis(request.origin_manifest_timeout_ms);
    let mut proof = tokio::time::timeout_at(deadline, probe_content_anchor(request, fetched))
        .await
        .map_err(|_| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))??;
    // Use the remainder of the same probe budget. Failure leaves the slot
    // unavailable and lets the existing handoff policy evaluate its candidate.
    proof.repair = tokio::time::timeout_at(deadline, prepare_gap_repair(request, fetched, &proof)).await.ok().flatten();
    if request.session.try_read().is_ok_and(|session| !proof.is_current(&session, &proof.generation)) {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    }
    Ok(proof)
}

struct HlsPreparedGapRepair {
    plan: HlsRetainedGapRepair,
    lease_id: crate::HlsAccessLeaseId,
    deadline_ms: u64,
    key: SegmentCacheKey,
    metadata: crate::CachedSegmentMetadata,
    digest: [u8; 32],
    cleanup: Mutex<crate::gc::HlsSwitchCacheCleanupReservation>,
    _file_pin: crate::HlsRevisionFilePin,
}

async fn prepare_gap_repair(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    proof: &HlsVerifiedContentAnchor,
) -> Option<HlsPreparedGapRepair> {
    let now_ms = request.hls_proxy.now_ms();
    let proxy = request.session.read().await.proxy_session_id.clone();
    let leases = request.hls_proxy.active_live_playback_snapshots_for_session(&proxy, now_ms).await;
    if leases.is_empty() {
        return None;
    }
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)
    else {
        return None;
    };
    let (plan, lease_id, deadline_ms) = {
        let session = request.session.read().await;
        if !proof.is_current(&session, &proof.generation) {
            return None;
        }
        let (plan, lease) = leases
            .iter()
            .filter_map(|lease| Some((session.retained_gap_repair(lease, &manifest, proof.anchor.proxy_seq)?, lease)))
            .min_by(|(left, left_lease), (right, right_lease)| {
                left.slot
                    .proxy_seq
                    .cmp(&right.slot.proxy_seq)
                    .then_with(|| left_lease.lease_id.0.cmp(&right_lease.lease_id.0))
            })?;
        let deadline_ms = session
            .origin_control
            .acceptance_episode
            .as_ref()?
            .timing()
            .acceptance_deadline
            .as_millis_since_epoch()
            .min(lease.active_until_ms?)
            .min(lease.valid_until_ms);
        (plan, lease.lease_id.clone(), deadline_ms)
    };
    let remaining = deadline_ms.checked_sub(request.hls_proxy.now_ms()).filter(|ms| *ms > 0)?;
    let (key, metadata, digest, cleanup, file_pin) =
        tokio::time::timeout(Duration::from_millis(remaining), download_gap_repair(request, fetched, proof, &plan))
            .await
            .ok()??;
    Some(HlsPreparedGapRepair {
        plan,
        lease_id,
        deadline_ms,
        key,
        metadata,
        digest,
        cleanup: Mutex::new(cleanup),
        _file_pin: file_pin,
    })
}

type StagedGapMedia = (
    SegmentCacheKey,
    crate::CachedSegmentMetadata,
    [u8; 32],
    crate::gc::HlsSwitchCacheCleanupReservation,
    crate::HlsRevisionFilePin,
);

async fn download_gap_repair(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    proof: &HlsVerifiedContentAnchor,
    plan: &HlsRetainedGapRepair,
) -> Option<StagedGapMedia> {
    if request.hls_proxy.segment_cache().cache_path() != request.segment_cache.cache_path() {
        return None;
    }
    let _permit = request.segment_worker_pool.try_reserve_origin_probe(&request.session).await?;
    let context = alternative_switch_fetch_context(request, fetched);
    // A private physical name for the same proxy slot prevents an in-flight
    // repair or its cleanup from touching a concurrently published READY file.
    let key = SegmentCacheKey::new(
        plan.slot.cache_key.proxy_session_id().clone(),
        plan.slot.proxy_seq,
        format!("repair-{:032x}.ts", fastrand::u128(..)),
    );
    let staged = stage_hls_switch_resource(
        &context,
        &request.segment_worker_pool.policy(),
        key.clone(),
        plan.candidate.resolved_origin_url.clone(),
        plan.candidate.byte_range,
        HlsSwitchResourceKind::Segment,
    )
    .await
    .ok()?;
    let Some(digest) = validate_gap_media(request, proof, &staged).await else {
        let _ = request.segment_cache.remove_staged(staged).await;
        return None;
    };
    {
        let session = request.session.read().await;
        if !proof.is_current(&session, &proof.generation)
            || session.segments.get(&plan.slot.proxy_seq) != Some(&plan.slot)
        {
            drop(session);
            let _ = request.segment_cache.remove_staged(staged).await;
            return None;
        }
    }
    let cleanup = request.hls_proxy.reserve_switch_segment_cleanup(key.clone())?;
    let (metadata, cleanup) = request.segment_cache.commit_staged_with_guard(&key, staged, cleanup).await.ok()?;
    let file_pin = request.segment_cache.pin_revision_file(&metadata.path).ok()?;
    Some((key, metadata, digest, cleanup, file_pin))
}

async fn validate_gap_media(
    request: &OriginRefreshRequest,
    proof: &HlsVerifiedContentAnchor,
    staged: &StagedCacheObject,
) -> Option<[u8; 32]> {
    const PACKET: usize = 188;
    if staged.size == 0 || !staged.size.is_multiple_of(PACKET as u64) {
        return None;
    }
    let anchor_tracks = inspect_cache_object_tracks(&request.segment_cache, &proof.anchor.cache_key).await;
    let mut file = tokio::fs::File::open(&staged.path).await.ok()?;
    let tracks: HlsTrackEvidenceResolution =
        crate::inspect_mpeg_ts_async(&mut file, crate::HlsTsProbeProtection::Clear, crate::HlsTsProbeBudget::default())
            .await
            .into();
    if tracks.signature()? != anchor_tracks.signature()? {
        return None;
    }
    file.rewind().await.ok()?;
    let mut digest = Sha256::new();
    let mut remaining = staged.size;
    let mut buffer = [0_u8; PACKET * 64];
    while remaining != 0 {
        let length = usize::try_from(remaining.min(buffer.len() as u64)).ok()?;
        let chunk = &mut buffer[..length];
        file.read_exact(chunk).await.ok()?;
        let (packets, rest) = chunk.as_chunks::<PACKET>();
        if !rest.is_empty()
            || packets.iter().any(|packet| {
                if packet[0] != 0x47 || packet[1] & 0x80 != 0 || packet[3] & 0xc0 != 0 {
                    return true;
                }
                match packet[3] & 0x30 {
                    0x10 => false,
                    0x20 => packet[4] != 183,
                    0x30 => packet[4] > 182,
                    _ => true,
                }
            })
        {
            return None;
        }
        digest.update(chunk);
        remaining -= length as u64;
    }
    Some(digest.finalize().into())
}

struct HlsContentAnchorProbe {
    proof: HlsVerifiedContentAnchor,
    candidate_ref: crate::OriginSegmentFetchRef,
    reference_path: std::path::PathBuf,
    identity: super::anchor_probe::ProbeIdentity,
    round: Arc<super::anchor_probe::HlsAnchorProbeRound>,
}

async fn capture_content_anchor_probe(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
) -> Result<HlsContentAnchorProbe, HlsManifestCommitError> {
    let unavailable = || switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable);
    let manifest = match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) if manifest.maps.is_empty() => manifest,
        _ => return Err(unavailable()),
    };
    let fetch_context = alternative_switch_fetch_context(request, fetched);
    let (proof, candidate_ref, reference_path, identity, round) = {
        let session = request.session.read().await;
        let generation = switch_staging_generation(&session)
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        if !matches!(session.mode, HlsSessionMode::NormalCacheTimeline)
            || !generation
                .candidate_identity
                .matches_candidate(fetched_effective_manifest_host(fetched).as_deref(), &fetched.body)
        {
            return Err(unavailable());
        }
        let anchor = super::origin_probe::HlsOriginContentEvidence::latest_ready(&session).ok_or_else(|| {
            debug!("HLS content anchor probe: reason=missing-current-ready-anchor");
            unavailable()
        })?;
        // Sequence identifies the same occurrence; neither a filename nor the
        // overlap-trimmed handoff preview identifies the reference probe.
        let candidate = manifest
            .segments
            .iter()
            .find(|segment| segment.origin_seq == anchor.origin_key.host_local_sequence)
            .ok_or_else(|| {
                debug!(
                    "HLS content anchor probe: reference_origin_seq={} reason=reference-missing",
                    anchor.origin_key.host_local_sequence
                );
                unavailable()
            })?;
        if candidate.map_ref.is_some()
            || candidate.encryption.is_some()
            || !anchor.identity.same_path_and_range(HlsMediaResourceIdentity::from_url(
                &candidate.resolved_origin_url,
                candidate.origin_byte_range,
            ))
        {
            return Err(unavailable());
        }
        debug!("HLS content anchor probe: reference_origin_seq={} reference_proxy_seq={} candidate_origin_seq={} candidate_index={} origin_epoch={} origin_generation={} remaining_attempts={} reason=current-ready-reference",
            anchor.origin_key.host_local_sequence, anchor.proxy_seq, candidate.origin_seq,
            manifest.segments.iter().position(|segment| segment.origin_seq == candidate.origin_seq).unwrap_or_default(),
            session.origin_epoch, session.activity.origin_work_generation,
            request.attempt.as_ref().map_or_else(|| "none".to_string(), |attempt| attempt.remaining_attempts().to_string()));
        let entry = session.segments.get(&anchor.proxy_seq).ok_or_else(unavailable)?;
        let (path, revision) = anchor_revision_path(&session, &anchor, &request.segment_cache);
        let identity = super::anchor_probe::ProbeIdentity::new(
            &session,
            &anchor,
            &fetch_context,
            &candidate.resolved_origin_url,
            candidate.origin_byte_range,
        )
        .map_err(|()| unavailable())?;
        if session.origin_content.probes.rejects(&identity) {
            session.origin_content.probes.mark_candidate(generation.candidate_identity);
            debug!("HLS content anchor probe: reason=final-origin-content-mismatch");
            return Err(unavailable());
        }
        let (round, standalone) = if let Some(round) = session
            .origin_content
            .probes
            .round()
            .map_err(|()| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?
        {
            (round, None)
        } else {
            let owner = super::anchor_probe::HlsAnchorProbeRoundGuard::standalone(1);
            (Arc::clone(&owner.0), Some(owner))
        };
        let file_pin = round.pin_reference(&request.segment_cache, &path).map_err(|()| unavailable())?;
        let proof = HlsVerifiedContentAnchor {
            generation,
            anchor,
            round_lifetime: round.cancelled.clone(),
            _standalone_round: standalone,
            _read_pin: HlsCommittedAcceptanceReadPin::acquire(Arc::clone(&entry.access), current_time_millis()),
            _file_pin: file_pin,
            _revision: revision,
            repair: None,
        };
        (
            proof,
            crate::OriginSegmentFetchRef {
                resolved_origin_url: candidate.resolved_origin_url.clone(),
                byte_range: candidate.origin_byte_range,
                valid_until_ms: None,
            },
            path,
            identity,
            round,
        )
    };
    Ok(HlsContentAnchorProbe { proof, candidate_ref, reference_path, identity, round })
}

async fn probe_content_anchor(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
) -> Result<HlsVerifiedContentAnchor, HlsManifestCommitError> {
    let unavailable = || switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable);
    let HlsContentAnchorProbe { proof, candidate_ref, reference_path, identity, round } =
        capture_content_anchor_probe(request, fetched).await?;
    let fetch_context = alternative_switch_fetch_context(request, fetched);
    tokio::fs::metadata(&reference_path).await.map_err(|_| unavailable())?;
    log_current_anchor_reserve(request, &proof.generation);
    let digest = round
        .digest(
            identity.clone(),
            super::anchor_probe::fetch_digest(
                request.clone(),
                fetch_context,
                proof.anchor.cache_key.clone(),
                candidate_ref,
            ),
        )
        .await
        .map_err(|()| unavailable())?;
    {
        let mut session = request.session.write().await;
        if !round.is_active() {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        if digest != proof.anchor.digest {
            // The completed comparison belongs to this exact semantic binding,
            // even when a newer technical episode superseded its commit permission.
            session.origin_content.probes.reject(&identity);
            round.mark_rejected(proof.generation.candidate_identity);
            debug!("HLS content anchor probe: reason=original-content-mismatch-final");
        }
        if !proof.is_current(&session, &proof.generation) {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        // A concurrent round may have disproved this binding while this fetch
        // was in flight. A later matching body cannot override that finding.
        if session.origin_content.probes.rejects(&identity) {
            return Err(unavailable());
        }
    }
    Ok(proof)
}

pub(super) async fn reject_final_content_mismatch(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    acceptance_mode: crate::manifest_fetch::HlsManifestCommitAcceptanceMode,
) -> Result<(), HlsManifestCommitError> {
    if !matches!(
        acceptance_mode,
        crate::manifest_fetch::HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
            | crate::manifest_fetch::HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate
            | crate::manifest_fetch::HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate
    ) {
        return Ok(());
    }
    let manifest = match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) if manifest.maps.is_empty() => manifest,
        _ => return Ok(()),
    };
    let context = alternative_switch_fetch_context(request, fetched);
    let session = request.session.read().await;
    let Some(anchor) = super::origin_probe::HlsOriginContentEvidence::latest_ready(&session) else { return Ok(()) };
    let Some(candidate) =
        manifest.segments.iter().find(|candidate| candidate.origin_seq == anchor.origin_key.host_local_sequence)
    else {
        return Ok(());
    };
    if candidate.map_ref.is_some() || candidate.encryption.is_some() {
        return Ok(());
    }
    let identity = super::anchor_probe::ProbeIdentity::new(
        &session,
        &anchor,
        &context,
        &candidate.resolved_origin_url,
        candidate.origin_byte_range,
    )
    .map_err(|()| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    if !session.origin_content.probes.rejects(&identity) {
        return Ok(());
    }
    if let Some(generation) = switch_staging_generation(&session).filter(|generation| {
        generation
            .candidate_identity
            .matches_candidate(fetched_effective_manifest_host(fetched).as_deref(), &fetched.body)
    }) {
        session.origin_content.probes.mark_candidate(generation.candidate_identity);
    }
    debug!("HLS content anchor probe: reason=final-origin-content-mismatch");
    Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))
}

/// Diagnostics use the existing lease/READY reserve calculation. Contended or
/// missing evidence stays unknown; logging never waits for the playback locks.
fn log_current_anchor_reserve(request: &OriginRefreshRequest, generation: &HlsSwitchStagingGeneration) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let now_ms = request.hls_proxy.now_ms();
    let (Ok(mut leases), Ok(session)) = (request.hls_proxy.access_leases().try_write(), request.session.try_read())
    else {
        debug!("HLS content anchor reserve: guaranteed_reserve_ms=none transition_margin_ms=none reason=state-busy");
        return;
    };
    if !switch_staging_generation_matches(&session, generation) {
        return;
    }
    let Some(episode) = session.origin_control.acceptance_episode.as_ref() else { return };
    let budget = episode.timing().trigger_budget;
    let reserve = leases
        .active_live_playback_snapshots_for_session(&session.proxy_session_id, now_ms)
        .iter()
        .filter_map(|lease| {
            let manifest = lease.last_manifest_snapshot.as_ref()?;
            let ready_timeline = session.ready_timeline_snapshot(
                lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
                now_ms,
            );
            Some(crate::evaluate_lease_reserve(crate::HlsLeaseReserveInput {
                manifest,
                cursor: &lease.playback_cursor,
                ready_timeline: &ready_timeline,
                now_ms,
                recovery_trigger_budget: budget,
                origin_path_degraded: session.origin_control.path_condition.is_degraded(),
                recovery_committed: false,
            }))
        })
        .min_by_key(|reserve| reserve.guaranteed_reserve_ms);
    debug!("HLS content anchor reserve: guaranteed_reserve_ms={} transition_margin_ms={} episode_trigger_budget_ms={} remaining_attempts={} reason={}",
        reserve.map_or_else(|| "none".to_string(), |reserve| reserve.guaranteed_reserve_ms.to_string()),
        reserve.map_or_else(|| "none".to_string(), |reserve| reserve.transition_margin.as_millis().to_string()),
        budget.as_millis(), request.attempt.as_ref().map_or_else(|| "none".to_string(), |attempt| attempt.remaining_attempts().to_string()),
        if reserve.is_some() { "current-lease-ready-reserve" } else { "no-active-lease-evidence" });
}

fn anchor_revision_path(
    session: &crate::HlsSession,
    anchor: &super::origin_probe::HlsOriginContentAnchor,
    cache: &HlsSegmentCache,
) -> (std::path::PathBuf, Option<crate::SegmentRevisionGuard>) {
    let revision = session.startup.as_ref().and_then(|startup| startup.revisions.get(&anchor.proxy_seq)).cloned();
    let path = revision
        .as_ref()
        .and_then(|guard| match &*guard.revision().subscribe().borrow() {
            crate::SegmentRevisionState::Complete(metadata) => Some(metadata.path.clone()),
            _ => None,
        })
        .unwrap_or_else(|| cache.object_path(&anchor.cache_key));
    (path, revision)
}

pub(super) fn select_critical_handoff_lease(
    session: &crate::HlsSession,
    leases: &[crate::HlsAccessLease],
    generation: &HlsSwitchStagingGeneration,
    now_ms: u64,
) -> Option<(crate::HlsAccessLease, HlsCriticalHandoffSnapshot)> {
    select_critical_handoff_lease_for_generation(
        session,
        leases,
        generation.acceptance_generation,
        switch_staging_generation_matches(session, generation),
        now_ms,
    )
}

pub(super) fn critical_handoff_snapshot_is_current(
    leases: &mut crate::HlsAccessLeaseStore,
    session: &crate::HlsSession,
    generation: &HlsSwitchStagingGeneration,
    expected: &HlsCriticalHandoffSnapshot,
    now_ms: u64,
) -> bool {
    critical_handoff_snapshot_matches_generation(
        leases,
        session,
        generation.acceptance_generation,
        switch_staging_generation_matches(session, generation),
        expected,
        now_ms,
    )
}

pub(super) async fn verify_staged_emergency_handoff(
    request: &OriginRefreshRequest,
    fetched: &FetchedOriginManifest,
    staged: &HlsStagedSwitchCommit,
) -> Result<(), HlsManifestCommitError> {
    let manifest = match parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url) {
        OriginManifestParseOutcome::Normal(manifest) if manifest.maps.is_empty() => manifest,
        OriginManifestParseOutcome::Normal(_) | OriginManifestParseOutcome::TransientPassthrough { .. } => {
            return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
        }
    };
    if manifest.segments.first().is_none_or(|segment| segment.map_ref.is_some()) {
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    let preparation = staged
        .critical_handoff
        .as_ref()
        .filter(|preparation| preparation.generation == staged.generation)
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
    let candidate_key = {
        let session = request.session.read().await;
        if !switch_staging_generation_matches(&session, &staged.generation)
            || !matches!(session.mode, HlsSessionMode::NormalCacheTimeline)
        {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        staged
            .preview
            .segments
            .first()
            .filter(|segment| segment.map_ref.is_none() && segment.proxy_file_ext.eq_ignore_ascii_case("ts"))
            .map(|segment| segment.cache_key.clone())
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?
    };
    let base_tracks = preparation
        .base_evidence
        .track_resolution()
        .and_then(HlsTrackEvidenceResolution::signature)
        .cloned()
        .ok_or_else(|| {
            debug!(
                "HLS critical manifest handoff rejected: acceptance_generation={} evidence=lease-base reason={}",
                staged.generation.acceptance_generation.0,
                preparation.base_evidence.track_evidence_reason_code()
            );
            switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable)
        })?;
    let candidate_resolution = inspect_cache_object_tracks(&request.segment_cache, &candidate_key).await;
    let candidate_tracks = candidate_resolution.signature().cloned().ok_or_else(|| {
        debug!(
            "HLS critical manifest handoff rejected: acceptance_generation={} evidence=staged-candidate reason={}",
            staged.generation.acceptance_generation.0,
            candidate_resolution.reason_code()
        );
        switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable)
    })?;
    if candidate_tracks != base_tracks {
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    let terminal_comparison = terminal_alternative_compatibility_for_critical_lease(
        preparation.terminal_asset.as_deref(),
        &preparation.lease,
        &base_tracks,
    );
    if terminal_comparison != HlsTerminalAlternativeCompatibility::LiveHandoffSafer {
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    debug!(
        "HLS critical manifest handoff verified: session={} acceptance_generation={} decision=emergency-new-origin-epoch",
        {
            let session = request.session.read().await;
            safe_session_key(&session.key)
        },
        staged.generation.acceptance_generation.0
    );
    Ok(())
}

pub(super) async fn prepare_critical_handoff(
    request: &OriginRefreshRequest,
) -> Result<HlsCriticalHandoffPreparation, HlsManifestCommitError> {
    let now_ms = current_time_millis();
    let terminal_response = request.app_config.custom_stream_response.load_full();
    let proxy_session_id = request.session.read().await.proxy_session_id.clone();
    let leases = request.hls_proxy.active_live_playback_snapshots_for_session(&proxy_session_id, now_ms).await;
    let (generation, lease, snapshot, base_preparation) = {
        let session = request.session.read().await;
        let generation = switch_staging_generation(&session)
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        if !matches!(session.mode, HlsSessionMode::NormalCacheTimeline) {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        let (lease, snapshot) = select_critical_handoff_lease(&session, &leases, &generation, now_ms)
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
        let manifest = lease
            .last_manifest_snapshot
            .as_ref()
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        let base_preparation = pin_terminal_base_evidence(&session, manifest, now_ms);
        (generation, lease, snapshot, base_preparation)
    };
    let manifest = lease
        .last_manifest_snapshot
        .as_ref()
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
    let base_evidence = resolve_terminal_base_evidence(&request.segment_cache, manifest, base_preparation).await;
    if base_evidence.track_base() != Some(&snapshot.base) {
        return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
    }
    if base_evidence.track_resolution().and_then(HlsTrackEvidenceResolution::signature).is_none() {
        debug!(
            "HLS critical manifest handoff rejected: acceptance_generation={} evidence=lease-base reason={}",
            generation.acceptance_generation.0,
            base_evidence.track_evidence_reason_code()
        );
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    let terminal_asset = terminal_response
        .as_ref()
        .and_then(|response| response.channel_unavailable.as_ref())
        .and_then(|buffer| snapshot_terminal_media_asset(buffer).ok());
    Ok(HlsCriticalHandoffPreparation { generation, lease, snapshot, base_evidence, terminal_response, terminal_asset })
}

pub(super) async fn inspect_cache_object_tracks(
    cache: &HlsSegmentCache,
    key: &SegmentCacheKey,
) -> HlsTrackEvidenceResolution {
    let file = match cache.open_range(key, 0).await {
        Ok(file) => file,
        Err(error) => return HlsTrackEvidenceResolution::Io(error.kind()),
    };
    crate::inspect_mpeg_ts_async(file, crate::HlsTsProbeProtection::Clear, crate::HlsTsProbeBudget::default())
        .await
        .into()
}

pub(super) async fn remove_uncommitted_staged_switch_files(
    request: &OriginRefreshRequest,
    staged: &mut HlsStagedSwitchCommit,
) {
    let (segment_key, map_key) = {
        let session = request.session.read().await;
        let segment_key = staged
            .preview
            .segments
            .first()
            .filter(|preview| {
                session.segments.get(&preview.proxy_seq).is_none_or(|entry| entry.cache_key != preview.cache_key)
            })
            .map(|preview| preview.cache_key.clone());
        let map_key = staged.ready_map.and_then(|(ready_map_id, _)| {
            staged.preview.maps.iter().find_map(|preview| {
                (preview.proxy_map_id == ready_map_id
                    && session.maps.get(&preview.proxy_map_id).is_none_or(|entry| entry.cache_key != preview.cache_key))
                .then(|| preview.cache_key.clone())
            })
        });
        (segment_key, map_key)
    };
    if let Some(key) = segment_key {
        match request.segment_cache.delete(&key).await {
            Ok(()) => staged.segment_cleanup.disarm(),
            Err(_) => warn!("Failed to remove uncommitted HLS acceptance segment; cleanup deferred"),
        }
    } else {
        staged.segment_cleanup.disarm();
    }
    if let Some(key) = map_key {
        match request.segment_cache.delete(&key).await {
            Ok(()) => {
                if let Some(cleanup) = staged.map_cleanup.as_mut() {
                    cleanup.disarm();
                }
            }
            Err(_) => warn!("Failed to remove uncommitted HLS acceptance map; cleanup deferred"),
        }
    } else if let Some(cleanup) = staged.map_cleanup.as_mut() {
        cleanup.disarm();
    }
    if let Some(revision) = staged.ready_segment_revision.take() {
        let key = revision.revision().key.clone();
        drop(revision);
        if request.segment_cache.delete(&key).await.is_err() {
            warn!("Failed to remove uncommitted HLS switch revision; cleanup deferred");
        }
    }
}
