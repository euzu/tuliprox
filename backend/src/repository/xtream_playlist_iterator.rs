use crate::api::model::AppState;
use crate::model::ConfigTarget;
use crate::model::{xtream_mapping_option_from_target_options, AppConfig, ProxyUserCredentials};
use crate::repository::get_file_path_for_db_index;
use crate::repository::user_get_bouquet_filter;
use crate::repository::{open_playlist_reader, LockedReceiverStream};
use crate::repository::{xtream_get_file_path, xtream_get_storage_path};
use futures::Stream;
use log::error;
use shared::error::TuliproxError;
use shared::model::{PlaylistItemType, TargetType, XtreamCluster, XtreamMappingOptions, XtreamPlaylistItem};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task;

pub struct XtreamPlaylistIterator {
    inner: LockedReceiverStream<Result<(XtreamPlaylistItem, bool), TuliproxError>>,
}

fn is_cluster_allowed_for_user(user: &ProxyUserCredentials, cluster: XtreamCluster) -> bool {
    user.allows_cluster(cluster)
}

impl XtreamPlaylistIterator {
    fn empty() -> Self {
        // Ghost Channel pattern
        // When you immediately drop the sender with `_`, the channel is closed and receiver gets None.
        let (_tx, rx) = mpsc::channel::<Result<(XtreamPlaylistItem, bool), TuliproxError>>(1);
        Self { inner: LockedReceiverStream::new_empty(rx) }
    }

    pub async fn new(
        cluster: XtreamCluster,
        app_config: &AppConfig,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        // TODO use playlist memory cache and keep sorted
        if !is_cluster_allowed_for_user(user, cluster) {
            return Ok(Self::empty());
        }

        debug_assert!(target.get_xtream_output().is_some());
        let config = app_config.config.load();
        if let Some(storage_path) = xtream_get_storage_path(&config, target.name.as_str()) {
            let xtream_path = xtream_get_file_path(&storage_path, cluster);
            if !xtream_path.exists() {
                return Err(TuliproxError::Config(format!("No {cluster} entries found for target {}", target.name)));
            }
            // Hold iter_lock for the stream lifetime (LockedReceiverStream), and bg_lock for the background reader.
            let iter_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let bg_lock = app_config.file_locks.read_lock(&xtream_path).await;

            let filter =
                user_get_bouquet_filter(&config, &user.username, category_id, TargetType::Xtream, cluster).await;
            // Parse bouquet filter (strings) once into u32 set to minimize per-item allocations
            let filter_ids: Option<HashSet<u32>> = filter.as_ref().map(|set| {
                set.iter()
                    .filter_map(|s| {
                        s.parse::<u32>()
                            .map_err(|e| {
                                error!("Failed to parse bouquet filter id '{s}': {e}");
                                e
                            })
                            .ok()
                    })
                    .collect()
            });
            let hide_adult = user.hide_adult;

            let xtream_path = xtream_path.clone();
            let index_path = get_file_path_for_db_index(&xtream_path);
            let (tx, rx) = mpsc::channel::<Result<(XtreamPlaylistItem, bool), TuliproxError>>(256);

            let xtream_path_for_log = xtream_path.clone();
            let join_error_tx = tx.clone();
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
                        let _ = tx.blocking_send(Err(err));
                        return;
                    }
                };

                let mut pending: Option<XtreamPlaylistItem> = None;
                for entry in reader {
                    let item = match entry {
                        Ok((_, item)) => item,
                        Err(err) => {
                            error!("Skipping unreadable Xtream playlist entry: {err}");
                            continue;
                        }
                    };

                    if !Self::matches_filters(cluster, filter_ids.as_ref(), hide_adult, &item) {
                        continue;
                    }

                    if let Some(prev) = pending.replace(item) {
                        if tx.blocking_send(Ok((prev, true))).is_err() {
                            return;
                        }
                    }
                }

