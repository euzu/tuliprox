use crate::{
    get_file_path_for_db_index, user_get_bouquet_filter, xtream_get_file_path, xtream_get_item_for_stream_id,
    xtream_get_storage_path, LockedReceiverStream, PlaylistStorageState,
};
use futures::{Stream, StreamExt};
use log::error;
use shared::{
    error::TuliproxError,
    model::{TargetType, XtreamCluster, XtreamPlaylistItem},
};
use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::mpsc, task};
use tuliprox_core::{
    model::{xtream_mapping_option_from_target_options, AppConfig, Config, ConfigTarget, ProxyUserCredentials},
    utils::prepare_xtream_resource_hosts,
};

enum XtreamPlaylistIteratorInner {
    Disk(LockedReceiverStream<Result<(XtreamPlaylistItem, bool), TuliproxError>>),
    Memory(std::vec::IntoIter<XtreamPlaylistItem>),
}

pub struct XtreamPlaylistIterator {
    inner: XtreamPlaylistIteratorInner,
}

fn is_cluster_allowed_for_user(user: &ProxyUserCredentials, cluster: XtreamCluster) -> bool {
    user.allows_cluster(cluster)
}

/// Bouquet and content filter of one user for one Xtream cluster.
///
/// The playlist iterator and the single-item lookup below have to answer with the same item for
/// the same user, so the per-item checks live here and both use them.
struct XtreamItemFilter {
    cluster: XtreamCluster,
    filter_ids: Option<HashSet<u32>>,
    user_filter: Option<Arc<shared::foundation::Filter>>,
}

impl XtreamItemFilter {
    async fn new(
        config: &Config,
        cluster: XtreamCluster,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Self {
        let filter = user_get_bouquet_filter(config, &user.username, category_id, TargetType::Xtream, cluster).await;
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
        Self { cluster, filter_ids, user_filter: user.t_filter.clone() }
    }

    #[inline]
    fn matches(&self, item: &XtreamPlaylistItem) -> bool {
        if !crate::playlist_mem_cache::is_cluster_order_eligible(self.cluster, item.item_type) {
            return false;
        }

        // category_id-Filter
        if let Some(set) = &self.filter_ids {
            if !set.contains(&item.category_id) {
                return false;
            }
        }

        // per-user content filter (plan AND user), applied post-cache
        if let Some(filter) = &self.user_filter {
            let pli = shared::model::PlaylistItem::from(item);
            let provider = shared::foundation::ValueProvider { pli: &pli, match_as_ascii: false };
            if !filter.filter(&provider) {
                return false;
            }
        }

        true
    }
}

fn stream_playlist_entries(
    xtream_path: &std::path::Path,
    index_path: &std::path::Path,
    cluster: XtreamCluster,
    item_filter: &XtreamItemFilter,
    tx: &mpsc::Sender<Result<(XtreamPlaylistItem, bool), TuliproxError>>,
) {
    let mut pending: Option<XtreamPlaylistItem> = None;
    let visit_result = crate::visit_xtream_cluster_entries(xtream_path, index_path, cluster, |_key, item| {
        if !item_filter.matches(&item) {
            return true;
        }

        if let Some(prev) = pending.replace(item) {
            if tx.blocking_send(Ok((prev, true))).is_err() {
                return false;
            }
        }
        true
    });

    if let Err(err) = visit_result {
        error!("Failed to open Xtream playlist DB {} (cluster {cluster}): {err}", xtream_path.display());
        let _ = tx.blocking_send(Err(err));
        return;
    }

    if let Some(last) = pending {
        let _ = tx.blocking_send(Ok((last, false)));
    }
}

impl XtreamPlaylistIterator {
    fn empty() -> Self { Self { inner: XtreamPlaylistIteratorInner::Memory(Vec::new().into_iter()) } }

