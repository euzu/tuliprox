use super::{xtream_get_file_path, BATCH_SIZE};
use crate::bplustree::{ensure_distinct_sidecar_lock_domains, BPlusTreeError, BPlusTreeQuery, BPlusTreeUpdate};
use log::error;
use shared::{
    error::TuliproxError,
    model::{
        ClusterFlags, LiveStreamProperties, SeriesStreamProperties, StreamProperties, VideoStreamProperties,
        XtreamCluster, XtreamPlaylistItem,
    },
};
use std::{collections::HashMap, io, io::Error, path::Path, sync::Arc};
use tuliprox_core::model::AppConfig;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum PreserveDetailsOutcome {
    SourceMissing,
    Merged { scanned: usize, updated: usize },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum DetailPreservationOperation {
    Query,
    BatchWrite,
    Commit,
}

fn write_preserved_detail_batch<F>(
    staging_tree: &mut BPlusTreeUpdate<u32, XtreamPlaylistItem>,
    staging_path: &Path,
    updates: &mut Vec<(u32, XtreamPlaylistItem)>,
    before_operation: &mut F,
) -> Result<usize, TuliproxError>
where
    F: FnMut(DetailPreservationOperation) -> io::Result<()>,
{
    if updates.is_empty() {
        return Ok(0);
    }
    let batch_len = updates.len();
    let refs: Vec<(&u32, &XtreamPlaylistItem)> = updates.iter().map(|(id, item)| (id, item)).collect();
    before_operation(DetailPreservationOperation::BatchWrite)
        .and_then(|()| staging_tree.update_batch(&refs).map(|_| ()).map_err(BPlusTreeError::to_io))
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to update staging Xtream tree {} during detail preservation: {error}",
                staging_path.display()
            ))
        })?;
    updates.clear();
    Ok(batch_len)
}

pub(super) fn preserve_details_input_xtream_playlist_cluster_to_disk(
    published_path: &Path,
    staging_path: &Path,
) -> Result<PreserveDetailsOutcome, TuliproxError> {
    preserve_details_input_xtream_playlist_cluster_to_disk_with_hook(published_path, staging_path, |_| Ok(()))
}

fn preserve_details_input_xtream_playlist_cluster_to_disk_with_hook<F>(
    published_path: &Path,
    staging_path: &Path,
    mut before_operation: F,
) -> Result<PreserveDetailsOutcome, TuliproxError>
where
    F: FnMut(DetailPreservationOperation) -> io::Result<()>,
{
    ensure_distinct_sidecar_lock_domains(published_path, staging_path)
        .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;

    let mut published_tree = match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(published_path) {
        Ok(tree) => tree,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PreserveDetailsOutcome::SourceMissing);
        }
        Err(error) => {
            return Err(TuliproxError::RepositoryXtream(format!(
                "Failed to open published Xtream tree {} for detail preservation: {error}",
                published_path.display()
            )));
        }
    };

    let mut staging_tree =
        BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(staging_path).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to open staging Xtream tree {} for detail preservation: {error}",
                staging_path.display()
            ))
        })?;

    let mut pending_updates: Vec<(u32, XtreamPlaylistItem)> = Vec::with_capacity(BATCH_SIZE);
    let mut scanned_count = 0usize;
    let mut updated_count = 0usize;
    for entry in published_tree.iter() {
        let (_, old_item) = entry.map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to read published Xtream tree {} during detail preservation: {error}",
                published_path.display()
            ))
        })?;
        scanned_count = scanned_count.saturating_add(1);
        if let Some(old_props) = old_item.additional_properties.as_ref() {
            if old_props.has_details() {
                let staging_item = before_operation(DetailPreservationOperation::Query)
                    .and_then(|()| staging_tree.query(&old_item.provider_id).map_err(BPlusTreeError::to_io))
                    .map_err(|error| {
                        TuliproxError::RepositoryXtream(format!(
                            "Failed to query staging Xtream tree {} for provider {}: {error}",
                            staging_path.display(),
                            old_item.provider_id
                        ))
                    })?;
                if let Some(mut new_item) = staging_item {
                    if let Some(new_props) = new_item.additional_properties.as_mut() {
                        if merge_preserved_stream_properties(new_props, old_props) {
                            pending_updates.push((new_item.provider_id, new_item));
                            if pending_updates.len() >= BATCH_SIZE {
                                updated_count = updated_count.saturating_add(write_preserved_detail_batch(
                                    &mut staging_tree,
                                    staging_path,
                                    &mut pending_updates,
                                    &mut before_operation,
                                )?);
                            }
                        }
                    }
                }
            }
        }
    }

    updated_count = updated_count.saturating_add(write_preserved_detail_batch(
        &mut staging_tree,
        staging_path,
        &mut pending_updates,
        &mut before_operation,
    )?);
    before_operation(DetailPreservationOperation::Commit).and_then(|()| staging_tree.commit()).map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Failed to commit staging Xtream tree {} after detail preservation: {error}",
            staging_path.display()
        ))
    })?;

    Ok(PreserveDetailsOutcome::Merged { scanned: scanned_count, updated: updated_count })
}

