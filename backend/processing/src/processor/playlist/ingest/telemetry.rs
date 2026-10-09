use super::{
    cluster_selected,
    status::{ClusterStatusSource, ProviderFailureEvidence},
    PersistedSnapshotContext, PlaylistDownloadResult,
};
use crate::input_cache;
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, InputRefreshPolicy, InputType, PersistedPlaylistUpdateClusterSnapshot,
        PersistedPlaylistUpdateQualityDecision, PersistedPlaylistUpdateQualitySnapshot,
        PersistedPlaylistUpdateTechnicalState, PlaylistUpdateClusterDecision, PlaylistUpdateClusterTelemetry,
        PlaylistUpdateDataSource, PlaylistUpdateInputTelemetry, XtreamCluster,
    },
};
use std::collections::HashMap;
use tuliprox_core::model::{ConfigInput, ConfigInputFlags};
use tuliprox_iptv::provider::PlaylistFetch;

pub(in crate::processor::playlist) const PIPELINE_TRANSPARENCY_CLUSTERS: [XtreamCluster; 3] =
    [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];

pub(super) fn is_cluster_input(input_type: InputType) -> bool {
    matches!(input_type, InputType::Xtream | InputType::Stalker | InputType::M3u)
}

pub(in crate::processor::playlist) fn cluster_is_configured(input: &ConfigInput, cluster: XtreamCluster) -> bool {
    let skip_flag = match cluster {
        XtreamCluster::Live => ConfigInputFlags::SkipLive,
        XtreamCluster::Video => ConfigInputFlags::SkipVod,
        XtreamCluster::Series => ConfigInputFlags::SkipSeries,
    };
    !input.has_flag(skip_flag)
}

pub(super) fn input_telemetry_for_fetch(
    input: &ConfigInput,
    refresh_policy: InputRefreshPolicy,
    source: PlaylistUpdateDataSource,
    provider_clusters: &[XtreamCluster],
    fetch: Option<&PlaylistFetch>,
) -> PlaylistUpdateInputTelemetry {
    let input_type = input.get_download_input_type();
    if !is_cluster_input(input_type) {
        return PlaylistUpdateInputTelemetry { refresh_policy, source: Some(source), clusters: Vec::new() };
    }

    let mut candidate_counts = HashMap::<XtreamCluster, usize>::new();
    if let Some(fetch) = fetch {
        for group in &fetch.groups {
            *candidate_counts.entry(group.xtream_cluster).or_default() += group.channels.len();
        }
        if fetch.errors.is_empty() && !fetch.partial && !fetch.persisted {
            for cluster in provider_clusters {
                candidate_counts.entry(*cluster).or_default();
            }
        }
    }

    let clusters = PIPELINE_TRANSPARENCY_CLUSTERS
        .into_iter()
        .map(|cluster| {
            let requested = cluster_is_configured(input, cluster);
            let source = requested.then(|| {
                if provider_clusters.contains(&cluster) {
                    PlaylistUpdateDataSource::Provider
                } else {
                    PlaylistUpdateDataSource::Cache
                }
            });
            let configured_threshold =
                input.options.as_ref().map_or(0, |options| options.update_quality.threshold(cluster));
            let mut telemetry = PlaylistUpdateClusterTelemetry {
                cluster,
                requested,
                source,
                baseline_count: None,
                candidate_count: candidate_counts.get(&cluster).copied(),
                active_count: None,
                threshold: (configured_threshold > 0).then_some(configured_threshold),
                quality: None,
                decision: None,
                technical_state: None,
            };

            if let Some(fetch) = fetch {
                if requested && fetch.failed_clusters.contains(&cluster) {
                    telemetry.technical_state = Some(PersistedPlaylistUpdateTechnicalState::Failed);
                }
                if let Some(rejection) = fetch.quality_rejections.iter().find(|item| item.cluster == cluster) {
                    telemetry.baseline_count = Some(rejection.current_count);
                    telemetry.candidate_count = Some(rejection.candidate_count);
                    telemetry.active_count = Some(rejection.current_count);
                    telemetry.threshold = Some(rejection.threshold);
                    telemetry.quality = Some(rejection.quality);
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Rejected);
                } else if let Some(acceptance) = fetch.quality_acceptances.iter().find(|item| item.cluster == cluster) {
                    telemetry.baseline_count = acceptance.current_count;
                    telemetry.candidate_count = Some(acceptance.candidate_count);
                    telemetry.threshold = Some(acceptance.threshold);
                    telemetry.quality = acceptance.quality;
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                } else if let Some(force_update) = fetch.force_updates.iter().find(|item| item.cluster == cluster) {
                    telemetry.candidate_count = Some(force_update.candidate_count);
                    telemetry.threshold =
                        (force_update.configured_threshold > 0).then_some(force_update.configured_threshold);
                    telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                } else if source == Some(PlaylistUpdateDataSource::Provider) {
                    if configured_threshold == 0
                        && (candidate_counts.contains_key(&cluster) || (fetch.errors.is_empty() && !fetch.partial))
                    {
                        telemetry.decision = Some(PlaylistUpdateClusterDecision::Accepted);
                    } else if (provider_clusters == [cluster] || input_type == InputType::M3u)
                        && fetch.groups.is_empty()
                        && !fetch.persisted
                        && !fetch.errors.is_empty()
                    {
                        telemetry.decision = Some(PlaylistUpdateClusterDecision::TechnicalError);
                    }
                }
            }
            telemetry
        })
        .collect();

    PlaylistUpdateInputTelemetry { refresh_policy, source: None, clusters }
}