    pub async fn new(
        cluster: XtreamCluster,
        app_config: &AppConfig,
        playlists: &PlaylistStorageState,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        if !is_cluster_allowed_for_user(user, cluster) {
            return Ok(Self::empty());
        }

        debug_assert!(target.get_xtream_output().is_some());
        let config = app_config.config.load();
        let item_filter = XtreamItemFilter::new(&config, cluster, category_id, user).await;

        if target.use_memory_cache {
            let memory_items = {
                let data = playlists.data.read().await;
                data.get(&target.name).and_then(|storage| storage.xtream.as_ref()).map(|xtream_storage| {
                    let (tree, order) = xtream_storage.get_cluster(cluster);
                    let mut matching = Vec::new();
                    for (_, key) in order {
                        if let Some(item) = tree.query(key) {
                            if item_filter.matches(item) {
                                matching.push(item.clone());
                            }
                        }
                    }
                    matching
                })
            };

            if let Some(items) = memory_items {
                return Ok(Self { inner: XtreamPlaylistIteratorInner::Memory(items.into_iter()) });
            }
        }

        if let Some(storage_path) = xtream_get_storage_path(&config, target.name.as_str()) {
            let xtream_path = xtream_get_file_path(&storage_path, cluster);
            if !xtream_path.exists() {
                return Err(TuliproxError::Config(format!("No {cluster} entries found for target {}", target.name)));
            }
            // One read lock, shared by the stream (LockedReceiverStream) and the background reader.
            // A second read lock on the same path would deadlock behind a queued writer.
            let file_lock = Arc::new(app_config.file_locks.read_lock(&xtream_path).await);
            let bg_lock = Arc::clone(&file_lock);

            let xtream_path = xtream_path.clone();
            let index_path = get_file_path_for_db_index(&xtream_path);
            let (tx, rx) = mpsc::channel::<Result<(XtreamPlaylistItem, bool), TuliproxError>>(256);

            let xtream_path_for_log = xtream_path.clone();
            let join_error_tx = tx.clone();
            let handle = task::spawn_blocking(move || {
                let _guard = bg_lock;
                stream_playlist_entries(&xtream_path, &index_path, cluster, &item_filter, &tx);
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

            Ok(Self { inner: XtreamPlaylistIteratorInner::Disk(LockedReceiverStream::new_shared(rx, file_lock)) })
        } else {
            Err(TuliproxError::Config(format!("Failed to find xtream storage for target {}", target.name)))
        }
    }
}

/// Single item lookup of one user's Xtream output.
///
/// The `HDHomeRun` tuner endpoints resolve the guide numbers they find in the target guide index
/// with this instead of walking the playlists. It answers with the same item the iterators publish,
/// and the bouquet and content filter of every cluster are prepared once for all the candidates of
/// one request.
pub struct XtreamUserItemLookup<'a> {
    app_config: &'a Arc<AppConfig>,
    playlists: &'a PlaylistStorageState,
    target: &'a ConfigTarget,
    /// Filters of the requested clusters this user may watch.
    filters: Vec<XtreamItemFilter>,
}

impl<'a> XtreamUserItemLookup<'a> {
    pub async fn new(
        app_config: &'a Arc<AppConfig>,
        playlists: &'a PlaylistStorageState,
        target: &'a ConfigTarget,
        user: &ProxyUserCredentials,
        clusters: &[XtreamCluster],
    ) -> Self {
        let config = app_config.config.load();
        let mut filters = Vec::with_capacity(clusters.len());
        for &cluster in clusters {
            if is_cluster_allowed_for_user(user, cluster) {
                filters.push(XtreamItemFilter::new(&config, cluster, None, user).await);
            }
        }
        Self { app_config, playlists, target, filters }
    }

