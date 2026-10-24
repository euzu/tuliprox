use super::*;
use crate::test_config::{app_config_from, empty_paths};
use shared::model::{ChannelMemberRefDto, ConfigPaths, FailoverFileDto, ManualFailoverGroupDto};
use std::path::Path;
use tuliprox_core::model::Config;

fn test_app_config(config_path: &Path) -> AppConfig {
    let paths = ConfigPaths { config_path: config_path.to_string_lossy().into_owned(), ..empty_paths() };
    app_config_from(Config::default(), paths, None)
}

/// App config whose snapshot was loaded from `source`, like the startup loader does.
fn loaded_app_config(config_path: &Path, source: &FailoverSource) -> AppConfig {
    let app_config = test_app_config(config_path);
    app_config.failover.store(Arc::new(load_failover_overrides(source).expect("loads")));
    app_config
}

fn member(input: &str, stream_id: &str) -> ChannelMemberRefDto {
    ChannelMemberRefDto { input: input.to_string(), stream_id: stream_id.to_string() }
}

fn section(target: &str, group_id: &str) -> TargetFailoverOverridesDto {
    let mut dto = TargetFailoverOverridesDto::new(target);
    dto.groups.push(ManualFailoverGroupDto {
        id: group_id.to_string(),
        name: format!("Group {group_id}"),
        members: vec![member("prov-a", "1")],
        ..ManualFailoverGroupDto::default()
    });
    dto
}

fn write_file(path: &Path, targets: Vec<TargetFailoverOverridesDto>) {
    let dto = FailoverFileDto { version: 1, targets };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create dir");
    }
    std::fs::write(path, serde_saphyr::to_string(&dto).expect("serialize")).expect("write file");
}

fn read_file(path: &Path) -> FailoverFileDto {
    serde_saphyr::from_str(&std::fs::read_to_string(path).expect("read file")).expect("parse file")
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn load_merges_files_sorted_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    write_file(&failover_dir.join("b.yml"), vec![section("Kids", "kids-1")]);
    write_file(&failover_dir.join("a.yml"), vec![section("Main", "main-1"), section("News", "news-1")]);

    let loaded = load_failover_overrides(&FailoverSource::Directory(failover_dir.clone())).expect("loads");

    assert!(loaded.error.is_none());
    assert_eq!(loaded.files_used, vec![failover_dir.join("a.yml"), failover_dir.join("b.yml")]);
    let mut targets: Vec<&str> = loaded.sections.keys().map(AsRef::as_ref).collect();
    targets.sort_unstable();
    assert_eq!(targets, ["Kids", "Main", "News"]);
    let main = loaded.sections.get("Main").expect("main");
    assert_eq!(main.file, failover_dir.join("a.yml"));
    assert_eq!(main.etag.as_ref(), main.overrides.etag());
    assert!(main.findings.is_empty());
}

#[test]
fn load_reports_validation_findings_without_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "Bad Id")]);

    let loaded = load_failover_overrides(&FailoverSource::File(path)).expect("loads");

    let main = loaded.sections.get("Main").expect("main");
    assert_eq!(main.findings, vec![OverrideValidationError::InvalidManualId("Bad Id".to_string())]);
}

#[test]
fn load_rejects_duplicate_target_across_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    write_file(&failover_dir.join("a.yml"), vec![section("Main", "g1")]);
    write_file(&failover_dir.join("b.yml"), vec![section("Main", "g2")]);

    let err = load_failover_overrides(&FailoverSource::Directory(failover_dir.clone())).expect_err("duplicate");

    let message = err.to_string();
    assert!(message.contains("Target 'Main' is defined in"), "{message}");
    assert!(message.contains("a.yml") && message.contains("b.yml"), "{message}");
}

#[test]
fn load_rejects_duplicate_target_in_one_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "g1"), section("Main", "g2")]);

    let err = load_failover_overrides(&FailoverSource::File(path)).expect_err("duplicate");

    assert!(err.to_string().contains("Target 'Main' is defined in"), "{err}");
}

#[test]
fn load_rejects_unsupported_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    std::fs::write(&path, "version: 2\ntargets: []\n").expect("write");

    let err = load_failover_overrides(&FailoverSource::File(path)).expect_err("version");

    assert!(err.to_string().contains("version"), "{err}");
}

#[test]
fn load_rejects_invalid_yaml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    std::fs::write(&path, "version: [1\n").expect("write");

    assert!(load_failover_overrides(&FailoverSource::File(path)).is_err());
}

