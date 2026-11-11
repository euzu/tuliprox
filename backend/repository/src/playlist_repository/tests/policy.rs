use super::*;
use crate::playlist_repository::persist_playlist;
use shared::model::{ConfigProviderDto, ProviderUrlSelectionPolicy};
use std::fs;
use tuliprox_core::model::{ConfigInput, ConfigProvider, ConfigSource, SourcesConfig};

#[tokio::test]
async fn ap12_strm_replacement_preserves_exports_indexed_by_another_target() {
    check_shared_strm_replacement(false).await;
    check_shared_strm_replacement(true).await;
}

async fn check_shared_strm_replacement(case_alias: bool) {
    let directory = tempdir().unwrap();
    if case_alias {
        let probe = directory.path().join("Case");
        fs::write(&probe, "").unwrap();
        let aliases = directory.path().join("case").exists();
        fs::remove_file(probe).unwrap();
        if !aliases {
            return;
        }
    }
    let app = app_config_in(directory.path());
    let output =
        StrmTargetOutput::from(&StrmTargetOutputDto { directory: "strm".into(), flat: true, ..Default::default() });
    let selected = Arc::new(target_with_options("selected", vec![TargetOutput::Strm(output.clone())], false));
    let other = Arc::new(ConfigTarget { id: 2, name: "other".into(), ..selected.as_ref().clone() });
    app.sources.store(Arc::new(SourcesConfig {
        sources: vec![ConfigSource { inputs: vec![], targets: vec![selected.clone(), other.clone()] }],
        ..Default::default()
    }));

    let mut shared = target_group(XtreamCluster::Live, 1, "shared");
    shared.channels[0].header.url = "https://example.test/shared".intern();
    let mut exclusive = target_group(XtreamCluster::Live, 2, "exclusive");
    exclusive.channels[0].header.url = "https://example.test/exclusive".intern();
    let ordinary = TargetPlaylistPersistOptions::default();
    persist_playlist(&app, &mut [shared.clone(), exclusive], None, &selected, None, ordinary.clone()).await.unwrap();
    let mut other_group = shared.clone();
    if case_alias {
        other_group.channels[0].header.name = "Shared".intern();
        other_group.channels[0].header.title = "Shared".intern();
    }
    persist_playlist(&app, &mut [other_group], None, &other, None, ordinary).await.unwrap();
    let index_prefix = hash_string_as_hex("strm");
    let selected_index = strm_get_file_paths(&index_prefix, &directory.path().join("selected"));
    let other_index = strm_get_file_paths(&index_prefix, &directory.path().join("other"));
    let previous_index = fs::read(&other_index).unwrap();
    let shared_file = directory.path().join("strm/shared.strm");
    let previous = fs::read(&shared_file).unwrap();
    if case_alias {
        assert_eq!(
            fs::read_to_string(&selected_index).unwrap().lines().find(|name| *name == "shared.strm"),
            Some("shared.strm")
        );
        assert_eq!(std::str::from_utf8(&previous_index).unwrap().trim(), "Shared.strm");
    }

    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    shared.channels[0].header.url = "https://example.test/changed".intern();
    let errors = persist_playlist(&app, &mut [shared], None, &selected, None, replacement.clone()).await.unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("STRM file is also managed by another target")));
    assert!(directory.path().join("strm/exclusive.strm").is_file());
    assert_eq!(fs::read(&shared_file).unwrap(), previous);

    persist_playlist(&app, &mut [], None, &selected, None, replacement).await.unwrap();
    assert_eq!(fs::read_to_string(selected_index).unwrap(), "");
    assert_eq!(fs::read(other_index).unwrap(), previous_index);
    assert_eq!(fs::read(shared_file).unwrap(), previous);
    assert!(!directory.path().join("strm/exclusive.strm").exists());
}

#[tokio::test]
async fn ap12_strm_replacement_rolls_back_when_new_names_alias_on_disk() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let output =
        StrmTargetOutput::from(&StrmTargetOutputDto { directory: "strm".into(), flat: true, ..Default::default() });
    let target = target_with_options("selected", vec![TargetOutput::Strm(output)], false);
    let mut prior = target_group(XtreamCluster::Live, 1, "prior");
    prior.channels[0].header.url = "https://example.test/prior".intern();
    persist_playlist(&app, &mut [prior], None, &target, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let root = directory.path().join("strm");
    let case_alias = root.join("PRIOR.strm").is_file();
    let index = strm_get_file_paths(&hash_string_as_hex("strm"), &directory.path().join("selected"));
    let before = fs::read(&index).unwrap();

    let mut first = target_group(XtreamCluster::Live, 2, "New");
    first.channels[0].header.url = "https://example.test/first".intern();
    let mut second = target_group(XtreamCluster::Live, 3, "new");
    second.channels[0].header.url = "https://example.test/second".intern();
    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let result = persist_playlist(&app, &mut [first, second], None, &target, None, replacement).await;
    if case_alias {
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.to_string().contains("new.strm") || error.to_string().contains("New.strm")));
        assert_eq!(fs::read(&index).unwrap(), before);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(fs::read_to_string(root.join("prior.strm")).unwrap().trim(), "https://example.test/prior");
    } else {
        result.unwrap();
        assert_eq!(fs::read_to_string(index).unwrap(), "New.strm\nnew.strm\n");
        assert_eq!(fs::read_to_string(root.join("New.strm")).unwrap().trim(), "https://example.test/first");
        assert_eq!(fs::read_to_string(root.join("new.strm")).unwrap().trim(), "https://example.test/second");
        assert!(!root.join("prior.strm").exists());
    }
}

fn m3u_test_target(name: &str, filename: &str) -> Arc<ConfigTarget> {
    Arc::new(target_with_options(
        name,
        vec![TargetOutput::M3u(M3uTargetOutput {
            filename: Some(filename.to_owned()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })],
        false,
    ))
}

#[tokio::test]
async fn replacement_rejects_an_unindexed_strm_hard_link_and_retires_the_indexed_name() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let mut target = m3u_test_target("selected", "catalog.m3u");
    Arc::make_mut(&mut target).output.push(TargetOutput::Strm(StrmTargetOutput::from(&StrmTargetOutputDto {
        directory: "strm".into(),
        flat: true,
        ..Default::default()
    })));
    let mut old = target_group(XtreamCluster::Live, 1, "old");
    old.channels[0].header.url = "https://example.test/old".intern();
    persist_playlist(&app, &mut [old], None, &target, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let indexed = directory.path().join("strm/old.strm");
    let alias = directory.path().join("strm/alias.strm");
    let previous = fs::read(&indexed).unwrap();
    fs::hard_link(&indexed, &alias).unwrap();
    let mut candidate = target_group(XtreamCluster::Live, 2, "alias");
    candidate.channels[0].header.url = "https://example.test/new".intern();
    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let errors = persist_playlist(&app, &mut [candidate], None, &target, None, replacement.clone()).await.unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("Unmanaged STRM file already exists")));
    assert_eq!(fs::read(&indexed).unwrap(), previous);
    assert_eq!(fs::read(&alias).unwrap(), previous);

    persist_playlist(&app, &mut [], None, &target, None, replacement).await.unwrap();
    assert!(!indexed.exists());
    assert_eq!(fs::read(alias).unwrap(), previous);
}

