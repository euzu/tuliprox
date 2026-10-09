use super::{
    behavior::{json_reader, make_input_group, read_live_props, wait_for_child},
    count_input_xtream_cluster, fixed_refresh_paths, get_collection_path, load_input_xtream_playlist, make_live_item,
    persist_input_xtream_playlist, persist_input_xtream_playlist_cluster_to_disk, publish_staged_file_same_directory,
    target_category_lock_path, target_writer_config, target_writer_group, test_app_config, write_single_item,
    xtream_cluster_category_collection, xtream_get_playlist_categories, xtream_write_playlist,
    xtream_write_playlist_with_mode, TargetEmptyPublicationHook, TargetEmptyReplacementMode,
};
use crate::{
    bplustree::{ensure_distinct_sidecar_lock_domains, sidecar_lock_path},
    build_input_storage_path, get_file_path_for_db_index, get_input_storage_path,
};
use shared::{
    model::{ClusterFlags, InputType, XtreamCluster},
    utils::Internable,
};
use std::{
    env, fs, io,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tempfile::tempdir;
use tuliprox_core::model::ConfigInput;

#[tokio::test]
async fn target_force_empty_category_reader_waits_through_the_backup_window() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let app_config = test_app_config(directory.path());
    let target = target_writer_config();
    let mut baseline = vec![
        target_writer_group(XtreamCluster::Live, 1, 101),
        target_writer_group(XtreamCluster::Video, 2, 201),
        target_writer_group(XtreamCluster::Series, 3, 301),
    ];
    xtream_write_playlist(&app_config, &target, &mut baseline, ClusterFlags::empty()).await?;
    let storage_path = {
        let config = app_config.config.load();
        super::super::xtream_get_storage_path(&config, &target.name).expect("target Xtream storage")
    };
    let category_path = super::super::get_vod_cat_collection_path(&storage_path);
    let hook = TargetEmptyPublicationHook::new();
    let entered = Arc::clone(&hook.backup_window_entered);
    let resume = Arc::clone(&hook.resume_publication);
    let writer_app_config = Arc::clone(&app_config);
    let writer_target = target.clone();
    let writer = tokio::spawn(async move {
        let mut candidate =
            vec![target_writer_group(XtreamCluster::Live, 1, 102), target_writer_group(XtreamCluster::Series, 3, 302)];
        xtream_write_playlist_with_mode(
            &writer_app_config,
            &writer_target,
            &mut candidate,
            ClusterFlags::Vod,
            TargetEmptyReplacementMode::PauseDuringPublication(hook),
        )
        .await
    });

    tokio::task::spawn_blocking(move || entered.wait()).await?;
    let category_was_temporarily_backed_up = !category_path.exists();
    let category_lock_path = target_category_lock_path(&category_path);
    let category_write_lock_was_held = app_config.file_locks.try_write_lock(&category_lock_path).await.is_err();

    let category_read = xtream_get_playlist_categories(&app_config, &target.name, XtreamCluster::Video);
    tokio::pin!(category_read);
    let category_read_poll = futures::poll!(category_read.as_mut());
    let category_reader_state = match &category_read_poll {
        std::task::Poll::Pending => "waiting",
        std::task::Poll::Ready(None) => "missing",
        std::task::Poll::Ready(Some(categories)) if categories.is_empty() => "new",
        std::task::Poll::Ready(Some(_)) => "old-or-partial",
    };

    tokio::task::spawn_blocking(move || resume.wait()).await?;
    writer.await??;
    assert!(category_was_temporarily_backed_up, "failure hook must expose the internal backup window");
    assert!(category_write_lock_was_held, "writer must hold the target category lock during publication");
    assert_eq!(category_reader_state, "waiting", "the category reader must wait on the writer's category lock");
    let categories = category_read.await.expect("published VOD category catalog");
    assert!(categories.is_empty(), "reader must observe the complete force-empty category catalog");
    Ok(())
}