#[test]
fn load_missing_source_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    for source in [
        FailoverSource::File(dir.path().join("failover.yml")),
        FailoverSource::Directory(dir.path().join("failover.d")),
    ] {
        let loaded = load_failover_overrides(&source).expect("loads");
        assert!(loaded.sections.is_empty());
        assert!(loaded.files_used.is_empty());
        assert!(loaded.error.is_none());
        assert_eq!(loaded.source, Some(source));
    }
}

#[tokio::test]
async fn write_updates_section_in_owning_file_and_keeps_other_targets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    write_file(&failover_dir.join("a.yml"), vec![section("Main", "main-1"), section("News", "news-1")]);
    write_file(&failover_dir.join("b.yml"), vec![section("Kids", "kids-1")]);
    let source = FailoverSource::Directory(failover_dir.clone());
    let app_config = loaded_app_config(dir.path(), &source);
    let old_etag = app_config.failover_section("Main").expect("main").etag.to_string();

    let entry = write_failover_section(&app_config, "Main", Some(&old_etag), Some(section("ignored", "main-2")))
        .await
        .expect("writes")
        .expect("entry");

    assert_eq!(entry.overrides.target, "Main");
    assert_eq!(entry.overrides.groups[0].id, "main-2");
    assert_ne!(entry.etag.as_ref(), old_etag);
    let file = read_file(&failover_dir.join("a.yml"));
    let targets: Vec<&str> = file.targets.iter().map(|t| t.target.as_str()).collect();
    assert_eq!(targets, ["Main", "News"]);
    assert_eq!(file.targets[0].groups[0].id, "main-2");
    assert_eq!(file.targets[1].groups[0].id, "news-1");
    assert_eq!(read_file(&failover_dir.join("b.yml")).targets[0].groups[0].id, "kids-1");
    assert_eq!(app_config.failover_section("Main").expect("main").etag, entry.etag);
    assert_eq!(app_config.failover.load().files_used.len(), 2);
}

#[tokio::test]
async fn write_new_section_creates_target_file_in_dir_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    let source = FailoverSource::Directory(failover_dir.clone());
    let app_config = loaded_app_config(dir.path(), &source);

    let entry = write_failover_section(&app_config, "Main TV", None, Some(section("Main TV", "g1")))
        .await
        .expect("writes")
        .expect("entry");
    assert_eq!(entry.file, failover_dir.join("Main_TV.yml"));
    assert_eq!(read_file(&entry.file).targets[0].target, "Main TV");

    // a name collision with a file that defines other targets gets a numeric suffix
    write_file(&failover_dir.join("Kids.yml"), vec![section("Other", "o1")]);
    let reloaded = loaded_app_config(dir.path(), &source);
    let kids = write_failover_section(&reloaded, "Kids", None, Some(section("Kids", "k1")))
        .await
        .expect("writes")
        .expect("entry");
    assert_eq!(kids.file, failover_dir.join("Kids-2.yml"));
    assert_eq!(file_names(&failover_dir), ["Kids-2.yml", "Kids.yml", "Main_TV.yml"]);
}

#[tokio::test]
async fn write_new_section_appends_in_file_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    let source = FailoverSource::File(path.clone());
    let app_config = loaded_app_config(dir.path(), &source);

    // the file does not exist yet: it is created with version 1
    write_failover_section(&app_config, "Main", None, Some(section("Main", "g1"))).await.expect("creates");
    let created = read_file(&path);
    assert_eq!(created.version, 1);
    assert_eq!(created.targets.len(), 1);

    // an existing file gets the new section at the end
    write_failover_section(&app_config, "Kids", None, Some(section("Kids", "g2"))).await.expect("appends");
    let appended = read_file(&path);
    let targets: Vec<&str> = appended.targets.iter().map(|t| t.target.as_str()).collect();
    assert_eq!(targets, ["Main", "Kids"]);
    assert_eq!(app_config.failover.load().files_used, vec![path]);
}