    /// The item the output publishes for `stream_id` in `cluster`, or `None` when the cluster is
    /// not served to this user, the playlist does not hold the id, or this user's filters exclude it.
    pub async fn get(&self, stream_id: u32, cluster: XtreamCluster) -> Option<XtreamPlaylistItem> {
        let filter = self.filters.iter().find(|filter| filter.cluster == cluster)?;
        match xtream_get_item_for_stream_id(stream_id, self.app_config, self.playlists, self.target, Some(cluster))
            .await
        {
            Ok(item) => filter.matches(&item).then_some(item),
            Err(err) => {
                log::debug!("No xtream playlist item {stream_id} in {cluster} for target {}: {err}", self.target.name);
                None
            }
        }
    }
}

impl Stream for XtreamPlaylistIterator {
    type Item = Result<(XtreamPlaylistItem, bool), TuliproxError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.inner {
            XtreamPlaylistIteratorInner::Disk(stream) => Pin::new(stream).poll_next(cx),
            XtreamPlaylistIteratorInner::Memory(iter) => {
                let next_item = iter.next().map(|item| {
                    let has_next = !iter.as_slice().is_empty();
                    (item, has_next)
                });
                Poll::Ready(next_item.map(Ok))
            }
        }
    }
}

type XtreamJsonStream = Pin<Box<dyn Stream<Item = Result<(String, bool), TuliproxError>> + Send>>;

pub struct XtreamPlaylistJsonIterator {
    inner: XtreamJsonStream,
}

impl XtreamPlaylistJsonIterator {
    pub async fn new(
        cluster: XtreamCluster,
        app_config: &Arc<AppConfig>,
        playlists: &PlaylistStorageState,
        target: &ConfigTarget,
        category_id: Option<u32>,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        let xtream_output = target.get_xtream_output().ok_or_else(|| {
            TuliproxError::Config(format!("Unexpected: xtream output required for target {}", target.name))
        })?;
        if !is_cluster_allowed_for_user(user, cluster) {
            return Ok(Self { inner: Box::pin(futures::stream::empty()) });
        }
        let encrypt_secret = app_config.get_encrypt_secret();
        let options = Arc::new(xtream_mapping_option_from_target_options(
            target,
            xtream_output,
            app_config,
            user,
            encrypt_secret,
        )?);
        let raw = XtreamPlaylistIterator::new(cluster, app_config, playlists, target, category_id, user).await?;
        let inner = raw.then(move |entry| {
            let options = Arc::clone(&options);
            async move {
                let (item, has_next) = entry?;
                prepare_xtream_resource_hosts(&item, &options).await;
                let json = serde_json::to_string(&item.to_document(&options))
                    .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;
                Ok((json, has_next))
            }
        });
        Ok(Self { inner: Box::pin(inner) })
    }
}

impl Stream for XtreamPlaylistJsonIterator {
    type Item = Result<(String, bool), TuliproxError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_cluster_allowed_for_user, XtreamItemFilter, XtreamPlaylistIterator, XtreamPlaylistIteratorInner};
    use crate::{
        bplustree::BPlusTree,
        test_config::{app_config_in, target_with_output, xtream_playlist_item},
        xtream_get_file_path, xtream_get_storage_path, LockedReceiverStream, PlaylistStorageState,
    };
    use futures::{poll, StreamExt};
    use shared::{
        model::{ClusterFlags, PlaylistItemType, VirtualId, XtreamCluster, XtreamPlaylistItem},
        utils::Internable,
    };
    use std::time::Duration;
    use tokio::{sync::mpsc, time::timeout};
    use tuliprox_core::model::{ProxyUserCredentials, TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput};

    #[test]
    fn cluster_guard_respects_user_cluster_flags() {
        let mut user = ProxyUserCredentials::default();
        user.output_clusters = ClusterFlags::Live | ClusterFlags::Series;

        assert!(is_cluster_allowed_for_user(&user, XtreamCluster::Live));
        assert!(!is_cluster_allowed_for_user(&user, XtreamCluster::Video));
        assert!(is_cluster_allowed_for_user(&user, XtreamCluster::Series));
    }