#[tokio::test]
async fn replacement_waits_for_ordinary_strm_to_finish_updating_ownership() {
    use crate::{storage::TARGET_EXPORT_LOCK, strm_repository::write_strm_playlist};
    use std::time::Duration;

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let output =
        StrmTargetOutput::from(&StrmTargetOutputDto { directory: "strm".into(), flat: true, ..Default::default() });
    let mut selected = m3u_test_target("selected", "selected.m3u");
    Arc::make_mut(&mut selected).output.push(TargetOutput::Strm(output.clone()));
    let mut other = m3u_test_target("other", "other.m3u");
    Arc::make_mut(&mut other).id = 2;
    Arc::make_mut(&mut other).output.push(TargetOutput::Strm(output.clone()));
    app.sources.store(Arc::new(SourcesConfig {
        sources: vec![ConfigSource { inputs: vec![], targets: vec![selected.clone(), other.clone()] }],
        ..Default::default()
    }));
    let mut old = target_group(XtreamCluster::Live, 1, "shared");
    old.channels[0].header.url = "https://example.test/old".intern();
    persist_playlist(&app, &mut [old.clone()], None, &selected, None, TargetPlaylistPersistOptions::default())
        .await
        .unwrap();

    let other_index = strm_get_file_paths(&hash_string_as_hex("strm"), &directory.path().join("other"));
    let index_lock = app.file_locks.write_lock(&other_index).await;
    let ordinary = tokio::spawn({
        let (app, old, other) = (app.clone(), old.clone(), other.clone());
        async move { write_strm_playlist(&app, &other, &output, &mut [old], false).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.file_locks.try_write_lock(Path::new(TARGET_EXPORT_LOCK)).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordinary writer holds the export lock while waiting for its index");

    let second_output = StrmTargetOutput::from(&StrmTargetOutputDto {
        directory: "parallel-strm".into(),
        flat: true,
        ..Default::default()
    });
    let mut parallel = [old.clone()];
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        write_strm_playlist(&app, &other, &second_output, &mut parallel, false),
    )
    .await
    .is_err());
    assert!(!directory.path().join("parallel-strm/shared.strm").exists());
    tokio::time::timeout(
        Duration::from_secs(5),
        write_strm_playlist(&app, &selected, &second_output, &mut parallel, false),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(directory.path().join("parallel-strm/shared.strm").is_file());

    old.channels[0].header.url = "https://example.test/changed".intern();
    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let mut candidate = [old];
    let replacing = persist_playlist(&app, &mut candidate, None, &selected, None, replacement);
    tokio::pin!(replacing);
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut replacing).await.is_err());
    drop(index_lock);
    ordinary.await.unwrap().unwrap();
    let errors = tokio::time::timeout(Duration::from_secs(5), replacing).await.unwrap().unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("also managed by another target")));
    assert_eq!(fs::read_to_string(directory.path().join("strm/shared.strm")).unwrap(), "https://example.test/old");
}

#[tokio::test]
async fn replacement_keeps_exports_available_and_preserves_a_concurrently_created_strm_file() {
    use crate::{
        m3u_repository::stage_m3u_playlist,
        strm_repository::stage_strm_playlist,
        xtream_repository::target_publish::{
            prepare_target_file_replacements, publish_prepared_target_file_replacements, sync_staged_target_files,
            TargetEmptyReplacementMode,
        },
    };

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let mut target = m3u_test_target("selected", "catalog.m3u");
    let strm =
        StrmTargetOutput::from(&StrmTargetOutputDto { directory: "strm".into(), flat: true, ..Default::default() });
    Arc::make_mut(&mut target).output.push(TargetOutput::Strm(strm.clone()));
    let target_path = directory.path().join("selected");
    let main = directory.path().join("catalog.m3u");
    let old_file = directory.path().join("strm/old.strm");
    let new_file = directory.path().join("strm/new.strm");

    let mut old = target_group(XtreamCluster::Live, 1, "old");
    old.channels[0].header.url = "https://example.test/old".intern();
    persist_playlist(&app, &mut [old.clone()], None, &target, None, TargetPlaylistPersistOptions::default())
        .await
        .unwrap();
    let previous_m3u = fs::read(&main).unwrap();
    let previous_strm = fs::read(&old_file).unwrap();
    let index = strm_get_file_paths(&hash_string_as_hex("strm"), &target_path);
    old.channels[0].header.url = "https://example.test/changed".intern();
    let mut new = target_group(XtreamCluster::Live, 2, "new");
    new.channels[0].header.url = "https://example.test/new".intern();

    for (before_preparation, previous_index) in [(true, "old.strm\nnew.strm\n"), (false, "old.strm\n")] {
        // An indexed path whose file is already missing must also remain unclaimed.
        fs::write(&index, previous_index).unwrap();
        let token = uuid::Uuid::new_v4().simple();
        let mut candidate = [old.clone(), new.clone()];
        let (_, mut files) =
            stage_m3u_playlist(&app, &target, target.get_m3u_output().unwrap(), &target_path, &candidate, token)
                .await
                .unwrap();
        let (_, strm_files) =
            stage_strm_playlist(&app, &target, &strm, &target_path, &mut candidate, token).await.unwrap();
        files.extend(strm_files);
        sync_staged_target_files(&files).unwrap();
        if before_preparation {
            fs::write(&new_file, "foreign").unwrap();
            let error = prepare_target_file_replacements(&mut files).unwrap_err();
            assert!(error.to_string().contains("changed after staging"));
        } else {
            prepare_target_file_replacements(&mut files).unwrap();
            assert_eq!(fs::read(&main).unwrap(), previous_m3u);
            assert_eq!(fs::read(&old_file).unwrap(), previous_strm);
            fs::write(&new_file, "foreign").unwrap();
            let error = publish_prepared_target_file_replacements(&mut files, &TargetEmptyReplacementMode::Persist)
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        }
        drop(files);
        assert_eq!(fs::read(&main).unwrap(), previous_m3u);
        assert_eq!(fs::read(&old_file).unwrap(), previous_strm);
        assert_eq!(fs::read_to_string(&new_file).unwrap(), "foreign");
        assert_eq!(fs::read_to_string(&index).unwrap(), previous_index);
        assert_eq!(fs::read_dir(directory.path().join("strm")).unwrap().count(), 2);
        fs::remove_file(&new_file).unwrap();
    }

    let mut m3u = target.get_m3u_output().unwrap().clone();
    m3u.filename = Some("strm/new.strm".into());
    let token = uuid::Uuid::new_v4().simple();
    let (_, mut files) = stage_m3u_playlist(&app, &target, &m3u, &target_path, &[new], token).await.unwrap();
    fs::write(&new_file, "foreign").unwrap();
    let error = prepare_target_file_replacements(&mut files).unwrap_err();
    assert!(error.to_string().contains("changed after staging"));
    assert_eq!(fs::read_to_string(new_file).unwrap(), "foreign");
}