                if let Some(last) = pending {
                    let _ = tx.blocking_send(Ok((last, false)));
                }
            });
            tokio::spawn(async move {
                if let Err(err) = handle.await {
                    error!(
                        "Xtream playlist iterator task failed for {} (cluster {cluster}): {err}",
                        xtream_path_for_log.display()
                    );
                    let _ = join_error_tx
                        .send(Err(TuliproxError::RepositoryXtream(format!(
                            "Xtream playlist iterator task failed for {}: {err}",
                            xtream_path_for_log.display()
                        ))))
                        .await;
                }
            });

            Ok(Self { inner: LockedReceiverStream::new(rx, iter_lock) })
        } else {
            Err(TuliproxError::Config(format!("Failed to find xtream storage for target {}", target.name)))
        }
    }

    fn matches_filters(
        cluster: XtreamCluster,
        filter_ids: Option<&HashSet<u32>>,
        hide_adult: bool,
        item: &XtreamPlaylistItem,
    ) -> bool {
        // We can't serve episodes within series
        if cluster == XtreamCluster::Series
            && !matches!(item.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
        {
            return false;
        }

        // category_id-Filter
        if let Some(set) = filter_ids {
            if !set.contains(&item.category_id) {
                return false;
            }
        }

        if hide_adult
            && (crate::model::ProxyUserCredentials::is_adult_group(item.group.as_ref())
                || crate::model::ProxyUserCredentials::stream_props_marked_adult(item.additional_properties.as_ref()))
        {
            return false;
        }

        true
    }
}

impl Stream for XtreamPlaylistIterator {
    type Item = Result<(XtreamPlaylistItem, bool), TuliproxError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

pub struct XtreamPlaylistJsonIterator {
    inner: XtreamPlaylistIterator,
    options: Option<XtreamMappingOptions>,
}

impl XtreamPlaylistJsonIterator {
    pub async fn new(
        cluster: XtreamCluster,
        app_state: &Arc<AppState>,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        let xtream_output = target.get_xtream_output().ok_or_else(|| {
            TuliproxError::Config(format!("Unexpected: xtream output required for target {}", target.name))
        })?;
        if !is_cluster_allowed_for_user(user, cluster) {
            return Ok(Self {
                inner: XtreamPlaylistIterator::empty(),
                options: None,
            });
        }
        let encrypt_secret = app_state.get_encrypt_secret();
        let options = xtream_mapping_option_from_target_options(
            target,
            xtream_output,
            &app_state.app_config,
            user,
            encrypt_secret,
        )?;
        Ok(Self {
            inner: XtreamPlaylistIterator::new(cluster, &app_state.app_config, target, category_id, user).await?,
            options: Some(options),
        })
    }
}


impl Stream for XtreamPlaylistJsonIterator {
    type Item = Result<(String, bool), TuliproxError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok((pli, has_next)))) => {
                let Some(options) = self.options.as_ref() else {
                    return Poll::Ready(None);
                };
                let json = serde_json::to_string(&pli.to_document(options))
                    .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()));
                Poll::Ready(Some(json.map(|json| (json, has_next))))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::{is_cluster_allowed_for_user, XtreamPlaylistIterator};
    use crate::model::ProxyUserCredentials;
    use crate::repository::LockedReceiverStream;
    use shared::{
        model::{ClusterFlags, PlaylistItemType, XtreamCluster, XtreamPlaylistItem},
        utils::Internable,
    };
    use futures::StreamExt;
    use tokio::sync::mpsc;

    #[test]
    fn cluster_guard_respects_user_cluster_flags() {
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::Live | ClusterFlags::Series;

        assert!(is_cluster_allowed_for_user(&user, XtreamCluster::Live));
        assert!(!is_cluster_allowed_for_user(&user, XtreamCluster::Video));
        assert!(is_cluster_allowed_for_user(&user, XtreamCluster::Series));
    }

    #[tokio::test]
    async fn empty_iterator_yields_no_items() {
        let mut iter = XtreamPlaylistIterator::empty();
        assert!(iter.next().await.is_none());
    }

    #[tokio::test]
    async fn iterator_forwards_one_storage_error_then_ends() {
        let (tx, rx) = mpsc::channel(2);
        let item = XtreamPlaylistItem {
            virtual_id: 1,
            provider_id: 1,
            name: "name".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "group".intern(),
            title: "title".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://example.test/live.ts".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 1,
            input_name: "input".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "1".intern(),
            upstream_user_agent: None,
        };
        assert!(tx.send(Ok((item, true))).await.is_ok());
        assert!(tx.send(Err(shared::error::TuliproxError::RepositoryXtream("corrupt page".into()))).await.is_ok());
        drop(tx);

        let mut iterator = XtreamPlaylistIterator { inner: LockedReceiverStream::new_empty(rx) };
        assert!(iterator.next().await.is_some_and(|entry| entry.is_ok()));
        assert!(iterator.next().await.is_some_and(|entry| entry.is_err()));
        assert!(iterator.next().await.is_none());
    }
}
