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
    current_time_millis, OriginRefreshRequest,
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
    terminal_tail::{
        pin_terminal_base_evidence, resolve_terminal_base_evidence, snapshot_terminal_media_asset,
        HlsTerminalBaseEvidence, HlsTerminalMediaAsset,
    },
    timeline::{effective_origin_host_id, HlsOriginHandoffPreview, HlsOriginHandoffPreviewError},
    HlsSegmentCache, HlsSessionMode, HlsTrackEvidenceResolution, MapCacheStatus, MapEntry, SegmentCacheKey,
    SegmentCacheStatus, SegmentEntry, SegmentFetchContext, StagedCacheObject, TransientResourceKind,
    TransientResourceRef,
};
use log::{debug, warn};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
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
}

pub(super) struct HlsStagedSwitchCommit {
    pub(super) generation: HlsSwitchStagingGeneration,
    pub(super) effective_host_id: u64,
    pub(super) preview: HlsOriginHandoffPreview,
    pub(super) ready_segment_proxy_seq: u64,
    pub(super) ready_segment_content_length: u64,
    pub(super) ready_map: Option<(crate::ProxyMapId, u64)>,
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
    let candidate_key_resources = materialize_normal_key_resources(
        &mut manifest,
        &request.reverse_proxy_rewrite_secret,
        request.now_ms,
        request.transient_resource_ttl_ms,
    );
    let effective_host = fetched_effective_manifest_host(fetched)
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
    let effective_host_id = effective_origin_host_id(&effective_host);
    let (generation, preview) = {
        let session = request.session.read().await;
        let generation = switch_staging_generation(&session)
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated))?;
        if !generation.candidate_identity.matches_candidate(Some(&effective_host), &fetched.body) {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        let preview =
            session.preview_origin_handoff_manifest(&manifest, effective_host_id, 0).map_err(handoff_preview_error)?;
        (generation, preview)
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
        ready_map: committed_media.ready_map,
        segment_cleanup: committed_media.segment_cleanup,
        map_cleanup: committed_media.map_cleanup,
        critical_handoff: None,
    };
    if let Err(error) =
        advance_alternative_switch_candidate_workload(request, &staged_switch, &first_segment, &candidate_key_resources)
            .await
    {
        remove_uncommitted_staged_switch_files(request, &mut staged_switch).await;
        return Err(error);
    }

    Ok(staged_switch)
}

fn alternative_switch_fetch_context(
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
        ready_segment_content_length: segment_metadata.size,
        ready_map: map_metadata,
        segment_cleanup,
        map_cleanup,
    })
}

pub(super) async fn verify_staged_content_anchor(
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
    let candidate = manifest
        .segments
        .first()
        .filter(|segment| segment.map_ref.is_none())
        .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    let candidate_identity =
        HlsMediaResourceIdentity::from_url(&candidate.resolved_origin_url, candidate.origin_byte_range);
    let (candidate_key, committed_key, committed_read_pin) = {
        let session = request.session.read().await;
        if !switch_staging_generation_matches(&session, &staged.generation)
            || !matches!(session.mode, HlsSessionMode::NormalCacheTimeline)
        {
            return Err(switch_staging_error(HlsManifestRejectLogReason::StagedSwitchInvalidated));
        }
        let candidate_key = staged
            .preview
            .segments
            .first()
            .filter(|segment| segment.map_ref.is_none() && segment.proxy_file_ext.eq_ignore_ascii_case("ts"))
            .map(|segment| segment.cache_key.clone())
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
        let (committed_key, committed_access) = session
            .segments
            .values()
            .rev()
            .filter(|entry| entry.origin_key.origin_epoch == session.origin_epoch)
            .filter(|entry| {
                entry.map_ref.is_none()
                    && entry.proxy_file_ext.eq_ignore_ascii_case("ts")
                    && matches!(&entry.status, SegmentCacheStatus::Ready { .. })
            })
            .find_map(|entry| {
                let fetch_ref = entry.origin_fetch_ref.as_ref()?;
                HlsMediaResourceIdentity::from_url(&fetch_ref.resolved_origin_url, fetch_ref.byte_range)
                    .matches(candidate_identity)
                    .then(|| (entry.cache_key.clone(), Arc::clone(&entry.access)))
            })
            .ok_or_else(|| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
        let committed_read_pin = HlsCommittedAcceptanceReadPin::acquire(committed_access, current_time_millis());
        (candidate_key, committed_key, committed_read_pin)
    };
    let candidate_fingerprint = Box::pin(sha256_cache_object(&request.segment_cache, &candidate_key)).await?;
    let committed_fingerprint = Box::pin(sha256_cache_object(&request.segment_cache, &committed_key)).await?;
    if candidate_fingerprint != committed_fingerprint {
        return Err(switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable));
    }
    drop(committed_read_pin);
    Ok(())
}

async fn sha256_cache_object(
    cache: &HlsSegmentCache,
    key: &SegmentCacheKey,
) -> Result<[u8; 32], HlsManifestCommitError> {
    let mut file = cache
        .open_range(key, 0)
        .await
        .map_err(|_| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
    let mut hasher = Sha256::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|_| switch_staging_error(HlsManifestRejectLogReason::SwitchResourceUnavailable))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hasher.finalize().into())
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
}