    #[test]
    fn matches_filters_applies_user_content_filter() {
        let make_item = |group: &str| XtreamPlaylistItem {
            virtual_id: VirtualId::new(1),
            provider_id: 1,
            name: "name".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: group.intern(),
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
        let filter = std::sync::Arc::new(
            shared::foundation::get_filter(r#"NOT Group ~ "^VIP.*""#, None).expect("filter parses"),
        );

        let without_filter = XtreamItemFilter { cluster: XtreamCluster::Live, filter_ids: None, user_filter: None };
        let with_filter =
            XtreamItemFilter { cluster: XtreamCluster::Live, filter_ids: None, user_filter: Some(filter) };

        // no filter: everything passes
        assert!(without_filter.matches(&make_item("VIP Sports")));
        // filter hides matching groups, keeps the rest
        assert!(!with_filter.matches(&make_item("VIP Sports")));
        assert!(with_filter.matches(&make_item("News")));
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
            virtual_id: VirtualId::new(1),
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
        assert!(tx.send(Err(shared::error::TuliproxError::RepositoryXtream("corrupt page"))).await.is_ok());
        drop(tx);

        let mut iterator =
            XtreamPlaylistIterator { inner: XtreamPlaylistIteratorInner::Disk(LockedReceiverStream::new_empty(rx)) };
        assert!(iterator.next().await.is_some_and(|entry| entry.is_ok()));
        assert!(iterator.next().await.is_some_and(|entry| entry.is_err()));
        assert!(iterator.next().await.is_none());
    }

    /// The iterator holds one read lock shared by its stream and its producer. The
    /// old shape took two on the same path, which deadlocked behind a queued writer.
    #[tokio::test]
    async fn iterator_holds_one_shared_read_lock_until_it_is_dropped() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let app_config = app_config_in(temp.path());
        let target = target_with_output(
            "lock-target",
            TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        );
        let storage_path = xtream_get_storage_path(&app_config.config.load(), &target.name).expect("storage path");
        let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
        std::fs::create_dir_all(xtream_path.parent().expect("parent dir")).expect("storage dir should be created");
        // More entries than the channel depth: the producer is still active while nothing is consumed.
        let mut tree = BPlusTree::new();
        for id in 1..=400u32 {
            tree.insert(id, xtream_playlist_item(id));
        }
        tree.store(&xtream_path).expect("xtream playlist should persist");

        let mut iterator = XtreamPlaylistIterator::new(
            XtreamCluster::Live,
            &app_config,
            &PlaylistStorageState::default(),
            &target,
            None,
            &ProxyUserCredentials::default(),
        )
        .await
        .expect("iterator should open");
        assert!(iterator.next().await.is_some_and(|entry| entry.is_ok()));

        let mut write = Box::pin(app_config.file_locks.write_lock(&xtream_path));
        assert!(poll!(&mut write).is_pending(), "writer must wait while the iterator is alive");

        drop(iterator);
        let guard = timeout(Duration::from_secs(5), write)
            .await
            .expect("queued writer must complete once iterator and producer are gone");
        drop(guard);
        assert!(app_config.file_locks.try_write_lock(&xtream_path).await.is_ok(), "no read guard may be left behind");
    }

    fn sample_series_item_with_details(id: u32) -> XtreamPlaylistItem {
        use shared::model::{
            SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties,
            SeriesStreamProperties, StreamProperties,
        };

        let details = SeriesStreamDetailProperties {
            year: Some(2024),
            seasons: Some(vec![SeriesStreamDetailSeasonProperties {
                name: "Season 1".intern(),
                season_number: 1,
                episode_count: 5,
                overview: Some("Overview".intern()),
                air_date: Some("2024-01-01".intern()),
                cover: Some("http://example.test/s1.jpg".intern()),
                cover_tmdb: None,
                cover_big: None,
                duration: None,
            }]),
            episodes: Some(vec![SeriesStreamDetailEpisodeProperties {
                id: 501,
                episode_num: 1,
                season: 1,
                title: "Episode 1".intern(),
                container_extension: "mp4".intern(),
                custom_sid: None,
                added: "1700000000".intern(),
                direct_source: "".intern(),
                tmdb: Some(999),
                release_date: "2024-01-01".intern(),
                series_release_date: None,
                plot: Some("Plot".intern()),
                crew: None,
                duration_secs: 3600,
                duration: "60".intern(),
                movie_image: "http://example.test/e1.jpg".intern(),
                bitrate: 4500,
                rating: Some(8.0),
                video: None,
                audio: None,
            }]),
        };
        let series_props = SeriesStreamProperties {
            name: "Test Series".intern(),
            category_id: 1,
            series_id: id,
            cast: "Star".intern(),
            genre: Some("Sci-Fi".intern()),
            details: Some(details),
            ..Default::default()
        };
        XtreamPlaylistItem {
            virtual_id: VirtualId::new(id),
            provider_id: id,
            name: "Test Series".intern(),
            logo: "http://example.test/logo.png".intern(),
            logo_small: "".intern(),
            group: "Sci-Fi".intern(),
            title: "Test Series".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: format!("http://example.test/series/{id}").intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
            item_type: PlaylistItemType::SeriesInfo,
            category_id: 1,
            input_name: "test-input".intern(),
            channel_no: 10,
            source_ordinal: 0,
            input_stream_id: id.to_string().intern(),
            upstream_user_agent: None,
        }
    }

    #[tokio::test]
    async fn series_iterator_skips_episode_details() {
        use shared::model::StreamProperties;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let app_config = app_config_in(temp.path());
        let target = target_with_output(
            "series-target",
            TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        );
        let storage_path = xtream_get_storage_path(&app_config.config.load(), &target.name).expect("storage path");
        let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Series);
        std::fs::create_dir_all(xtream_path.parent().expect("parent dir")).expect("storage dir should be created");

        let mut tree = BPlusTree::new();
        tree.insert(77u32, sample_series_item_with_details(77));
        tree.store(&xtream_path).expect("series playlist should persist");

        let mut iterator = XtreamPlaylistIterator::new(
            XtreamCluster::Series,
            &app_config,
            &PlaylistStorageState::default(),
            &target,
            None,
            &ProxyUserCredentials::default(),
        )
        .await
        .expect("iterator should open");

        let entry = iterator.next().await.expect("entry should be yielded").expect("entry is ok");
        let (item, has_next) = entry;
        assert!(!has_next);

        // Header fields intact
        assert_eq!(item.virtual_id, VirtualId::new(77));
        assert_eq!(item.provider_id, 77);
        assert_eq!(item.name.as_ref(), "Test Series");
        assert_eq!(item.category_id, 1);
        assert_eq!(item.item_type, PlaylistItemType::SeriesInfo);

        // Details skipped
        match item.additional_properties {
            Some(StreamProperties::Series(series)) => {
                assert_eq!(series.name.as_ref(), "Test Series");
                assert_eq!(series.series_id, 77);
                assert_eq!(series.genre.as_deref(), Some("Sci-Fi"));
                assert_eq!(series.details, None, "details must be None on the list iterator");
            }
            other => panic!("expected Series properties, got {other:?}"),
        }

        assert!(iterator.next().await.is_none());
    }