pub(super) fn confirm_published_cluster_counts(input_telemetry: &mut PlaylistUpdateInputTelemetry) {
    for cluster in &mut input_telemetry.clusters {
        if cluster.decision == Some(PlaylistUpdateClusterDecision::Accepted) {
            cluster.active_count = cluster.candidate_count;
        }
    }
}

pub(super) fn finalize_input_telemetry(
    playlist_download_result: &mut PlaylistDownloadResult,
    storage_error: Option<&TuliproxError>,
) {
    let activation_is_uncertain = storage_error.is_some()
        || playlist_download_result.partial
        || !playlist_download_result.download_err.is_empty();
    if let Some(input_telemetry) = playlist_download_result.input_telemetry.as_mut() {
        if activation_is_uncertain {
            for cluster in &mut input_telemetry.clusters {
                cluster.active_count = None;
            }
        } else {
            confirm_published_cluster_counts(input_telemetry);
        }
    }
}

pub(crate) fn neutralize_overlaid_cluster_facts(
    input_telemetry: &mut PlaylistUpdateInputTelemetry,
    overlaid_clusters: ClusterFlags,
) {
    for cluster in &mut input_telemetry.clusters {
        if cluster_selected(cluster.cluster, overlaid_clusters) {
            cluster.source = None;
            cluster.baseline_count = None;
            cluster.candidate_count = None;
            cluster.active_count = None;
            cluster.quality = None;
            cluster.decision = None;
            cluster.technical_state = None;
        }
    }
}

fn persisted_quality_snapshot(
    cluster: &PlaylistUpdateClusterTelemetry,
    effective_policy: InputRefreshPolicy,
) -> Option<PersistedPlaylistUpdateQualitySnapshot> {
    if effective_policy.bypasses_quality() {
        return None;
    }
    let threshold = cluster.threshold?;
    let decision = match cluster.decision? {
        PlaylistUpdateClusterDecision::Accepted => PersistedPlaylistUpdateQualityDecision::Accepted,
        PlaylistUpdateClusterDecision::Rejected => PersistedPlaylistUpdateQualityDecision::Rejected,
        PlaylistUpdateClusterDecision::TechnicalError => return None,
    };
    Some(PersistedPlaylistUpdateQualitySnapshot {
        threshold,
        baseline_count: cluster.baseline_count,
        candidate_count: cluster.candidate_count,
        achieved_quality: cluster.quality,
        decision,
    })
}