#[tokio::test]
async fn m3u_replacement_rejects_main_and_provider_paths_owned_by_strm() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let mut owner = m3u_test_target("other", "other.m3u");
    Arc::make_mut(&mut owner).id = 2;
    Arc::make_mut(&mut owner).output.push(TargetOutput::Strm(StrmTargetOutput::from(&StrmTargetOutputDto {
        directory: "strm".into(),
        flat: true,
        ..Default::default()
    })));
    let mut old = target_group(XtreamCluster::Live, 1, "shared_provider");
    old.channels[0].header.url = "https://example.test/shared".intern();
    persist_playlist(&app, &mut [old], None, &owner, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let strm_file = directory.path().join("strm/shared_provider.strm");
    let previous = fs::read(&strm_file).unwrap();
    let input = m3u_provider_input();

    for (filename, artifact) in
        [("strm/shared_provider.strm", "M3U export"), ("strm/shared.strm", "Provider M3U export")]
    {
        let selected = m3u_test_target("selected", filename);
        app.sources.store(Arc::new(SourcesConfig {
            sources: vec![
                ConfigSource { inputs: vec![input.name.clone()], targets: vec![selected.clone()] },
                ConfigSource { inputs: vec![], targets: vec![owner.clone()] },
            ],
            inputs: vec![Arc::new(input.clone())],
            ..Default::default()
        }));
        let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
        let errors = persist_playlist(&app, &mut [], None, &selected, None, replacement).await.unwrap_err();
        assert!(errors.iter().any(|error| {
            let message = error.to_string();
            message.contains(artifact) && message.contains("also used by target 'other'")
        }));
        assert_eq!(fs::read(&strm_file).unwrap(), previous);
        assert!(!directory.path().join("selected/m3u/m3u.db").exists());
    }
}

#[tokio::test]
async fn ap12_m3u_replacement_rejects_main_paths_used_by_another_target() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let output = directory.path().join("plain.m3u");
    let owner = m3u_test_target("other", output.to_str().unwrap());
    let mut old = target_group(XtreamCluster::Live, 1, "old-live");
    old.channels[0].header.url = "https://example.test/old.ts".intern();
    persist_playlist(&app, &mut [old], None, &owner, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let db = directory.path().join("other/m3u/m3u.db");
    let previous = fs::read(&output).unwrap();
    let previous_db = fs::read(&db).unwrap();
    let mut names = vec!["plain.m3u"];
    if directory.path().join("PLAIN.m3u").is_file() {
        names.push("PLAIN.m3u");
    }
    for name in names {
        let selected = m3u_test_target("selected", name);
        app.sources.store(Arc::new(SourcesConfig {
            sources: vec![ConfigSource { inputs: vec![], targets: vec![selected.clone(), owner.clone()] }],
            ..Default::default()
        }));
        let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
        let errors = persist_playlist(&app, &mut [], None, &selected, None, replacement).await.unwrap_err();
        assert!(errors.iter().any(|error| error.to_string().contains("also used by target 'other'")));
        assert_eq!(fs::read(&output).unwrap(), previous);
        assert_eq!(fs::read(&db).unwrap(), previous_db);
        assert!(!directory.path().join("selected/m3u/m3u.db").exists());
    }
}

#[tokio::test]
async fn ap12_m3u_replacement_retires_the_legacy_provider_export_with_the_main_file() {
    check_m3u_provider_retirement(false).await;
    check_m3u_provider_retirement(true).await;
}

async fn check_m3u_provider_retirement(legacy: bool) {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let target = m3u_test_target("selected", "plain.m3u");
    let input = m3u_provider_input();
    let install = |input: ConfigInput, targets| {
        app.sources.store(Arc::new(SourcesConfig {
            sources: vec![ConfigSource { inputs: vec![input.name.clone()], targets }],
            inputs: vec![Arc::new(input)],
            ..Default::default()
        }));
    };
    install(input.clone(), vec![target.clone()]);
    let mut old = target_group(XtreamCluster::Live, 1, "old-live");
    old.channels[0].header.url = "provider://fixture/old.ts".intern();
    persist_playlist(&app, &mut [old], None, &target, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let main = directory.path().join("plain.m3u");
    let companion = directory.path().join("plain_provider.m3u");
    let owner = directory.path().join("selected/m3u/provider-export.json");
    let old_companion = fs::read_to_string(&companion).unwrap();
    assert!(old_companion.contains("provider://fixture/old.ts"));
    assert!(fs::read_to_string(&main).unwrap().contains("https://example.test/old.ts"));
    assert!(owner.is_file());
    if legacy {
        fs::remove_file(&owner).unwrap();
    }

    let direct = ConfigInput { url: "https://example.test".into(), ..input };
    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let other = m3u_test_target("other", "plain_provider.m3u");
    install(direct.clone(), vec![target.clone(), other.clone()]);
    let errors = persist_playlist(&app, &mut [], None, &other, None, replacement.clone()).await.unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("also used by target 'selected'")));
    assert_eq!(fs::read_to_string(&companion).unwrap(), old_companion);

    install(direct.clone(), vec![target.clone()]);
    let backup = main.with_extension("fixture");
    fs::rename(&main, &backup).unwrap();
    fs::create_dir(&main).unwrap();
    assert!(persist_playlist(&app, &mut [], None, &target, None, replacement.clone()).await.is_err());
    assert_eq!(fs::read_to_string(&companion).unwrap(), old_companion);
    fs::remove_dir(&main).unwrap();
    fs::rename(backup, &main).unwrap();

    persist_playlist(&app, &mut [], None, &target, None, replacement.clone()).await.unwrap();
    assert_eq!(fs::read_to_string(&main).unwrap(), "#EXTM3U\n");
    assert!(!companion.exists());
    assert!(!owner.exists());

    install(direct, vec![target.clone(), other]);
    fs::write(&companion, &old_companion).unwrap();
    persist_playlist(&app, &mut [], None, &target, None, replacement).await.unwrap();
    assert_eq!(fs::read_to_string(companion).unwrap(), old_companion);
}