#[cfg(test)]
pub(super) fn preserve_details_with_injected_operation_failure(
    published_path: &Path,
    staging_path: &Path,
    failure: DetailPreservationOperation,
) -> Result<PreserveDetailsOutcome, TuliproxError> {
    preserve_details_input_xtream_playlist_cluster_to_disk_with_hook(published_path, staging_path, move |operation| {
        if operation == failure {
            Err(io::Error::other(format!("injected {failure:?} failure")))
        } else {
            Ok(())
        }
    })
}

pub(super) const fn cluster_flag(cluster: XtreamCluster) -> ClusterFlags {
    match cluster {
        XtreamCluster::Live => ClusterFlags::Live,
        XtreamCluster::Video => ClusterFlags::Vod,
        XtreamCluster::Series => ClusterFlags::Series,
    }
}

// Checks if the info has changed after the last update
pub fn needs_update_info_details(new_stream_props: &StreamProperties, old_stream_props: &StreamProperties) -> bool {
    let new_modified = new_stream_props.get_last_modified();
    let old_modified = old_stream_props.get_last_modified();

    match (new_modified, old_modified) {
        (Some(new_ts), Some(old_ts)) => new_ts > old_ts,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Merges persisted fields from old stream properties into freshly fetched properties.
///
/// This keeps long-lived metadata stable across full playlist rewrites:
/// - VOD/Series `details` are preserved when incoming provider metadata is not newer.
/// - Learned Live fields are merged through [`LiveStreamProperties::merge_learned_metadata_from`].
/// - Live catchup remains separate provider metadata and is copied only when missing.
pub fn merge_preserved_stream_properties(
    new_stream_props: &mut StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {
    let preserve_info_details =
        old_stream_props.has_details() && !needs_update_info_details(new_stream_props, old_stream_props);

    match (new_stream_props, old_stream_props) {
        (StreamProperties::Video(v_new), StreamProperties::Video(v_old)) => {
            let mut changed = false;

            if preserve_info_details && v_old.details.is_some() && v_new.details != v_old.details {
                v_new.details.clone_from(&v_old.details);
                changed = true;
            }

            if v_new.tmdb.is_none() && v_old.tmdb.is_some() {
                v_new.tmdb = v_old.tmdb;
                changed = true;
            }

            changed
        }
        (StreamProperties::Series(s_new), StreamProperties::Series(s_old)) => {
            let mut changed = false;

            if preserve_info_details && s_old.details.is_some() && s_new.details != s_old.details {
                s_new.details.clone_from(&s_old.details);
                changed = true;
            }

            if s_new.tmdb.is_none() && s_old.tmdb.is_some() {
                s_new.tmdb = s_old.tmdb;
                changed = true;
            }

            if s_new.release_date.is_none() && s_old.release_date.is_some() {
                s_new.release_date.clone_from(&s_old.release_date);
                changed = true;
            }

            changed
        }
        (StreamProperties::Live(l_new), StreamProperties::Live(l_old)) => {
            let mut changed = l_new.merge_learned_metadata_from(l_old);

            if l_new.catchup.is_none() && l_old.catchup.is_some() {
                l_new.catchup.clone_from(&l_old.catchup);
                changed = true;
            }

            changed
        }
        _ => false,
    }
}

async fn persist_input_info(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    provider_id: u32,
    props: StreamProperties,
) -> Result<(), Error> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        let file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let xtream_path_clone = xtream_path.clone();
        let input_name_owned = input_name.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let _guard = file_lock;
            let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> =
                BPlusTreeUpdate::try_new_with_backoff(&xtream_path_clone).map_err(|err| {
                    Error::other(format!("failed to open BPlusTree for input {input_name_owned}: {err}"))
                })?;
            match tree.query(&provider_id) {
                Ok(Some(mut pli)) => {
                    pli.additional_properties = Some(props);
                    tree.update(&provider_id, pli).map_err(|err| {
                        Error::other(format!("failed to write {cluster} info for input {input_name_owned}: {err}"))
                    })?;
                    //rebuild_source_ordinal_index_if_present(&xtream_path_clone)
                    //    .map_err(|err| Error::other(format!("failed to rebuild sorted index for input {input_name_owned}: {err}")))?;
                }
                Ok(None) => {
                    error!("Could not find input entry for provider_id: {provider_id} and input: {input_name_owned}");
                }
                Err(err) => {
                    error!(
                        "Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name_owned}: {err}"
                    );
                }
            }
            Ok(())
        })
        .await
        .map_err(|err| Error::other(format!("failed to join blocking input info persist for {input_name}: {err}")))??;
    }
    Ok(())
}