fn persisted_technical_state(
    cluster: &PlaylistUpdateClusterTelemetry,
    context: PersistedSnapshotContext,
) -> Option<PersistedPlaylistUpdateTechnicalState> {
    let source = cluster.source?;
    if cluster.technical_state == Some(PersistedPlaylistUpdateTechnicalState::Failed) {
        return cluster.technical_state;
    }
    if cluster.decision == Some(PlaylistUpdateClusterDecision::TechnicalError) {
        return Some(PersistedPlaylistUpdateTechnicalState::Failed);
    }
    if context.storage_failure && source == PlaylistUpdateDataSource::Provider {
        return Some(PersistedPlaylistUpdateTechnicalState::Failed);
    }
    if !context.technical_failure {
        return Some(PersistedPlaylistUpdateTechnicalState::Succeeded);
    }
    (source == PlaylistUpdateDataSource::Provider
        && context.provider_cluster_count == 1
        && matches!(context.provider_failure_evidence, ProviderFailureEvidence::TechnicalFailure))
    .then_some(PersistedPlaylistUpdateTechnicalState::Failed)
}

pub(super) fn persisted_cluster_snapshot(
    cluster: &PlaylistUpdateClusterTelemetry,
    context: PersistedSnapshotContext,
) -> PersistedPlaylistUpdateClusterSnapshot {
    let quality = persisted_quality_snapshot(cluster, context.effective_policy);
    PersistedPlaylistUpdateClusterSnapshot {
        policy: cluster.source.and(context.request_policy),
        source: cluster.source,
        quality_guard_threshold: (quality.is_none() && cluster.source.is_some())
            .then_some(cluster.threshold.unwrap_or(0)),
        quality,
        active_count: cluster.active_count,
        technical_state: persisted_technical_state(cluster, context),
    }
}

pub(super) fn persist_finalized_cluster_snapshots(
    playlist_download_result: &mut PlaylistDownloadResult,
    request_policy: Option<InputRefreshPolicy>,
    storage_error: Option<&TuliproxError>,
) {
    let technical_failure = storage_error.is_some()
        || playlist_download_result.partial
        || !playlist_download_result.download_err.is_empty();
    let provider_failure_evidence = playlist_download_result.provider_failure_evidence;
    let snapshots = playlist_download_result.input_telemetry.as_ref().map_or_else(Vec::new, |telemetry| {
        let provider_cluster_count = telemetry
            .clusters
            .iter()
            .filter(|cluster| cluster.requested && cluster.source == Some(PlaylistUpdateDataSource::Provider))
            .count();
        telemetry
            .clusters
            .iter()
            .filter(|cluster| cluster.requested)
            .map(|cluster| {
                let snapshot = persisted_cluster_snapshot(
                    cluster,
                    PersistedSnapshotContext {
                        effective_policy: telemetry.refresh_policy,
                        request_policy,
                        technical_failure,
                        storage_failure: storage_error.is_some(),
                        provider_failure_evidence,
                        provider_cluster_count,
                    },
                );
                (cluster.cluster, snapshot)
            })
            .collect()
    });

    let Some(mut pending_status) = playlist_download_result.pending_status.take() else {
        return;
    };
    let mut changed = pending_status.status_changed;
    for (cluster, snapshot) in snapshots {
        changed |= match pending_status.cluster_status_source {
            ClusterStatusSource::PerCluster => {
                input_cache::replace_cluster_snapshot(&mut pending_status.status, cluster.as_ref(), snapshot)
            }
            ClusterStatusSource::Default => input_cache::replace_cluster_snapshot_from_default_status(
                &mut pending_status.status,
                cluster.as_ref(),
                snapshot,
            ),
        };
    }
    if changed {
        input_cache::save_input_status(&pending_status.storage_path, &pending_status.status);
    }
}