#[tokio::test]
async fn write_removes_empty_section_and_deletes_empty_dir_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    write_file(&failover_dir.join("main.yml"), vec![section("Main", "g1")]);
    write_file(&failover_dir.join("shared.yml"), vec![section("Kids", "k1"), section("News", "n1")]);
    let source = FailoverSource::Directory(failover_dir.clone());
    let app_config = loaded_app_config(dir.path(), &source);

    // an empty section counts as removal; the file without sections is deleted
    let main_etag = app_config.failover_section("Main").expect("main").etag.to_string();
    let result =
        write_failover_section(&app_config, "Main", Some(&main_etag), Some(TargetFailoverOverridesDto::new("Main")))
            .await
            .expect("removes");
    assert!(result.is_none());
    assert_eq!(file_names(&failover_dir), ["shared.yml"]);
    assert!(app_config.failover_section("Main").is_none());

    // removing one of two sections keeps the file with the remaining one
    let kids_etag = app_config.failover_section("Kids").expect("kids").etag.to_string();
    write_failover_section(&app_config, "Kids", Some(&kids_etag), None).await.expect("removes");
    let remaining = read_file(&failover_dir.join("shared.yml"));
    assert_eq!(remaining.targets.len(), 1);
    assert_eq!(remaining.targets[0].target, "News");
}

#[tokio::test]
async fn write_removal_keeps_empty_targets_list_in_file_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "g1")]);
    let source = FailoverSource::File(path.clone());
    let app_config = loaded_app_config(dir.path(), &source);
    let etag = app_config.failover_section("Main").expect("main").etag.to_string();

    write_failover_section(&app_config, "Main", Some(&etag), None).await.expect("removes");

    let file = read_file(&path);
    assert_eq!(file.version, 1);
    assert!(file.targets.is_empty());
    assert!(app_config.failover.load().sections.is_empty());
}

#[tokio::test]
async fn write_rejects_stale_etag_and_unexpected_existing_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "g1")]);
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));
    let current = app_config.failover_section("Main").expect("main").etag.to_string();

    let stale =
        write_failover_section(&app_config, "Main", Some("0000000000000000"), Some(section("Main", "g2"))).await;
    assert!(matches!(stale, Err(FailoverWriteError::Conflict { current_etag: Some(ref etag) }) if *etag == current));

    let existing = write_failover_section(&app_config, "Main", None, Some(section("Main", "g2"))).await;
    assert!(matches!(existing, Err(FailoverWriteError::Conflict { current_etag: Some(ref etag) }) if *etag == current));

    let missing =
        write_failover_section(&app_config, "Kids", Some("0000000000000000"), Some(section("Kids", "g3"))).await;
    assert!(matches!(missing, Err(FailoverWriteError::Conflict { current_etag: None })));
    assert_eq!(read_file(&path).targets[0].groups[0].id, "g1");
}

#[tokio::test]
async fn write_rejects_invalid_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));

    let result = write_failover_section(&app_config, "Main", None, Some(section("Main", "Bad Id"))).await;

    match result {
        Err(FailoverWriteError::Invalid(findings)) => {
            assert_eq!(findings, vec![OverrideValidationError::InvalidManualId("Bad Id".to_string())]);
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert!(!path.exists());
}

#[tokio::test]
async fn write_detects_disk_change_not_yet_reloaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "g1")]);
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));
    let loaded_etag = app_config.failover_section("Main").expect("main").etag.to_string();

    // hand edit after load, before any reload
    write_file(&path, vec![section("Main", "hand-edit")]);

    let result = write_failover_section(&app_config, "Main", Some(&loaded_etag), Some(section("Main", "g2"))).await;

    match result {
        Err(FailoverWriteError::Conflict { current_etag: Some(current) }) => assert_ne!(current, loaded_etag),
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(read_file(&path).targets[0].groups[0].id, "hand-edit");
}

#[tokio::test]
async fn write_detects_new_section_added_on_disk_not_yet_reloaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    let app_config = loaded_app_config(dir.path(), &FailoverSource::Directory(failover_dir.clone()));

    write_file(&failover_dir.join("hand.yml"), vec![section("Main", "hand-edit")]);

    let result = write_failover_section(&app_config, "Main", None, Some(section("Main", "g1"))).await;

    assert!(matches!(result, Err(FailoverWriteError::Conflict { current_etag: Some(_) })));
    assert_eq!(file_names(&failover_dir), ["hand.yml"]);
}

#[tokio::test]
async fn write_refused_when_snapshot_has_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    let app_config = test_app_config(dir.path());
    app_config.failover.store(Arc::new(FailoverOverrides {
        source: Some(FailoverSource::File(path.clone())),
        error: Some("boom".to_string()),
        ..FailoverOverrides::default()
    }));

    let result = write_failover_section(&app_config, "Main", None, Some(section("Main", "g1"))).await;

    assert!(matches!(result, Err(FailoverWriteError::Locked(ref message)) if message == "boom"));
    assert!(!path.exists());
}