#[tokio::test]
async fn input_cluster_count_returns_none_for_missing_baseline() {
    let directory = tempdir().expect("temp directory");
    let app_config = test_app_config(directory.path());
    let input = ConfigInput { name: "provider-a".intern(), input_type: InputType::Xtream, ..ConfigInput::default() };

    assert_eq!(
        count_input_xtream_cluster(&app_config, &input, XtreamCluster::Live)
            .await
            .expect("missing baseline should be readable"),
        None
    );
}

#[tokio::test]
async fn input_cluster_count_reads_the_canonical_active_raw_cluster() {
    let directory = tempdir().expect("temp directory");
    let app_config = test_app_config(directory.path());
    let input = ConfigInput { name: "provider-a".intern(), input_type: InputType::Xtream, ..ConfigInput::default() };

    let storage_path = get_input_storage_path(&input.name, directory.path().to_string_lossy().as_ref())
        .await
        .expect("canonical input storage");
    write_single_item(
        &super::super::xtream_get_file_path(&storage_path, XtreamCluster::Live),
        &make_live_item(700, None, None, None, None, 0),
    );

    assert_eq!(
        count_input_xtream_cluster(&app_config, &input, XtreamCluster::Live)
            .await
            .expect("active baseline should be readable"),
        Some(1)
    );
    assert_eq!(
        count_input_xtream_cluster(&app_config, &input, XtreamCluster::Video)
            .await
            .expect("other cluster should remain absent"),
        None
    );
}

#[tokio::test]
async fn in_memory_fallback_keeps_colliding_category_ids_separate_by_cluster() {
    let directory = tempdir().expect("temp directory");
    let app_config = test_app_config(directory.path());
    let storage_path = get_input_storage_path("provider-a", directory.path().to_string_lossy().as_ref())
        .await
        .expect("canonical input storage");

    let (_, seed_error) = persist_input_xtream_playlist(
        &app_config,
        &storage_path,
        vec![
            make_input_group(XtreamCluster::Live, 1, "Previous Live", 100),
            make_input_group(XtreamCluster::Video, 2, "Previous VOD", 200),
            make_input_group(XtreamCluster::Series, 3, "Previous Series", 300),
        ],
    )
    .await;
    assert!(seed_error.is_none(), "failed to seed persisted VOD: {seed_error:?}");
    let previous_vod = load_input_xtream_playlist(&app_config, &storage_path, &[XtreamCluster::Video])
        .await
        .expect("seeded VOD should load");
    let category_path = get_collection_path(&storage_path, xtream_cluster_category_collection(XtreamCluster::Video));
    let previous_categories = fs::read(&category_path).expect("persisted VOD categories");

    let (merged, persist_error) = persist_input_xtream_playlist(
        &app_config,
        &storage_path,
        vec![
            make_input_group(XtreamCluster::Live, 1, "New Live", 101),
            make_input_group(XtreamCluster::Series, 2, "New Series", 301),
        ],
    )
    .await;

    assert!(persist_error.is_none(), "failed to persist accepted clusters: {persist_error:?}");
    assert_eq!(fs::read(&category_path).expect("retained VOD categories"), previous_categories);
    assert!(merged
        .iter()
        .all(|group| { group.channels.iter().all(|item| item.header.xtream_cluster == group.xtream_cluster) }));

    let retained_vod = merged
        .iter()
        .find(|group| group.xtream_cluster == XtreamCluster::Video && group.id == 2)
        .expect("persisted VOD fallback");
    assert_eq!(retained_vod.title.as_ref(), "Previous VOD");
    assert_eq!(retained_vod.channels.len(), 1);
    assert_eq!(retained_vod.channels[0].header.id.as_ref(), "200");

    let accepted_live = merged
        .iter()
        .find(|group| group.xtream_cluster == XtreamCluster::Live && group.id == 1)
        .expect("accepted Live candidate");
    assert_eq!(accepted_live.title.as_ref(), "New Live");
    assert_eq!(accepted_live.channels[0].header.id.as_ref(), "101");

    let accepted_series = merged
        .iter()
        .find(|group| group.xtream_cluster == XtreamCluster::Series && group.id == 2)
        .expect("accepted Series candidate sharing VOD category id");
    assert_eq!(accepted_series.title.as_ref(), "New Series");
    assert_eq!(accepted_series.channels[0].header.id.as_ref(), "301");

    let loaded = load_input_xtream_playlist(&app_config, &storage_path, &[XtreamCluster::Video])
        .await
        .expect("retained VOD should load");
    assert_eq!(loaded.len(), previous_vod.len());
    assert_eq!(loaded[0].id, previous_vod[0].id);
    assert_eq!(loaded[0].title, previous_vod[0].title);
    assert_eq!(loaded[0].xtream_cluster, previous_vod[0].xtream_cluster);
    assert_eq!(loaded[0].channels.len(), previous_vod[0].channels.len());
    assert_eq!(loaded[0].channels[0].header.id, previous_vod[0].channels[0].header.id);
    assert_eq!(loaded[0].channels[0].header.name, previous_vod[0].channels[0].header.name);
    assert_eq!(loaded[0].channels[0].header.category_id, previous_vod[0].channels[0].header.category_id);
    assert_eq!(loaded[0].channels[0].header.xtream_cluster, previous_vod[0].channels[0].header.xtream_cluster);
}

