use super::*;
use futures::TryStreamExt;
use shared::model::XtreamPlaylistItem;
use std::{collections::BTreeMap, fs};
use tuliprox_core::model::ProxyUserCredentials;
use tuliprox_repository::{
    publication_fixture::{pause_target_staging, TargetStagingPause},
    wal_path, wal_temporary_path, xtream_get_file_path, xtream_get_playlist_categories, xtream_load_rewrite_playlist,
};

#[derive(Debug, PartialEq)]
struct PublishedView {
    items: [Vec<(u32, Arc<str>)>; 3],
    categories: [Vec<String>; 3],
}

async fn read_published(
    ctx: &PlaylistProcessingContext<CollectSink>,
    cache: &PlaylistStorageState,
    target: &ConfigTarget,
    counts: [usize; 3],
) -> PublishedView {
    let disk = load_xtream_target_storage(&ctx.config, target).await.unwrap();
    let mut user = ProxyUserCredentials::default();
    user.t_is_api_user = true;
    let mut view = PublishedView { items: Default::default(), categories: Default::default() };
    for (index, cluster) in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series].into_iter().enumerate() {
        let (tree, disk_order) = disk.get_cluster(cluster);
        let signature = |tree: &tuliprox_repository::BPlusTree<u32, XtreamPlaylistItem>| {
            tree.iter().map(|(id, item)| (*id, item.name.clone())).collect::<Vec<_>>()
        };
        let disk_items = signature(tree);
        {
            let cache = cache.data.read().await;
            let (tree, order) = cache[&target.name].xtream.as_ref().unwrap().get_cluster(cluster);
            assert_eq!(signature(tree), disk_items, "{cluster} memory cache");
            assert_eq!(order, disk_order, "{cluster} memory cache order");
        }
        let documents: Vec<_> = xtream_load_rewrite_playlist(cluster, &ctx.config, cache, target, None, &user)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        let mut names = documents
            .iter()
            .map(|(json, _)| {
                serde_json::from_str::<serde_json::Value>(json).unwrap()["name"].as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>();
        names.sort_unstable();
        let mut disk_names = disk_items.iter().map(|(_, name)| name.as_ref()).collect::<Vec<_>>();
        disk_names.sort_unstable();
        assert_eq!(names, disk_names, "{cluster} API entries");
        assert_eq!(names.len(), counts[index], "{cluster} API entry count");
        let categories = xtream_get_playlist_categories(&ctx.config, &target.name, cluster).await.unwrap();
        assert_eq!(categories.len(), usize::from(counts[index] > 0), "{cluster} API category count");
        assert!(tree.iter().all(|(_, item)| categories
            .iter()
            .any(|category| { category.id == item.category_id && category.name == item.group.as_ref() })));
        view.categories[index] = categories.into_iter().map(|category| category.name).collect();
        view.items[index] = disk_items;
    }
    view
}

fn target_files(storage: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(storage)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name().into_string().unwrap(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn check_run(
    ctx: &PlaylistProcessingContext<CollectSink>,
    run_id: &str,
    expected_state: PlaylistUpdateState,
    published: &[&str],
) {
    let events = ctx.events.0.lock().unwrap();
    let summary = events
        .iter()
        .find_map(|event| match event {
            EventMessage::PlaylistUpdate(summary)
                if summary.run_id.as_ref().is_some_and(|id| id.as_ref() == run_id) =>
            {
                Some(summary)
            }
            _ => None,
        })
        .expect("terminal summary");
    assert_eq!(summary.state, expected_state);
    assert!(summary.processed_targets().eq(published.iter().copied()));
    if expected_state != PlaylistUpdateState::Success {
        assert_eq!(summary.error.as_deref(), Some("Playlist processing timed out after 3600 secs"));
    }
}

enum TimeoutScenario {
    Cancel,
    Retry,
    PriorPublication,
}

#[tokio::test(start_paused = true)]
async fn ap11_s01_staging_writer_outlives_timed_out_run() {
    check_staging_timeout(TimeoutScenario::Cancel, "S01").await;
}

#[tokio::test(start_paused = true)]
async fn ap11_s03_retry_publishes_before_abandoned_writer_finishes() {
    check_staging_timeout(TimeoutScenario::Retry, "S03").await;
}

#[tokio::test(start_paused = true)]
async fn ap11_s04_prior_target_stays_published_after_staging_timeout() {
    check_staging_timeout(TimeoutScenario::PriorPublication, "S04").await;
}

async fn seed_staging_targets(
    ctx: &PlaylistProcessingContext<CollectSink>,
    input: &Arc<ConfigInput>,
    cache: &Arc<PlaylistStorageState>,
) -> (Arc<ConfigTarget>, Arc<ConfigTarget>) {
    let mut config = (*ctx.config.config.load_full()).clone();
    config.process_parallel = false;
    ctx.config.config.store(Arc::new(config));
    let mut first = xtream_target(true);
    Arc::make_mut(&mut first).filter = get_filter("Type = live", None).unwrap().into();
    let blocked = Arc::new(ConfigTarget { name: "waiting-target".to_owned(), id: 2, ..first.as_ref().clone() });
    seed_baseline(ctx, input).await;
    seed_valid_cluster_cache(ctx, input).await;
    for target in [&first, &blocked] {
        seed_xtream_target(ctx, target, cache).await;
    }
    install_source(ctx, input, &first);
    let mut sources = (*ctx.config.sources.load_full()).clone();
    sources.sources[0].targets.push(blocked.clone());
    ctx.config.sources.store(Arc::new(sources));
    (first, blocked)
}

fn replacement_run(
    ctx: &PlaylistProcessingContext<CollectSink>,
    cache: &Arc<PlaylistStorageState>,
    permit: &UpdateGuard,
    input_id: u16,
    run_id: &str,
    targets: &[&Arc<ConfigTarget>],
) -> ProcessingRun<CollectSink> {
    ProcessingRun::for_run(
        run_id.into(),
        ctx.client.clone(),
        ctx.config.clone(),
        Arc::new(ProcessTargets {
            enabled: true,
            inputs: vec![input_id],
            targets: targets.iter().map(|target| target.id).collect(),
            target_names: targets.iter().map(|target| target.name.clone()).collect(),
        }),
        ctx.events.clone(),
    )
    .with_expected_scope(Some(ctx.config.sources.load().playlist_update_scope()))
    .with_playlist_state(cache.clone())
    .with_replace_target_contents(true)
    .with_update_guard(permit.clone())
}

async fn check_staging_timeout(scenario: TimeoutScenario, case: &str) {
    let temp = tempfile::tempdir().unwrap();
    let server = TestXtreamServer::start([2, 2, 2]);
    let ctx = processing_context_with_events(temp.path(), CollectSink::default());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto::default(), 3600, &[]);
    let cache = Arc::new(PlaylistStorageState::new());
    let (first, blocked) = seed_staging_targets(&ctx, &input, &cache).await;
    let storage = xtream_get_storage_path(&ctx.config.config.load(), &blocked.name).unwrap();
    let first_storage = xtream_get_storage_path(&ctx.config.config.load(), &first.name).unwrap();
    let old = read_published(&ctx, &cache, &blocked, [2, 2, 2]).await;
    let first_old = read_published(&ctx, &cache, &first, [2, 2, 2]).await;
    let old_files = target_files(&storage);
    let first_old_files = target_files(&first_storage);
    let TargetStagingPause { entered, resume, written, mut cleaned } =
        pause_target_staging(&storage, XtreamCluster::Live, false);
    let permit = UpdateGuard::new();
    let run = |id, targets: &[&Arc<ConfigTarget>]| replacement_run(&ctx, &cache, &permit, input.id, id, targets);
    let prior_commit = matches!(scenario, TimeoutScenario::PriorPublication);
    let selected: &[_] = if prior_commit { &[&first, &blocked] } else { &[&blocked] };
    let mut worker = tokio::spawn(exec_processing(run("staging-timeout", selected)));
    let files = tokio::select! {
        files = entered => files.expect("real Xtream staging writer paused"),
        result = &mut worker => panic!("update finished before the Xtream writer paused: {result:?}"),
    };
    assert_eq!(files.existing(), std::slice::from_ref(&files.categories));
    assert!(permit.try_playlist().is_none(), "the live run must hold the update permit");
    let first_counts = if prior_commit { [2, 0, 0] } else { [2, 2, 2] };
    let (first_expected, first_files) = if prior_commit {
        (read_published(&ctx, &cache, &first, first_counts).await, target_files(&first_storage))
    } else {
        (first_old, first_old_files)
    };

    tokio::time::advance(Duration::from_secs(3601)).await;
    worker.await.unwrap();
    assert!(permit.try_playlist().is_some(), "the timed-out run must release its update permit");
    assert!(matches!(cleaned.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
    assert_eq!(files.existing(), std::slice::from_ref(&files.categories), "the writer is still paused");
    assert_eq!(read_published(&ctx, &cache, &blocked, [2, 2, 2]).await, old);
    if prior_commit {
        check_run(&ctx, "staging-timeout", PlaylistUpdateState::Partial, &[&first.name]);
    } else {
        check_run(&ctx, "staging-timeout", PlaylistUpdateState::Failure, &[]);
    }

    let mut expected = old;
    let mut expected_files = old_files;
    let mut counts = [2, 2, 2];
    let mut target = blocked;
    if matches!(scenario, TimeoutScenario::Retry) {
        Arc::make_mut(&mut target).filter = get_filter("Type = movie", None).unwrap().into();
        let mut sources = (*ctx.config.sources.load_full()).clone();
        sources.sources[0].targets[1] = target.clone();
        ctx.config.sources.store(Arc::new(sources));
        exec_processing(run("staging-retry", &[&target])).await;
        check_run(&ctx, "staging-retry", PlaylistUpdateState::Success, &[&target.name]);
        assert_eq!(
            files.existing(),
            std::slice::from_ref(&files.categories),
            "retry must leave the paused writer's files alone"
        );
        counts = [0, 2, 0];
        expected = read_published(&ctx, &cache, &target, counts).await;
        expected_files = target_files(&storage);
        expected_files.remove(files.categories.file_name().unwrap().to_str().unwrap()).unwrap();
    }
    // Mark the published database's WAL paths while only the old writer can run.
    // Remove the markers before opening the published database again.
    let published_wal = matches!(scenario, TimeoutScenario::Retry).then(|| {
        let database = xtream_get_file_path(&storage, XtreamCluster::Video);
        let paths = [wal_path(&database), wal_temporary_path(&database)];
        for path in &paths {
            assert!(!path.exists());
            fs::write(path, b"published WAL marker").unwrap();
        }
        paths
    });

    resume.send(()).unwrap();
    let written = written.await.expect("writer created its DB and index after timeout");
    assert!(written.contains(&files.database) && written.contains(&files.index) && written.contains(&files.categories));
    cleaned.await.unwrap();
    assert_eq!(files.existing(), Vec::<PathBuf>::new());
    if let Some(paths) = &published_wal {
        for path in paths {
            assert_eq!(fs::read(path).unwrap(), b"published WAL marker");
            fs::remove_file(path).unwrap();
        }
    }
    assert_eq!(target_files(&storage), expected_files);
    assert_eq!(target_files(&first_storage), first_files);
    let actual = read_published(&ctx, &cache, &target, counts).await;
    assert_eq!(actual, expected);
    assert_eq!(read_published(&ctx, &cache, &first, first_counts).await, first_expected);
    assert!(server.finish().is_empty(), "valid cache should not make upstream requests");
    let written: Vec<_> = written.iter().map(|path| path.file_name().unwrap()).collect();
    eprintln!("AP11 {case} permit=released-before-resume paused={:?} written={written:?} remaining=[] target={actual:?} other={first_expected:?} published_files={:?} published_wal_markers_verified={}", files.categories.file_name().unwrap(), expected_files.keys().collect::<Vec<_>>(), published_wal.is_some());
}