#[tokio::test]
async fn reassign_rewrites_target_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Old", "g1"), section("Other", "g2")]);
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));
    let etag = app_config.failover_section("Old").expect("old").etag.to_string();

    let entry = reassign_failover_section(&app_config, "Old", "New", &etag).await.expect("reassigns");

    assert_eq!(entry.overrides.target, "New");
    assert_eq!(entry.file, path);
    let file = read_file(&path);
    let targets: Vec<&str> = file.targets.iter().map(|t| t.target.as_str()).collect();
    assert_eq!(targets, ["New", "Other"]);
    assert!(app_config.failover_section("Old").is_none());
    assert!(app_config.failover_section("New").is_some());
}

#[tokio::test]
async fn reassign_conflicts_when_target_has_section_or_etag_is_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Old", "g1"), section("New", "g2")]);
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));
    let etag = app_config.failover_section("Old").expect("old").etag.to_string();

    let taken = reassign_failover_section(&app_config, "Old", "New", &etag).await;
    assert!(matches!(taken, Err(FailoverWriteError::Conflict { .. })));

    let stale = reassign_failover_section(&app_config, "Old", "Free", "0000000000000000").await;
    assert!(matches!(stale, Err(FailoverWriteError::Conflict { current_etag: Some(ref current) }) if *current == etag));

    let missing = reassign_failover_section(&app_config, "Nope", "Free", &etag).await;
    assert!(matches!(missing, Err(FailoverWriteError::Conflict { current_etag: None })));
    assert_eq!(read_file(&path).targets[0].target, "Old");
}

#[test]
fn empty_and_comment_only_files_are_valid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    std::fs::create_dir_all(&failover_dir).expect("create dir");
    std::fs::write(failover_dir.join("a-empty.yml"), "").expect("write");
    std::fs::write(failover_dir.join("b-blank.yml"), "  \n\n").expect("write");
    std::fs::write(failover_dir.join("c-comments.yml"), "# nothing here yet\n  # still nothing\n").expect("write");
    write_file(&failover_dir.join("d-real.yml"), vec![section("Main", "g1")]);

    let loaded = load_failover_overrides(&FailoverSource::Directory(failover_dir.clone())).expect("loads");

    assert!(loaded.error.is_none());
    assert_eq!(loaded.files_used.len(), 4);
    assert_eq!(loaded.sections.len(), 1);
    assert_eq!(loaded.sections.get("Main").expect("main").file, failover_dir.join("d-real.yml"));
}

#[tokio::test]
async fn write_into_empty_file_keeps_it_as_owner_file_in_file_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    std::fs::write(&path, "# placeholder\n").expect("write");
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));

    write_failover_section(&app_config, "Main", None, Some(section("Main", "g1"))).await.expect("writes");

    let file = read_file(&path);
    assert_eq!(file.version, 1);
    assert_eq!(file.targets[0].target, "Main");
}

#[tokio::test]
async fn env_placeholders_survive_write_back() {
    // SAFETY: the variable is only read by this test through the (absent) resolver; no other test touches FOO_FAILOVER_ENV.
    unsafe { std::env::set_var("FOO_FAILOVER_ENV", "resolved-secret") };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    let mut main = section("Main", "g1");
    main.groups[0].name = "${env:FOO_FAILOVER_ENV}".to_string();
    write_file(&path, vec![main]);
    let app_config = loaded_app_config(dir.path(), &FailoverSource::File(path.clone()));
    assert_eq!(app_config.failover_section("Main").expect("main").overrides.groups[0].name, "${env:FOO_FAILOVER_ENV}");

    write_failover_section(&app_config, "Kids", None, Some(section("Kids", "g2"))).await.expect("writes");

    let content = std::fs::read_to_string(&path).expect("read");
    assert!(content.contains("${env:FOO_FAILOVER_ENV}"), "{content}");
    assert!(!content.contains("resolved-secret"), "{content}");
}

