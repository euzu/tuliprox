use crate::model::ConfigTarget;
use crate::model::{xtream_mapping_option_from_target_options, AppConfig, ProxyUserCredentials};
use crate::repository::{LockedReceiverStream, open_playlist_reader};
use crate::repository::user_get_bouquet_filter;
use crate::repository::{xtream_get_file_path, xtream_get_storage_path};
use futures::Stream;
use log::error;
use shared::error::{TuliproxError, info_err, info_err_res};
use shared::model::{PlaylistItemType, TargetType, XtreamCluster, XtreamMappingOptions, XtreamPlaylistItem};
use std::collections::HashSet;
use crate::repository::get_file_path_for_db_index;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task;

pub struct XtreamPlaylistIterator {
    inner: LockedReceiverStream<(XtreamPlaylistItem, bool)>,
}

impl XtreamPlaylistIterator {
    pub async fn new(
        cluster: XtreamCluster,
        app_config: &AppConfig,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {

        // TODO use playlist memory cache and keep sorted

        debug_assert!(target.get_xtream_output().is_some());
        let config = app_config.config.load();
        if let Some(storage_path) = xtream_get_storage_path(&config, target.name.as_str()) {
            let xtream_path = xtream_get_file_path(&storage_path, cluster);
            if !xtream_path.exists() {
                return info_err_res!("No {cluster} entries found for target {}", &target.name);
            }
            // Hold iter_lock for the stream lifetime (LockedReceiverStream), and bg_lock for the background reader.
            let iter_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let bg_lock = app_config.file_locks.read_lock(&xtream_path).await;

            let filter = user_get_bouquet_filter(&config, &user.username, category_id, TargetType::Xtream, cluster).await;
            // Parse bouquet filter (strings) once into u32 set to minimize per-item allocations
            let filter_ids: Option<HashSet<u32>> = filter.as_ref().map(|set| {
                set.iter().filter_map(|s| {
                    s.parse::<u32>().map_err(|e| {
                        error!("Failed to parse bouquet filter id '{s}': {e}");
                        e
                    }).ok()
                }).collect()
            });

            let xtream_path = xtream_path.clone();
            let index_path = get_file_path_for_db_index(&xtream_path);
            let (tx, rx) = mpsc::channel::<(XtreamPlaylistItem, bool)>(256);

            let xtream_path_for_log = xtream_path.clone();
            let handle = task::spawn_blocking(move || {
                let _guard = bg_lock;
                let reader = match open_playlist_reader::<u32, XtreamPlaylistItem, u32>(
                    &xtream_path,
                    &index_path,
                    Some("Sorted index error, falling back to unsorted"),
                ) {
                    Ok(reader) => reader,
                    Err(err) => {
                        error!(
                            "Failed to open Xtream playlist DB {} (cluster {cluster}): {err}",
                            xtream_path.display()
                        );
                        return;
                    }
                };

                let mut pending: Option<XtreamPlaylistItem> = None;
                for entry in reader {
                    let item = match entry {
                        Ok((_, item)) => item,
                        Err(err) => {
                            error!("Error reading sorted index: {err}");
                            continue;
                        }
                    };

                    if !Self::matches_filters(cluster, filter_ids.as_ref(), &item) {
                        continue;
                    }

                    if let Some(prev) = pending.replace(item) {
                        if tx.blocking_send((prev, true)).is_err() {
                            return;
                        }
                    }
                }

                if let Some(last) = pending {
                    let _ = tx.blocking_send((last, false));
                }
            });
            tokio::spawn(async move {
                if let Err(err) = handle.await {
                    error!(
                        "Xtream playlist iterator task failed for {} (cluster {cluster}): {err}",
                        xtream_path_for_log.display()
                    );
                }
            });

            Ok(Self {
                inner: LockedReceiverStream::new(rx, iter_lock),
            })
        } else {
            info_err_res!("Failed to find xtream storage for target {}", &target.name)
        }
    }

    fn matches_filters(cluster: XtreamCluster, filter_ids: Option<&HashSet<u32>>, item: &XtreamPlaylistItem) -> bool {
        // We can't serve episodes within series
        if cluster == XtreamCluster::Series
            && !matches!(item.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo) {
            return false;
        }

        // category_id-Filter
        if let Some(set) = filter_ids {
            if !set.contains(&item.category_id) {
                return false;
            }
        }

        true
    }

}

impl Stream for XtreamPlaylistIterator {
    type Item = (XtreamPlaylistItem, bool);
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}


pub struct XtreamPlaylistJsonIterator {
    inner: XtreamPlaylistIterator,
    options: XtreamMappingOptions,
}

impl XtreamPlaylistJsonIterator {
    pub async fn new(
        cluster: XtreamCluster,
        config: &AppConfig,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        let xtream_output = target.get_xtream_output().ok_or_else(|| info_err!("Unexpected: xtream output required for target {}", target.name))?;
        let server_info = config.get_user_server_info(user);
        let options = xtream_mapping_option_from_target_options(
            target,
            xtream_output,
            config,
            user,
            Some(server_info.get_base_url().as_str()),
        );
        Ok(Self {
            inner: XtreamPlaylistIterator::new(cluster, config, target, category_id, user).await?,
            options,
        })
    }
}

impl Stream for XtreamPlaylistJsonIterator {
    type Item = (String, bool);
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some((pli, has_next))) => {
                let json = serde_json::to_string(&pli.to_document(&self.options))
                    .unwrap_or_else(|err| {
                        error!("Failed to serialize playlist item {}: {err}", pli.virtual_id);
                        "{}".to_string()
                    });
                Poll::Ready(Some((json, has_next)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