fn m3u_provider_input() -> ConfigInput {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "fixture".into(),
        urls: vec!["https://example.test".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::default(),
        dns: None,
    }));
    ConfigInput {
        name: "target-test-input".intern(),
        url: "provider://fixture".into(),
        provider_configs: Some(vec![provider]),
        ..Default::default()
    }
}

#[tokio::test]
async fn ordinary_m3u_update_adopts_a_legacy_provider_export_with_different_bytes() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let target = m3u_test_target("selected", "plain.m3u");
    let input = m3u_provider_input();
    app.sources.store(Arc::new(SourcesConfig {
        sources: vec![ConfigSource { inputs: vec![input.name.clone()], targets: vec![target.clone()] }],
        inputs: vec![Arc::new(input)],
        ..Default::default()
    }));
    let mut playlist = [target_group(XtreamCluster::Live, 1, "live")];
    playlist[0].channels[0].header.url = "provider://fixture/live.ts".intern();
    let ordinary = TargetPlaylistPersistOptions::default();
    persist_playlist(&app, &mut playlist, None, &target, None, ordinary.clone()).await.unwrap();

    let companion = directory.path().join("plain_provider.m3u");
    let owner = directory.path().join("selected/m3u/provider-export.json");
    let previous = fs::read_to_string(&companion).unwrap();
    let legacy = format!("{previous}# Legacy export formatting\n");
    fs::remove_file(&owner).unwrap();
    fs::write(&companion, &legacy).unwrap();

    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let errors = persist_playlist(&app, &mut playlist, None, &target, None, replacement).await.unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("Cannot verify ownership of provider M3U")));
    assert_eq!(fs::read_to_string(&companion).unwrap(), legacy);

    persist_playlist(&app, &mut playlist, None, &target, None, ordinary).await.unwrap();
    assert_eq!(fs::read_to_string(&companion).unwrap(), previous);
    assert!(owner.is_file());
}

#[tokio::test]
async fn ap12_m3u_replacement_preserves_an_unowned_or_changed_companion() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let target = m3u_test_target("selected", "plain.m3u");
    let companion = directory.path().join("plain_provider.m3u");
    let foreign = "#EXTM3U\n#EXTINF:-1,Independent playlist\nhttps://example.test/foreign.ts\n";
    fs::write(&companion, foreign).unwrap();
    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    persist_playlist(&app, &mut [], None, &target, None, replacement.clone()).await.unwrap();
    assert_eq!(fs::read_to_string(&companion).unwrap(), foreign);

    let owner = directory.path().join("selected/m3u/provider-export.json");
    let recorded_digest = blake3::hash(b"previous generated export").to_hex().to_string();
    fs::write(&owner, serde_json::to_vec(&serde_json::json!({"path":companion,"digest":recorded_digest})).unwrap())
        .unwrap();
    persist_playlist(&app, &mut [], None, &target, None, replacement).await.unwrap();
    assert_eq!(fs::read_to_string(&companion).unwrap(), foreign);
    assert!(owner.exists());
}

#[test]
fn target_force_empty_authorization_is_limited_to_xtream_outputs() {
    let xtream_target = target_with_outputs(vec![TargetOutput::Xtream(XtreamTargetOutput {
        flags: XtreamTargetFlagsSet::new(),
        trakt: None,
        filter: None,
    })]);
    let options = TargetPlaylistPersistOptions {
        accepted_empty_clusters: ClusterFlags::Vod,
        ..TargetPlaylistPersistOptions::default()
    };
    assert!(validate_target_playlist_persistence(&xtream_target, true, &options).is_ok());

    let mixed_target = target_with_outputs(vec![
        TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
        TargetOutput::M3u(M3uTargetOutput {
            filename: None,
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        }),
    ]);
    let error = validate_target_playlist_persistence(&mixed_target, true, &options)
        .expect_err("fully empty mixed output must fail before persistence");
    assert!(error.to_string().contains("non-Xtream outputs"));
}

#[tokio::test]
async fn force_replacement_can_publish_empty_filtered_m3u_and_strm() {
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let vod_only = get_filter("Type = vod", None).unwrap();
    let mut target = m3u_test_target("selected", "catalog.m3u");
    let TargetOutput::M3u(m3u) = &mut Arc::make_mut(&mut target).output[0] else { unreachable!() };
    m3u.filter = Some(vod_only.clone());
    let mut strm =
        StrmTargetOutput::from(&StrmTargetOutputDto { directory: "strm".into(), flat: true, ..Default::default() });
    strm.filter = Some(vod_only);
    Arc::make_mut(&mut target).output.push(TargetOutput::Strm(strm));
    let mut prior = target_group(XtreamCluster::Video, 1, "old film");
    prior.channels[0].header.url = "https://example.test/old-film".intern();
    persist_playlist(&app, &mut [prior], None, &target, None, TargetPlaylistPersistOptions::default()).await.unwrap();
    let m3u = directory.path().join("catalog.m3u");
    let index = strm_get_file_paths(&hash_string_as_hex("strm"), &directory.path().join("selected"));
    let previous = fs::read_to_string(&index).unwrap();
    assert_ne!(previous, "");
    assert!(fs::read_to_string(&m3u).unwrap().contains("old film"));

    let mut live = [target_group(XtreamCluster::Live, 2, "current live")];
    let force = TargetPlaylistPersistOptions { accepted_empty_clusters: ClusterFlags::Vod, ..Default::default() };
    let errors = persist_playlist(&app, &mut live, None, &target, None, force.clone()).await.unwrap_err();
    assert!(errors.iter().any(|error| error.to_string().contains("Refusing to publish force-empty M3U")));
    assert_eq!(fs::read_to_string(&index).unwrap(), previous);
    assert!(fs::read_to_string(&m3u).unwrap().contains("old film"));

    let replacement = TargetPlaylistPersistOptions { replace_target_contents: true, ..force };
    persist_playlist(&app, &mut live, None, &target, None, replacement).await.unwrap();
    assert_eq!(fs::read_to_string(m3u).unwrap(), "#EXTM3U\n");
    assert_eq!(fs::read_to_string(index).unwrap(), "");
    for path in previous.lines() {
        assert!(!directory.path().join("strm").join(path).exists());
    }
}