#[test]
fn refresh_staging_database_uses_distinct_published_lock_domain() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 1);

    assert_eq!(paths.published_database.file_name().and_then(|name| name.to_str()), Some("live.db"));
    assert_ne!(sidecar_lock_path(&paths.published_database), sidecar_lock_path(&paths.staging_database));
}

#[test]
fn refresh_staging_database_and_index_share_one_generation_lock_domain() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 2);
    let staging_index = get_file_path_for_db_index(&paths.staging_database);

    assert_eq!(sidecar_lock_path(&paths.staging_database), sidecar_lock_path(&staging_index));
}

#[test]
fn colliding_staging_path_is_rejected_before_lock_acquisition() {
    let dir = tempdir().expect("temp dir should be created");
    let published = dir.path().join("live.db");
    let colliding = dir.path().join("live.tmp");

    let error = ensure_distinct_sidecar_lock_domains(&published, &colliding)
        .expect_err("colliding sidecar domains must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn sequential_refresh_generations_use_distinct_stems() {
    let dir = tempdir().expect("temp dir should be created");
    let first = fixed_refresh_paths(dir.path(), 10);
    let second = fixed_refresh_paths(dir.path(), 11);

    assert_ne!(first.staging_database.file_stem(), second.staging_database.file_stem());
    assert_ne!(sidecar_lock_path(&first.staging_database), sidecar_lock_path(&second.staging_database));
}

#[test]
fn category_publish_atomically_replaces_same_directory_file() {
    let dir = tempdir().expect("temp dir should be created");
    let published = dir.path().join("cat_live.json");
    let staging = dir.path().join("cat_live.refresh-fixed.json");
    fs::write(&published, b"old").expect("published fixture should be written");
    fs::write(&staging, b"new").expect("staging fixture should be written");

    publish_staged_file_same_directory(&staging, &published).expect("category publish should succeed");

    assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
    assert!(!staging.exists());
}

#[test]
fn category_publish_creates_missing_same_directory_file() {
    let dir = tempdir().expect("temp dir should be created");
    let published = dir.path().join("cat_live.json");
    let staging = dir.path().join("cat_live.refresh-fixed.json");
    fs::write(&staging, b"new").expect("staging fixture should be written");

    publish_staged_file_same_directory(&staging, &published).expect("category publish should succeed");

    assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
    assert!(!staging.exists());
}

#[test]
fn category_publish_rejects_different_parent_before_replace() {
    let dir = tempdir().expect("temp dir should be created");
    let staging_dir = dir.path().join("staging");
    let published_dir = dir.path().join("published");
    fs::create_dir_all(&staging_dir).expect("staging directory should be created");
    fs::create_dir_all(&published_dir).expect("published directory should be created");
    let staging = staging_dir.join("cat_live.refresh-fixed.json");
    let published = published_dir.join("cat_live.json");
    fs::write(&staging, b"new").expect("staging fixture should be written");
    fs::write(&published, b"old").expect("published fixture should be written");

    let error = publish_staged_file_same_directory(&staging, &published)
        .expect_err("cross-directory category publication should fail");

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(&staging).expect("staging fixture should remain"), b"new");
    assert_eq!(fs::read(&published).expect("published fixture should remain"), b"old");
}

