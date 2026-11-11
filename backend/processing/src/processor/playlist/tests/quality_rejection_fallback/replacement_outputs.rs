use super::*;
use shared::model::{EpgChannel, EpgProgramme, PluginOutputRequest, StrmTargetOutputDto, TargetType};
use std::{collections::BTreeMap, fs};
use tuliprox_core::model::{Epg, HdHomeRunTargetOutput, M3uTargetOutput, PluginTargetOutput, StrmTargetOutput};
use tuliprox_repository::{
    epg_query_channels_by_storage_key, get_target_storage_path, lineup_guide_candidates, load_m3u_target_storage,
    m3u_get_epg_file_path_for_target, m3u_lineup_guide_index_path, xtream_get_epg_file_path_for_target,
    LineupGuideLookup,
};

#[derive(Clone)]
struct FileRenderer {
    fail: bool,
}

impl PluginHost for FileRenderer {
    async fn render_output(
        &self,
        _plugin: &str,
        _ctx: &HookCtx<'_>,
        request: &PluginOutputRequest<'_>,
        items: &[&PlaylistItem],
    ) -> Result<(), String> {
        let names: Vec<_> = items.iter().map(|item| item.header.name.as_ref()).collect();
        let output = serde_json::to_vec(&names).unwrap();
        tokio::fs::write(request.path, output).await.map_err(|err| err.to_string())?;
        if self.fail {
            return Err("fixture renderer failed after writing".into());
        }
        Ok(())
    }
}

fn mixed_target() -> Arc<ConfigTarget> {
    let mut target = xtream_target(true);
    Arc::make_mut(&mut target).output.extend([
        TargetOutput::M3u(M3uTargetOutput {
            filename: Some("mixed.m3u".to_owned()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        }),
        TargetOutput::Strm(StrmTargetOutput::from(&StrmTargetOutputDto {
            directory: "strm".into(),
            cleanup: true,
            flat: true,
            ..Default::default()
        })),
        TargetOutput::HdHomeRun(HdHomeRunTargetOutput {
            device: "fixture".into(),
            username: "fixture".into(),
            use_output: Some(TargetType::M3u),
        }),
        TargetOutput::Plugin(PluginTargetOutput {
            plugin: "fixture".into(),
            format: Some("json".into()),
            filename: Some("streams.json".into()),
            plugin_config: serde_json::Map::default(),
            filter: None,
        }),
    ]);
    target
}

fn groups(name: &str, id: u32) -> Vec<PlaylistGroup> {
    let mut groups = vec![
        baseline_group(XtreamCluster::Live, 1, &format!("{name}-live"), id),
        baseline_group(XtreamCluster::Video, 2, &format!("{name}-vod"), id + 100),
        baseline_group(XtreamCluster::Series, 3, &format!("{name}-series"), id + 200),
    ];
    groups[0].channels[0].header.epg_channel_id = Some(format!("{name}-guide").intern());
    groups
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

async fn check_mixed(
    ctx: &PlaylistProcessingContext<CollectSink>,
    cache: &PlaylistStorageState,
    target: &ConfigTarget,
    root: &Path,
    generation: Option<&str>,
) {
    let count = usize::from(generation.is_some());
    assert_target_counts(&ctx.config, target, [2 * count; 3]).await;
    let m3u = load_m3u_target_storage(&ctx.config, target).await.unwrap();
    let names: Vec<_> = m3u.iter().map(|(_, item)| item.name.to_string()).collect();
    assert_eq!(names.len(), 4 * count);
    let cache = cache.data.read().await;
    let stored = &cache[&target.name];
    assert_eq!(stored.xtream.as_ref().unwrap().vod.len(), 2 * count);
    assert!(stored
        .m3u
        .as_ref()
        .unwrap()
        .iter()
        .map(|(_, item)| item.name.as_ref())
        .eq(names.iter().map(String::as_str)));
    drop(cache);

    let config = ctx.config.config.load();
    let target_path = get_target_storage_path(&config, &target.name).unwrap();
    let mut plugin: Vec<String> = serde_json::from_slice(&fs::read(target_path.join("streams.json")).unwrap()).unwrap();
    let mut expected_plugin = names.clone();
    plugin.sort_unstable();
    expected_plugin.sort_unstable();
    assert_eq!(plugin, expected_plugin);
    let text = fs::read_to_string(root.join("mixed.m3u")).unwrap();
    let strm = files(&root.join("strm"));
    let managed: Vec<_> = strm.keys().filter(|path| path.extension().is_some_and(|ext| ext == "strm")).collect();
    assert_eq!(managed.len(), 4 * count);
    assert_eq!(strm[&root.join("strm").join("foreign.txt")], b"foreign");
    let keys = vec!["old-live-1".intern(), "new-live-1".intern()];
    let guide = lineup_guide_candidates(&ctx.config.file_locks, m3u_lineup_guide_index_path(&target_path), keys).await;
    if let Some(name) = generation {
        assert!(names.iter().all(|item| item.starts_with(name)));
        assert!(names.iter().all(|item| text.contains(item)));
        assert!(managed.iter().all(|path| path.file_name().unwrap().to_string_lossy().starts_with(name)));
        let LineupGuideLookup::Candidates(candidates) = guide else {
            panic!("expected a current HDHomeRun guide index")
        };
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].key.starts_with(name));
    } else {
        assert_eq!(text, "#EXTM3U\n");
        assert!(matches!(guide, LineupGuideLookup::NoEntry));
    }
    eprintln!(
        "AP12 outputs generation={generation:?} Xtream={:?} M3U={names:?} STRM={} plugin={plugin:?}",
        [2 * count; 3],
        managed.len()
    );
}