#[tokio::test]
async fn target_empty_replacement_failures_leave_the_memory_cache_unchanged() {
    for failure in [
        crate::TargetEmptyReplacementFailure::CategoryPersistence,
        crate::TargetEmptyReplacementFailure::BTreePersistence,
        crate::TargetEmptyReplacementFailure::Publication,
    ] {
        let directory = tempdir().expect("temporary storage");
        let app_config = app_config_in(directory.path());
        let playlist_state = Arc::new(PlaylistStorageState::new());
        let target = xtream_target(&format!("empty-persist-{failure:?}"), true);
        seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
        let before = cached_target_signature(&playlist_state, &target.name).await;
        let mut candidate = vec![
            target_group(XtreamCluster::Live, 102, "new-live"),
            target_group(XtreamCluster::Series, 302, "new-series"),
        ];

        let errors = persist_playlist_with_mode(
            &app_config,
            &mut candidate,
            None,
            &target,
            Some(&playlist_state),
            TargetPlaylistPersistOptions {
                accepted_empty_clusters: ClusterFlags::Vod,
                ..TargetPlaylistPersistOptions::default()
            },
            TargetPersistenceMode::FailEmptyReplacementAt(failure),
        )
        .await
        .expect_err("injected empty replacement must fail");

        assert!(errors.iter().any(|error| error.to_string().contains("target cluster failed")));
        assert_eq!(cached_target_signature(&playlist_state, &target.name).await, before);
        let persisted = load_xtream_target_storage(&app_config, &target).await.expect("retained target storage");
        assert_eq!(persisted.vod.len(), 1);
    }
}

#[test]
// Unchanged by the move; rustfmt reflowed it past the 100-line threshold.
#[allow(clippy::too_many_lines)]
fn materializes_media_server_series_info_episodes_after_target_virtual_ids_are_assigned() {
    let series_uuid = "123e4567-e89b-12d3-a456-426614174111";
    let mut series_info = PlaylistItem {
        header: PlaylistItemHeader {
            uuid: UUIDType::from_valid_uuid(series_uuid),
            id: "media-server:server:shows:series:series".intern(),
            name: "Media Server Series".intern(),
            title: "Media Server Series".intern(),
            url: "media-server://unavailable/server/shows/series".intern(),
            item_type: PlaylistItemType::SeriesInfo,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                name: "Media Server Series".intern(),
                details: Some(SeriesStreamDetailProperties {
                    year: Some(2024),
                    seasons: Some(vec![SeriesStreamDetailSeasonProperties {
                        name: "Season 1".intern(),
                        season_number: 1,
                        episode_count: 2,
                        overview: Some("season summary".intern()),
                        air_date: Some("2024-01-01".intern()),
                        cover: None,
                        cover_tmdb: None,
                        cover_big: None,
                        duration: None,
                    }]),
                    episodes: None,
                }),
                ..SeriesStreamProperties::default()
            }))),
            ..PlaylistItemHeader::default()
        },
    };
    let series_parent_code = series_info.header.uuid.to_string();
    let media_episode_two =
        PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-two", 7002, 1, 2) };
    let media_episode_one =
        PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-one", 7001, 1, 1) };
    let malformed_media_episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "media-server:server:shows:episode:malformed".intern(),
            parent_code: series_parent_code.clone().intern(),
            url: "media-server://plex/server/malformed?part_key=%2Flibrary%2Fparts%2Fredacted".intern(),
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(7003),
            additional_properties: None,
            ..PlaylistItemHeader::default()
        },
    };
    let provider_episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "999".intern(),
            parent_code: series_parent_code.clone().intern(),
            url: "http://provider.example.invalid/series/999.mkv".intern(),
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(7999),
            ..PlaylistItemHeader::default()
        },
    };

    let mut media_server_series = HashMap::<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>::new();
    assign_media_server_series_info_episode(&mut media_server_series, &media_episode_two.header);
    assign_media_server_series_info_episode(&mut media_server_series, &provider_episode.header);
    assign_media_server_series_info_episode(&mut media_server_series, &malformed_media_episode.header);
    assign_media_server_series_info_episode(&mut media_server_series, &media_episode_one.header);

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Media Server Series".intern(),
        channels: vec![series_info, media_episode_two, provider_episode, malformed_media_episode, media_episode_one],
        xtream_cluster: XtreamCluster::Series,
    }];

    materialize_media_server_series_info_episodes(&mut playlist, &media_server_series);
    series_info = playlist[0].channels[0].clone();

    let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let details = series.details.as_ref().expect("series details should be present");
    assert_eq!(details.year, Some(2024));
    assert_eq!(details.seasons.as_ref().expect("seasons should be preserved")[0].episode_count, 2);
    let episodes = details.episodes.as_ref().expect("media-server episodes should be materialized");
    assert_eq!(episodes.len(), 2);
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[0].episode_num, 1);
    assert_eq!(episodes[0].season, 1);
    assert_eq!(episodes[0].title.as_ref(), "Episode 1");
    assert_eq!(episodes[0].container_extension.as_ref(), "mkv");
    assert_eq!(episodes[0].release_date.as_ref(), "2024-02-03");
    assert_eq!(episodes[0].series_release_date.as_deref(), Some("2024-01-01"));
    assert_eq!(episodes[0].plot.as_deref(), Some("Episode summary"));
    assert_eq!(episodes[0].tmdb, Some(67890));
    assert_eq!(episodes[0].direct_source.as_ref(), "");
    assert_eq!(
        episodes[0].movie_image.as_ref(),
        "media-server://image/plex/server/episode?image_path=%2Flibrary%2Fmetadata%2Fredacted%2Fthumb"
    );
    assert!(episodes[0].video.as_deref().is_some_and(|video| video.contains("h264")));
    assert!(episodes[0].audio.as_deref().is_some_and(|audio| audio.contains("aac")));
    assert_eq!(episodes[1].id, 7002);
    assert!(!episodes.iter().any(|episode| episode.id == 7003));
}