pub async fn persist_input_info_batch(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    updates: Vec<(u32, StreamProperties)>,
) -> Result<(), Error> {
    if updates.is_empty() {
        return Ok(());
    }
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        let file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let xtream_path_clone = xtream_path.clone();
        let input_name_owned = input_name.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let _guard = file_lock;
            let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new_with_backoff(&xtream_path_clone)
                .map_err(|err| Error::other(format!("failed to open BPlusTree for input {input_name_owned}: {err}")))?;

            // Keep only the latest update per provider id to avoid duplicate reads/writes.
            let mut deduped_updates: HashMap<u32, StreamProperties> = HashMap::with_capacity(updates.len());
            for (provider_id, props) in updates {
                deduped_updates.insert(provider_id, props);
            }

            let mut updated_plis = Vec::with_capacity(deduped_updates.len());
            for (provider_id, props) in deduped_updates {
                match tree.query(&provider_id) {
                    Ok(Some(mut pli)) => {
                        pli.additional_properties = Some(props);
                        updated_plis.push((provider_id, pli));
                    }
                    Ok(None) => {
                        error!("Could not find input entry for provider_id: {provider_id} and input: {input_name_owned}");
                    }
                    Err(err) => {
                        error!("Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name_owned}: {err}");
                    }
                }
            }

            if !updated_plis.is_empty() {
                let refs: Vec<(&u32, &XtreamPlaylistItem)> = updated_plis.iter()
                    .map(|(id, pli)| (id, pli))
                    .collect();
                tree.update_batch(&refs).map_err(|err| Error::other(format!("failed to write batch {cluster} info for input {input_name_owned}: {err}")))?;
                //rebuild_source_ordinal_index_if_present(&xtream_path_clone)
                //    .map_err(|err| Error::other(format!("failed to rebuild sorted index for input {input_name_owned}: {err}")))?;
            }
            Ok(())
        }).await.map_err(|err| Error::other(format!("failed to join blocking input info batch persist for {input_name}: {err}")))??;
    }
    Ok(())
}

pub async fn persist_input_vod_info(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    provider_id: u32,
    props: &VideoStreamProperties,
) -> Result<(), Error> {
    persist_input_info(
        app_config,
        storage_path,
        cluster,
        input_name,
        provider_id,
        StreamProperties::Video(Box::new(props.clone())),
    )
    .await
}

pub async fn persist_input_live_info(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    provider_id: u32,
    props: &LiveStreamProperties,
) -> Result<(), Error> {
    persist_input_info(
        app_config,
        storage_path,
        cluster,
        input_name,
        provider_id,
        StreamProperties::Live(Box::new(props.clone())),
    )
    .await
}

pub async fn persist_input_live_info_batch(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    updates: Vec<(u32, LiveStreamProperties)>,
) -> Result<(), Error> {
    let batch = updates.into_iter().map(|(id, props)| (id, StreamProperties::Live(Box::new(props)))).collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
}

pub async fn persist_input_vod_info_batch(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    updates: Vec<(u32, VideoStreamProperties)>,
) -> Result<(), Error> {
    let batch = updates.into_iter().map(|(id, props)| (id, StreamProperties::Video(Box::new(props)))).collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
}

pub async fn persists_input_series_info(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    provider_id: u32,
    props: &SeriesStreamProperties,
) -> Result<(), Error> {
    persist_input_info(
        app_config,
        storage_path,
        cluster,
        input_name,
        provider_id,
        StreamProperties::Series(Box::new(props.clone())),
    )
    .await
}

pub async fn persist_input_series_info_batch(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    input_name: &str,
    updates: Vec<(u32, SeriesStreamProperties)>,
) -> Result<(), Error> {
    let batch = updates.into_iter().map(|(id, props)| (id, StreamProperties::Series(Box::new(props)))).collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
}