    #[tokio::test]
    async fn memory_path_yields_same_items_and_order_as_disk_path_with_sorted_index() {
        use crate::{load_xtream_target_storage, PlaylistStorage};

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let app_config = app_config_in(temp.path());
        let mut target = target_with_output(
            "sorted-test-target",
            TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        );
        let storage_path = xtream_get_storage_path(&app_config.config.load(), &target.name).expect("storage path");
        let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
        std::fs::create_dir_all(xtream_path.parent().expect("parent dir")).expect("storage dir should be created");

        let mut item1 = xtream_playlist_item(1);
        item1.source_ordinal = 300;
        let mut item2 = xtream_playlist_item(2);
        item2.source_ordinal = 100;
        let mut item3 = xtream_playlist_item(3);
        item3.source_ordinal = 200;

        let mut tree = BPlusTree::new();
        tree.insert(1u32, item1);
        tree.insert(2u32, item2);
        tree.insert(3u32, item3);
        tree.store_with_index(&xtream_path, |pli| pli.source_ordinal).expect("store_with_index succeeds");

        let storage = load_xtream_target_storage(&app_config, &target).await.expect("load storage");
        assert_eq!(storage.live_order, vec![(100, 2), (200, 3), (300, 1)]);

        let playlists = PlaylistStorageState::new();
        playlists.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;

        // 1. Read via disk path (use_memory_cache: false)
        target.use_memory_cache = false;
        let disk_iter = XtreamPlaylistIterator::new(
            XtreamCluster::Live,
            &app_config,
            &playlists,
            &target,
            None,
            &ProxyUserCredentials::default(),
        )
        .await
        .expect("disk iterator opens");
        let disk_items: Vec<(XtreamPlaylistItem, bool)> =
            disk_iter.collect::<Vec<_>>().await.into_iter().map(Result::unwrap).collect();

        // 2. Read via memory path (use_memory_cache: true)
        target.use_memory_cache = true;
        let mem_iter = XtreamPlaylistIterator::new(
            XtreamCluster::Live,
            &app_config,
            &playlists,
            &target,
            None,
            &ProxyUserCredentials::default(),
        )
        .await
        .expect("memory iterator opens");
        let mem_items: Vec<(XtreamPlaylistItem, bool)> =
            mem_iter.collect::<Vec<_>>().await.into_iter().map(Result::unwrap).collect();

        assert_eq!(disk_items.len(), 3);
        assert_eq!(mem_items.len(), 3);
        assert_eq!(
            disk_items
                .iter()
                .map(|(item, has_next)| (item.virtual_id.get(), item.name.as_ref(), *has_next))
                .collect::<Vec<_>>(),
            mem_items
                .iter()
                .map(|(item, has_next)| (item.virtual_id.get(), item.name.as_ref(), *has_next))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            mem_items.iter().map(|(item, has_next)| (item.virtual_id.get(), *has_next)).collect::<Vec<_>>(),
            vec![(2, true), (3, true), (1, false)]
        );
    }
}