#[test]
fn rewrite_series_episode_parent_virtual_ids_updates_local_episode_mapping() {
    let series_uuid = "123e4567-e89b-12d3-a456-426614174000";
    let series_uuid_type = UUIDType::from_valid_uuid(series_uuid);
    let episode_uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174001");
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![
            PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: episode_uuid,
                    id: "101".intern(),
                    parent_code: series_uuid.intern(),
                    url: "/library/episode1.mkv".intern(),
                    item_type: PlaylistItemType::LocalSeries,
                    xtream_cluster: XtreamCluster::Series,
                    ..PlaylistItemHeader::default()
                },
            },
            PlaylistItem {
                header: PlaylistItemHeader {
                    uuid: series_uuid_type,
                    id: series_uuid.intern(),
                    item_type: PlaylistItemType::LocalSeriesInfo,
                    xtream_cluster: XtreamCluster::Series,
                    ..PlaylistItemHeader::default()
                },
            },
        ],
        xtream_cluster: XtreamCluster::Series,
    }];

    for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
        let uuid = channel.header.uuid;
        let provider_id = channel.header.get_provider_id().unwrap_or_default();
        let item_type = channel.header.item_type;
        channel.header.virtual_id =
            target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
        channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
    }

    let series_virtual_id = playlist[0].channels[1].header.virtual_id;
    let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

    rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, series_virtual_id);
    assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
}

#[test]
fn rewrite_series_episode_parent_virtual_ids_updates_provider_episode_mapping_using_series_info_uuid() {
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let input_name = "provider-input".intern();
    let xtream_series_info = XtreamPlaylistItem {
        virtual_id: VirtualId::new(0),
        provider_id: 9001,
        name: "Provider Series".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Provider Series".intern(),
        title: "Provider Series".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://provider.example.com/series/user/pass/9001".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: None,
        item_type: PlaylistItemType::SeriesInfo,
        category_id: 0,
        input_name: Arc::clone(&input_name),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "9001".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };
    let provider_parent_code = xtream_series_info.get_uuid().intern();
    let xtream_provider_episode = XtreamPlaylistItem {
        virtual_id: VirtualId::new(0),
        provider_id: 201,
        name: "Episode 1".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Provider Series".intern(),
        title: "Episode 1".intern(),
        parent_code: provider_parent_code,
        rec: "".intern(),
        url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: None,
        item_type: PlaylistItemType::Series,
        category_id: 0,
        input_name,
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "201".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };
    let provider_episode = PlaylistItem::from(&xtream_provider_episode);
    let mut series_info = PlaylistItem::from(&xtream_series_info);
    series_info.header.uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174099");

    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Provider Series".intern(),
        channels: vec![provider_episode, series_info],
        xtream_cluster: XtreamCluster::Series,
    }];

    for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
        let uuid = channel.get_uuid();
        let provider_id = channel.header.get_provider_id().unwrap_or_default();
        let item_type = channel.header.item_type;
        channel.header.virtual_id =
            target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
        channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
    }

    let series_virtual_id = playlist[0].channels[1].header.virtual_id;
    let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

    rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, series_virtual_id);
    assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
}

#[test]
fn initial_virtual_id_assignment_preserves_existing_parent_for_series_episode_without_parent_match() {
    let dir = tempdir().expect("tempdir");
    let mapping_path = dir.path().join("id_mapping.db");
    let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

    let input_name = "provider-input".intern();
    let mut episode = PlaylistItem {
        header: PlaylistItemHeader {
            id: "201".intern(),
            url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
            input_name,
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            ..PlaylistItemHeader::default()
        },
    };

    let provider_id = episode.header.get_provider_id().unwrap_or_default();
    let uuid = *episode.header.get_uuid();
    let original_virtual_id =
        target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, episode.header.item_type, VirtualId::new(77));

    let preserved_parent_virtual_id = target_id_mapping.get_parent_virtual_id_by_uuid(&uuid).unwrap_or_default();
    episode.header.virtual_id = target_id_mapping.get_and_update_virtual_id(
        &uuid,
        provider_id,
        episode.header.item_type,
        preserved_parent_virtual_id,
    );
    target_id_mapping.persist().expect("persist");

    let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
    let record = query.query_zero_copy(&original_virtual_id.get()).expect("query ok").expect("record missing");

    assert_eq!(record.parent_virtual_id, VirtualId::new(77));
    assert_eq!(episode.header.virtual_id, original_virtual_id);
}

#[test]
fn rewrite_local_series_info_uses_series_uuid_lookup_and_updates_episode_virtual_ids() {
    let series_uuid = "series-uuid";
    let mut series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    local_library_series.insert(
        series_uuid.intern(),
        vec![
            LocalEpisodeKey { path: "/library/episode1.mkv".intern(), virtual_id: 7001 },
            LocalEpisodeKey { path: "/library/episode2.mkv".intern(), virtual_id: 7002 },
        ],
    );

    rewrite_local_series_info_episode_virtual_id(&mut series_info, &local_library_series);

    let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
}

#[test]
fn rewrite_series_info_updates_local_episode_ids_before_parent_code_is_cleared() {
    let series_uuid = "series-uuid";
    let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
    let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_one, PlaylistItemType::LocalSeries);
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_two, PlaylistItemType::LocalSeries);

    let series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let local_episode_one = PlaylistItem { header: episode_one };
    let local_episode_two = PlaylistItem { header: episode_two };
    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![series_info, local_episode_one, local_episode_two],
        xtream_cluster: XtreamCluster::Series,
    }];

    rewrite_series_info_episode_virtual_id(
        &mut playlist,
        &local_library_series,
        &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
    );

    let Some(StreamProperties::Series(series)) = playlist[0].channels[0].header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
    assert!(playlist[0].channels[1].header.parent_code.is_empty());
    assert!(playlist[0].channels[2].header.parent_code.is_empty());
}