#[cfg(not(windows))]
#[test]
fn category_publish_reports_post_rename_barrier_failure_truthfully() {
    let dir = tempdir().expect("temp dir should be created");
    let published = dir.path().join("cat_live.json");
    let staging = dir.path().join("cat_live.refresh-fixed.json");
    fs::write(&published, b"old").expect("published fixture should be written");
    fs::write(&staging, b"new").expect("staging fixture should be written");
    let staging_path =
        tempfile::TempPath::try_from_path(&staging).expect("staging fixture should become an owned temporary path");

    let error = super::super::publish_staged_file_with_parent_sync(staging_path, &published, |_| {
        Err(io::Error::other("injected parent synchronization failure"))
    })
    .expect_err("post-rename synchronization failure should be reported");

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("was published, but its parent directory"));
    assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
    assert!(!staging.exists());
}

#[test]
fn xtream_refresh_end_to_end_child() -> io::Result<()> {
    let Some(storage_root) = env::var_os("TULIPROX_XTREAM_REFRESH_TEST_ROOT") else {
        return Ok(());
    };
    let storage_root = Path::new(&storage_root);
    let app_config = test_app_config(storage_root);
    let input = ConfigInput { name: "deadlock-test".intern(), input_type: InputType::Xtream, ..ConfigInput::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        for _ in 0..2 {
            persist_input_xtream_playlist_cluster_to_disk(
                &app_config,
                &input,
                XtreamCluster::Live,
                0,
                json_reader(r#"[{"category_id":"1","category_name":"Sports"}]"#),
                json_reader(r#"[{"name":"Live","stream_id":700,"category_id":"1","added":"0"}]"#),
            )
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        }
        Ok::<(), io::Error>(())
    })?;

    let input_storage = build_input_storage_path(&input.name, storage_root.to_string_lossy().as_ref());
    let published = super::super::xtream_get_file_path(&input_storage, XtreamCluster::Live);
    let learned = read_live_props(&published, 700);
    assert_eq!(learned.video, Some("{\"codec_name\":\"h264\"}".intern()));
    assert_eq!(learned.bitrate, 2_500_000);
    Ok(())
}

#[test]
fn two_sequential_cluster_refreshes_complete_without_generation_artifacts() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let input_name = "deadlock-test".intern();
    let input_storage = build_input_storage_path(&input_name, directory.path().to_string_lossy().as_ref());
    fs::create_dir_all(&input_storage)?;
    let published = super::super::xtream_get_file_path(&input_storage, XtreamCluster::Live);
    write_single_item(
        &published,
        &make_live_item(
            700,
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some(1_700_000_000),
            Some(1_700_000_100),
            2_500_000,
        ),
    );

    let child = Command::new(env::current_exe()?)
        .arg("--exact")
        .arg("xtream_repository::tests::xtream_refresh_end_to_end_child")
        .arg("--nocapture")
        .env("TULIPROX_XTREAM_REFRESH_TEST_ROOT", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let status = wait_for_child(child, Duration::from_secs(20))?;
    assert!(status.success(), "Xtream refresh child failed with {status}");

    let learned = read_live_props(&published, 700);
    assert_eq!(learned.video, Some("{\"codec_name\":\"h264\"}".intern()));
    assert_eq!(learned.bitrate, 2_500_000);
    let entries = fs::read_dir(&input_storage)?.collect::<io::Result<Vec<_>>>()?;
    let generation_artifacts = entries
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy().contains(".refresh-"))
        .collect::<Vec<_>>();
    assert!(generation_artifacts.is_empty(), "generation artifacts survived successful refreshes");
    assert!(sidecar_lock_path(&published).exists());
    Ok(())
}
