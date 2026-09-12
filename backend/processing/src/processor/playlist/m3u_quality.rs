//! M3U's document adapter at the existing locked input acquisition boundary.
use super::ingest::{cluster_is_configured, PIPELINE_TRANSPARENCY_CLUSTERS};
use shared::model::{M3uPlaylistItem, UpdateQualityPolicy};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tuliprox_core::model::{evaluate_update_quality, AppConfig, ClusterForceUpdate, ConfigInput};
use tuliprox_iptv::provider::PlaylistFetch;
use tuliprox_repository::{get_input_m3u_playlist_file_path, get_input_storage_path, load_input_m3u_playlist};

/// Called while the existing per-input lock protects baseline, decision and subsequent persistence.
pub(super) async fn effective_m3u_fetch(
    config: &Arc<AppConfig>,
    input: &ConfigInput,
    policy: UpdateQualityPolicy,
    mut fetch: PlaylistFetch,
) -> PlaylistFetch {
    if !fetch.is_ok() {
        return fetch;
    }
    // Match the existing M3U writer's last-wins primary key population. This also
    // keeps the target source and raw group catalogs consistent with its BTree.
    let mut winners = HashMap::new();
    for (group_index, group) in fetch.groups.iter_mut().enumerate() {
        for item in std::mem::take(&mut group.channels) {
            winners.insert(M3uPlaylistItem::from(&item).provider_id, (group_index, item));
        }
    }
    let mut winners: Vec<_> = winners.into_values().collect();
    winners.sort_by_key(|(group, item)| (*group, item.header.source_ordinal));
    for (group_index, item) in winners {
        fetch.groups[group_index].channels.push(item);
    }
    fetch.groups.retain(|group| !group.channels.is_empty());

    let needs_previous = PIPELINE_TRANSPARENCY_CLUSTERS.into_iter().any(|cluster| {
        !cluster_is_configured(input, cluster)
            || (policy == UpdateQualityPolicy::Enforce
                && input.options.as_ref().is_some_and(|options| options.update_quality.threshold(cluster) > 0))
    });
    let previous = if needs_previous {
        let storage_dir = config.config.load().storage_dir.clone();
        let loaded = async {
            let storage = get_input_storage_path(&input.name, &storage_dir).await.map_err(|error| {
                shared::error::TuliproxError::RepositoryM3u(format!("Cannot read M3U baseline: {error}"))
            })?;
            load_input_m3u_playlist(config, &get_input_m3u_playlist_file_path(&storage, &input.name)).await
        }
        .await;
        match loaded {
            Ok(groups) => groups,
            Err(error) => return PlaylistFetch::failed(error),
        }
    } else {
        Vec::new()
    };
    let mut retained = Vec::new();
    for cluster in PIPELINE_TRANSPARENCY_CLUSTERS {
        if !cluster_is_configured(input, cluster) {
            retained.push(cluster);
            continue;
        }
        let threshold = input.options.as_ref().map_or(0, |options| options.update_quality.threshold(cluster));
        let candidate_count =
            fetch.groups.iter().filter(|group| group.xtream_cluster == cluster).map(|group| group.channels.len()).sum();
        let current_count = previous
            .iter()
            .filter(|group| group.xtream_cluster == cluster)
            .map(|group| group.channels.len())
            .reduce(|total, count| total + count);
        if policy == UpdateQualityPolicy::Bypass {
            fetch.force_updates.push(ClusterForceUpdate { cluster, candidate_count, configured_threshold: threshold });
            continue;
        }
        let decision = evaluate_update_quality(current_count, candidate_count, threshold);
        if let Some(rejection) = decision.rejection(cluster) {
            fetch.quality_rejections.push(rejection);
            retained.push(cluster);
        } else if let Some(acceptance) = decision.acceptance(cluster) {
            fetch.quality_acceptances.push(acceptance);
        }
    }
    // Quality decisions must not reorder accepted candidate groups. Append retained
    // groups in their existing persisted order, without regrouping either source.
    fetch.groups.retain(|group| !retained.contains(&group.xtream_cluster));
    fetch.groups.extend(previous.into_iter().filter(|group| retained.contains(&group.xtream_cluster)));
    // Retention can combine an old cluster with a newly reclassified primary key.
    // The existing document BTree cannot represent both; fail before publication
    // instead of losing a retained row or exposing different target/catalog facts.
    let mut keys = HashSet::new();
    if fetch
        .groups
        .iter()
        .flat_map(|group| &group.channels)
        .any(|item| !keys.insert(M3uPlaylistItem::from(item).provider_id))
    {
        return PlaylistFetch::failed(shared::error::TuliproxError::RepositoryM3u(
            "Conflicting M3U keys across retained and candidate clusters".to_owned(),
        ));
    }
    fetch
}