#[test]
fn rewrite_series_info_updates_local_episode_ids_when_episodes_come_first() {
    // Test with episodes BEFORE series_info to verify iteration-order doesn't matter
    let series_uuid = "series-uuid";
    let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
    let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_one, PlaylistItemType::LocalSeries);
    assign_local_series_info_episode_key(&mut local_library_series, &mut episode_two, PlaylistItemType::LocalSeries);

    let series_info = make_local_series_info(
        series_uuid,
        vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
    );
    let local_episode_one = PlaylistItem { header: episode_one };
    let local_episode_two = PlaylistItem { header: episode_two };

    // Episodes FIRST, then series_info (reversed order)
    let mut playlist = vec![PlaylistGroup {
        id: 1,
        title: "Series".intern(),
        channels: vec![local_episode_one, local_episode_two, series_info],
        xtream_cluster: XtreamCluster::Series,
    }];

    rewrite_series_info_episode_virtual_id(
        &mut playlist,
        &local_library_series,
        &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
    );

    let Some(StreamProperties::Series(series)) = playlist[0].channels[2].header.additional_properties.as_ref() else {
        panic!("missing series properties");
    };
    let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
    assert_eq!(episodes[0].id, 7001);
    assert_eq!(episodes[1].id, 7002);
    assert!(playlist[0].channels[0].header.parent_code.is_empty());
    assert!(playlist[0].channels[1].header.parent_code.is_empty());
}

#[tokio::test]
async fn replace_target_contents_rolls_back_all_artifacts_then_allows_a_retry() {
    use crate::{get_live_cat_collection_path, xtream_get_item_for_stream_id};
    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("replace", true);
    let other = xtream_target("unselected", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;
    seed_memory_cached_xtream_target(&app, &state, &other).await;
    let before = cached_target_signature(&state, &target.name).await;
    let other_before = cached_target_signature(&state, &other.name).await;
    let config = app.config.load();
    let target_path = get_target_storage_path(&config, &target.name).unwrap();
    let storage = xtream_get_storage_path(&config, &target.name).unwrap();
    let category_paths = [
        get_live_cat_collection_path(&storage),
        get_vod_cat_collection_path(&storage),
        get_series_cat_collection_path(&storage),
    ];
    let old_categories = category_paths.iter().map(|path| std::fs::read(path).unwrap()).collect::<Vec<_>>();
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    let old_mapping = std::fs::read(&mapping_path).unwrap();
    let options = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
    let mut candidate = vec![target_group(XtreamCluster::Live, 102, "new-live")];

    let errors = persist_playlist_with_mode(
        &app,
        &mut candidate,
        None,
        &target,
        Some(&state),
        options.clone(),
        TargetPersistenceMode::FailEmptyReplacementAt(crate::TargetEmptyReplacementFailure::Publication),
    )
    .await
    .unwrap_err();
    assert!(errors[0].message().contains("injected"));
    assert_eq!(cached_target_signature(&state, &target.name).await, before);
    assert_eq!(std::fs::read(&mapping_path).unwrap(), old_mapping);
    for (path, previous) in category_paths.iter().zip(&old_categories) {
        assert_eq!(&std::fs::read(path).unwrap(), previous);
    }
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [1, 1, 1]);

    persist_playlist_with_mode(
        &app,
        &mut candidate,
        None,
        &target,
        Some(&state),
        options.clone(),
        TargetPersistenceMode::Persist,
    )
    .await
    .unwrap();
    let cached = cached_target_signature(&state, &target.name).await;
    assert_eq!(cached.0, before.0 + 1);
    assert_eq!(
        cached.1.iter().map(|(cluster, _, name)| (*cluster, name.as_ref())).collect::<Vec<_>>(),
        [(XtreamCluster::Live, "new-live")]
    );
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [1, 0, 0]);
    for (cluster, id, _) in before.1 {
        assert!(xtream_get_item_for_stream_id(id, &app, &state, &target, Some(cluster)).await.is_err());
    }
    assert_eq!(std::fs::read_to_string(&category_paths[1]).unwrap(), "[]");
    assert_eq!(std::fs::read_to_string(&category_paths[2]).unwrap(), "[]");
    assert_eq!(cached_target_signature(&state, &other.name).await, other_before);

    persist_playlist_with_mode(&app, &mut [], None, &target, Some(&state), options, TargetPersistenceMode::Persist)
        .await
        .unwrap();
    let on_disk = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([on_disk.live.len(), on_disk.vod.len(), on_disk.series.len()], [0, 0, 0]);
    assert_eq!(cached_target_signature(&state, &target.name).await, (cached.0, Vec::new()));
    for path in &category_paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[]");
    }
}