#[tokio::test]
async fn ap12_replacement_publishes_mixed_outputs_and_retries_after_plugin_failure() {
    let root = tempfile::tempdir().unwrap();
    let server = TestXtreamServer::start([2, 2, 2]);
    let ctx = processing_context_with_events(root.path(), CollectSink::default());
    let input = test_input(&server.base_url, ConfigInputUpdateQualityDto::default(), 3600, &[]);
    let cache = Arc::new(PlaylistStorageState::new());
    let mut target = mixed_target();
    let other = Arc::new(ConfigTarget { id: 2, name: "unselected".into(), ..xtream_target(true).as_ref().clone() });
    install_source(&ctx, &input, &target);
    seed_xtream_target(&ctx, &other, &cache).await;
    let other_storage = xtream_get_storage_path(&ctx.config.config.load(), &other.name).unwrap();
    let other_files = files(&other_storage);
    let permit = UpdateGuard::new();
    let run = |target: &Arc<ConfigTarget>, id: &str, fail| {
        super::target_staging::replacement_run(&ctx, &cache, &permit, input.id, id, &[target])
            .with_plugins(FileRenderer { fail })
    };
    let install_generation = |name, id| {
        let (config, input) = (ctx.config.clone(), input.clone());
        let playlist = groups(name, id);
        async move {
            let (_, error) = tuliprox_repository::persist_input_playlist(&config, &input, playlist).await;
            assert!(error.is_none(), "{error:?}");
        }
    };
    install_generation("old", 100).await;
    seed_valid_cluster_cache(&ctx, &input).await;
    exec_processing(run(&target, "seed-mixed", false)).await;
    fs::write(root.path().join("strm/foreign.txt"), "foreign").unwrap();
    check_mixed(&ctx, &cache, &target, root.path(), Some("old")).await;
    seed_old_epg(&ctx, &target).await;
    let before = files(root.path());

    install_generation("new", 1000).await;
    seed_valid_cluster_cache(&ctx, &input).await;
    // The input cache is intentionally refreshed above. Compare only published target outputs.
    let published = |all: BTreeMap<PathBuf, Vec<u8>>| {
        let target_dir = root.path().join(&target.name);
        all.into_iter()
            .filter(|(path, _)| {
                // Run analytics intentionally records the failed attempt.
                (path.starts_with(&target_dir) && !path.ends_with("analytics.json"))
                    || path.starts_with(root.path().join("strm"))
                    || path.ends_with("mixed.m3u")
            })
            .collect::<BTreeMap<_, _>>()
    };
    let before = published(before);
    exec_processing(run(&target, "mixed-render-failure", true)).await;
    assert!(permit.try_playlist().is_some());
    let after = published(files(root.path()));
    assert!(after.keys().eq(before.keys()), "published file list changed");
    for (path, previous) in &before {
        assert!(after[path] == *previous, "published file changed: {}", path.display());
    }
    check_mixed(&ctx, &cache, &target, root.path(), Some("old")).await;
    let events = std::mem::take(&mut *ctx.events.0.lock().unwrap());
    assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdateProgress(progress)
        if progress.failure_reason.as_deref().is_some_and(|reason| reason.contains("quality-target")
            && reason.contains("fixture renderer failed after writing")))));
    assert!(events.iter().any(|event| matches!(event, EventMessage::PlaylistUpdate(summary)
        if summary.run_id.as_ref().is_some_and(|id| id.as_ref() == "mixed-render-failure")
            && summary.state == PlaylistUpdateState::Failure)));

    exec_processing(run(&target, "mixed-retry", false)).await;
    check_mixed(&ctx, &cache, &target, root.path(), Some("new")).await;
    let config = ctx.config.config.load_full();
    let target_dir = get_target_storage_path(&config, &target.name).unwrap();
    let xtream = xtream_get_storage_path(&config, &target.name).unwrap();
    assert!(!m3u_get_epg_file_path_for_target(&target_dir).exists());
    assert!(!xtream_get_epg_file_path_for_target(&xtream).exists());
    assert_eq!(files(&other_storage), other_files);
    Arc::make_mut(&mut target).filter = get_filter(r#"Name = "nothing matches""#, None).unwrap().into();
    install_source(&ctx, &input, &target);
    exec_processing(run(&target, "mixed-empty", false)).await;
    check_mixed(&ctx, &cache, &target, root.path(), None).await;
    assert_empty_target_categories(&ctx.config, &target, &XTREAM_CLUSTER).await;
    assert_eq!(files(&other_storage), other_files);
    assert_eq!(server.finish(), Vec::<String>::new());
}

async fn seed_old_epg(ctx: &PlaylistProcessingContext<CollectSink>, target: &ConfigTarget) {
    let id = "old-guide".intern();
    let epg = Epg {
        children: vec![Arc::new(EpgChannel {
            id: id.clone(),
            title: None,
            icon: None,
            programmes: vec![EpgProgramme::new(10, 20, id.clone())],
        })],
        ..Default::default()
    };
    let config = ctx.config.config.load();
    let target_dir = get_target_storage_path(&config, &target.name).unwrap();
    let xtream = xtream_get_storage_path(&config, &target.name).unwrap();
    let playlist = groups("old", 100);
    for output in target.output.iter().filter(|output| output.target_type().supports_epg()) {
        tuliprox_repository::epg_write_for_target(
            &config,
            &ctx.config.file_locks,
            target,
            &target_dir,
            Some(&epg),
            output,
            Some(&playlist),
        )
        .await
        .unwrap();
    }
    for path in [m3u_get_epg_file_path_for_target(&target_dir), xtream_get_epg_file_path_for_target(&xtream)] {
        let guide = epg_query_channels_by_storage_key(&ctx.config.file_locks, &path, vec![id.clone()]).await.unwrap();
        assert_eq!(guide[0].1.as_ref().unwrap().programmes.len(), 1);
    }
}