/// Sections (target, file, etag) and the file list of a snapshot, comparable with a fresh load.
fn describe(snapshot: &FailoverOverrides) -> (Vec<(String, PathBuf, String)>, Vec<PathBuf>) {
    let mut sections: Vec<(String, PathBuf, String)> = snapshot
        .sections
        .iter()
        .map(|(target, entry)| (target.to_string(), entry.file.clone(), entry.etag.to_string()))
        .collect();
    sections.sort();
    (sections, snapshot.files_used.clone())
}

fn assert_snapshot_matches_disk(app_config: &AppConfig, source: &FailoverSource) {
    let fresh = load_failover_overrides(source).expect("loads");
    assert_eq!(describe(&app_config.failover.load()), describe(&fresh));
}

#[tokio::test]
async fn snapshot_after_writes_matches_a_fresh_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failover_dir = dir.path().join("failover.d");
    write_file(&failover_dir.join("a.yml"), vec![section("Main", "main-1"), section("News", "news-1")]);
    write_file(&failover_dir.join("b.yml"), vec![section("Kids", "kids-1")]);
    let source = FailoverSource::Directory(failover_dir.clone());
    let app_config = loaded_app_config(dir.path(), &source);

    // new section in a new file
    write_failover_section(&app_config, "Sport", None, Some(section("Sport", "s1"))).await.expect("adds");
    assert_snapshot_matches_disk(&app_config, &source);

    // edit in an existing file
    let main_etag = app_config.failover_section("Main").expect("main").etag.to_string();
    write_failover_section(&app_config, "Main", Some(&main_etag), Some(section("Main", "main-2")))
        .await
        .expect("edits");
    assert_snapshot_matches_disk(&app_config, &source);

    // removing the only section deletes the file
    let kids_etag = app_config.failover_section("Kids").expect("kids").etag.to_string();
    write_failover_section(&app_config, "Kids", Some(&kids_etag), None).await.expect("removes");
    assert!(!failover_dir.join("b.yml").exists());
    assert_snapshot_matches_disk(&app_config, &source);

    // reassign rewrites the target field in place
    let news_etag = app_config.failover_section("News").expect("news").etag.to_string();
    reassign_failover_section(&app_config, "News", "Weather", &news_etag).await.expect("reassigns");
    assert!(app_config.failover_section("News").is_none());
    assert!(app_config.failover_section("Weather").is_some());
    assert_snapshot_matches_disk(&app_config, &source);
}

#[tokio::test]
async fn reload_waits_for_a_running_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "main-1")]);
    let source = FailoverSource::File(path.clone());
    let app_config = Arc::new(loaded_app_config(dir.path(), &source));

    let mutation_lock = app_config.file_locks.write_lock_str(FAILOVER_MUTATION_LOCK).await;
    let reload = tokio::spawn({
        let app_config = Arc::clone(&app_config);
        let source = source.clone();
        async move { reload_failover_snapshot(&app_config, &source).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!reload.is_finished(), "a reload must not run while a failover write holds the mutation lock");

    // The write finished meanwhile; the reload reads the new disk state once it gets the lock.
    write_file(&path, vec![section("Main", "main-2")]);
    drop(mutation_lock);
    let (previous, next) = reload.await.expect("joins").expect("reloads");
    assert_eq!(previous.sections["Main"].overrides.groups[0].id, "main-1");
    assert_eq!(next.sections["Main"].overrides.groups[0].id, "main-2");
    assert_snapshot_matches_disk(&app_config, &source);
}

#[tokio::test]
async fn concurrent_saves_and_reloads_end_with_the_disk_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("failover.yml");
    write_file(&path, vec![section("Main", "g0")]);
    let source = FailoverSource::File(path.clone());
    let app_config = Arc::new(loaded_app_config(dir.path(), &source));

    let saver = tokio::spawn({
        let app_config = Arc::clone(&app_config);
        async move {
            for round in 1..=15 {
                let etag = app_config.failover_section("Main").map(|entry| entry.etag.to_string());
                let _ = write_failover_section(
                    &app_config,
                    "Main",
                    etag.as_deref(),
                    Some(section("Main", &format!("g{round}"))),
                )
                .await;
            }
        }
    });
    let reloader = tokio::spawn({
        let app_config = Arc::clone(&app_config);
        let source = source.clone();
        async move {
            for _ in 0..15 {
                reload_failover_snapshot(&app_config, &source).await.expect("reloads");
                tokio::task::yield_now().await;
            }
        }
    });
    saver.await.expect("saver");
    reloader.await.expect("reloader");

    assert_snapshot_matches_disk(&app_config, &source);
}