#[tokio::test(start_paused = true)]
async fn replace_target_contents_deadline_waits_for_started_publication() {
    use crate::{xtream_repository::target_publish::TargetEmptyPublicationHook, TargetReplacementDeadline};
    use std::time::Duration;
    use tokio::time::{advance, Instant};

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("deadline-started", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;

    let deadline = Arc::new(TargetReplacementDeadline::new(Instant::now() + Duration::from_secs(60)));
    let options = TargetPlaylistPersistOptions {
        replace_target_contents: true,
        replacement_deadline: Some(deadline.clone()),
        ..Default::default()
    };
    let hook = TargetEmptyPublicationHook::new();
    let entered = Arc::clone(&hook.backup_window_entered);
    let resume = Arc::clone(&hook.resume_publication);
    let writer = tokio::spawn({
        let (app, state, target) = (app.clone(), state.clone(), target.clone());
        async move {
            let mut candidate = [target_group(XtreamCluster::Live, 102, "new-live")];
            deadline
                .run(async {
                    persist_playlist_with_mode(
                        &app,
                        &mut candidate,
                        None,
                        &target,
                        Some(&state),
                        options,
                        TargetPersistenceMode::PauseDuringReplacement(hook),
                    )
                    .await
                    .unwrap();
                    std::future::pending::<()>().await;
                })
                .await
        }
    });

    tokio::task::spawn_blocking(move || entered.wait()).await.unwrap();
    advance(Duration::from_secs(61)).await;
    tokio::task::yield_now().await;
    let returned_before_commit = writer.is_finished();
    tokio::task::spawn_blocking(move || resume.wait()).await.unwrap();
    assert!(!returned_before_commit, "the run must retain the publication result after the deadline");
    let published = tokio::time::timeout(Duration::from_secs(1), writer)
        .await
        .expect("only the active commit may delay the timeout")
        .unwrap()
        .unwrap_err();
    assert_eq!(published.as_slice(), std::slice::from_ref(&target.name));

    let stored = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([stored.live.len(), stored.vod.len(), stored.series.len()], [1, 0, 0]);
    assert_eq!(cached_target_signature(&state, &target.name).await.1[0].2.as_ref(), "new-live");
    let config = app.config.load();
    let storage = xtream_get_storage_path(&config, &target.name).unwrap();
    assert_eq!(std::fs::read_to_string(get_vod_cat_collection_path(&storage)).unwrap(), "[]");
    assert!(std::fs::read_dir(&storage)
        .unwrap()
        .all(|entry| { !entry.unwrap().file_name().to_string_lossy().contains("force-empty") }));
    eprintln!("AP11 S04 countercheck completed-publication={published:?} disk=[1, 0, 0] cache=new-live VOD-categories=[] remaining=[]");
}

#[tokio::test]
async fn run_drains_an_ordinary_publication_after_processing_stops_early() {
    use crate::playlist_repository::{OrdinaryPublication, TargetReplacementDeadline};
    use std::time::Duration;
    use tokio::{sync::oneshot, time::Instant};

    let deadline = Arc::new(TargetReplacementDeadline::new(Instant::now() + Duration::from_secs(60)));
    let (entered, entered_rx) = oneshot::channel();
    let (finish, finish_rx) = oneshot::channel();
    let (stopped, stopped_rx) = oneshot::channel();
    let mut run = tokio::spawn(async move {
        deadline
            .run(async {
                let publisher = deadline.clone();
                let caller = tokio::spawn(async move {
                    publisher
                        .publish_ordinary(|publication| async move {
                            OrdinaryPublication::begin(Some(&publication)).unwrap();
                            entered.send(()).unwrap();
                            finish_rx.await.unwrap();
                        })
                        .await
                });
                entered_rx.await.unwrap();
                caller.abort();
                assert!(caller.await.unwrap_err().is_cancelled());
                stopped.send(()).unwrap();
            })
            .await
    });
    stopped_rx.await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut run).await.is_err());
    finish.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn ap11_s02_partial_xtream_staging_failure_cleans_owned_files() {
    use crate::publication_fixture::{pause_target_staging, TargetStagingPause};
    use std::{collections::BTreeMap, fs};

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("staging-failure", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;
    let cache_before = cached_target_signature(&state, &target.name).await;
    let storage = xtream_get_storage_path(&app.config.load(), &target.name).unwrap();
    let on_disk = || {
        fs::read_dir(&storage)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect::<BTreeMap<_, _>>()
    };
    let original = on_disk();
    let TargetStagingPause { entered, resume, written, cleaned } =
        pause_target_staging(&storage, XtreamCluster::Video, true);
    let mut writer = tokio::spawn({
        let (app, state, target) = (app.clone(), state.clone(), target.clone());
        async move {
            let mut candidate = [
                target_group(XtreamCluster::Live, 102, "new-live"),
                target_group(XtreamCluster::Video, 202, "new-vod"),
            ];
            persist_playlist_with_mode(
                &app,
                &mut candidate,
                None,
                &target,
                Some(&state),
                TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() },
                TargetPersistenceMode::Persist,
            )
            .await
        }
    });
    let files = tokio::select! {
        files = entered => files.unwrap(),
        result = &mut writer => panic!("replacement finished before the Xtream writer paused: {result:?}"),
    };
    assert_eq!(files.existing(), std::slice::from_ref(&files.categories));
    let partial =
        on_disk().into_keys().filter(|name| name.to_string_lossy().contains("force-empty")).collect::<Vec<_>>();
    assert!(partial.iter().any(|name| name.to_string_lossy().starts_with(".live.force-empty-")));
    assert!(!partial.iter().any(|name| name.to_string_lossy().starts_with(".series.force-empty-")));
    resume.send(()).unwrap();
    let written = written.await.unwrap();
    assert_eq!(
        written,
        [&files.database, &files.index, &files.categories]
            .into_iter()
            .chain(&files.sidecars)
            .cloned()
            .collect::<Vec<_>>()
    );
    cleaned.await.unwrap();
    let errors = writer.await.unwrap().unwrap_err();
    assert!(errors[0].message().contains("injected target staging failure"));
    assert_eq!(files.existing(), Vec::<std::path::PathBuf>::new());
    assert_eq!(on_disk(), original);
    let stored = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([stored.live.len(), stored.vod.len(), stored.series.len()], [1, 1, 1]);
    assert_eq!(cached_target_signature(&state, &target.name).await, cache_before);
    let written: Vec<_> = written.iter().map(|path| path.file_name().unwrap()).collect();
    eprintln!("AP11 S02 partial={partial:?} written={written:?} remaining=[] disk=[1, 1, 1] cache-and-published-files=unchanged published_files={:?}", original.keys().collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn replace_target_contents_deadline_rejects_late_publication() {
    use crate::{xtream_category_view_lock_path, TargetReplacementDeadline};
    use std::time::Duration;
    use tokio::time::{advance, Instant};

    let directory = tempdir().unwrap();
    let app = app_config_in(directory.path());
    let state = Arc::new(PlaylistStorageState::new());
    let target = xtream_target("deadline-expired", true);
    seed_memory_cached_xtream_target(&app, &state, &target).await;
    let before = cached_target_signature(&state, &target.name).await;
    let storage = xtream_get_storage_path(&app.config.load(), &target.name).unwrap();
    let vod_path = get_vod_cat_collection_path(&storage);
    let categories = std::fs::read(&vod_path).unwrap();
    let reader = app.file_locks.read_lock(&xtream_category_view_lock_path(&storage)).await;

    let deadline = Arc::new(TargetReplacementDeadline::new(Instant::now() + Duration::from_secs(60)));
    let options = TargetPlaylistPersistOptions {
        replace_target_contents: true,
        replacement_deadline: Some(deadline.clone()),
        ..Default::default()
    };
    let mut writer = tokio::spawn({
        let (app, state, target) = (app.clone(), state.clone(), target.clone());
        async move {
            persist_playlist_with_mode(
                &app,
                &mut [],
                None,
                &target,
                Some(&state),
                options,
                TargetPersistenceMode::Persist,
            )
            .await
        }
    });
    advance(Duration::from_secs(61)).await;
    // Keep the test task alive across timeout to exercise the final guard even
    // if its caller no longer waits for it.
    assert_eq!(deadline.run(&mut writer).await.unwrap_err(), Vec::<String>::new());
    drop(reader);
    let errors = writer.await.unwrap().unwrap_err();
    assert!(errors[0].message().contains("deadline expired before publication"));
    let stored = load_xtream_target_storage(&app, &target).await.unwrap();
    assert_eq!([stored.live.len(), stored.vod.len(), stored.series.len()], [1, 1, 1]);
    assert_eq!(cached_target_signature(&state, &target.name).await, before);
    assert_eq!(std::fs::read(&vod_path).unwrap(), categories);
}
