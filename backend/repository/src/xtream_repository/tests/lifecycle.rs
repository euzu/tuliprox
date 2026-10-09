use super::{fixed_refresh_paths, merge_preserved_stream_properties, XtreamRefreshLease};
use crate::{bplustree::sidecar_lock_path, cleanup_orphaned_staging_artifacts, refresh_generation_guard_path};
use shared::model::{SeriesStreamProperties, StreamProperties};
use std::{fs, time::Duration};
use tempfile::tempdir;

#[test]
fn merge_preserves_missing_series_tmdb_and_release_date() {
    let mut new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        tmdb: None,
        release_date: None,
        ..SeriesStreamProperties::default()
    }));
    let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        tmdb: Some(12345),
        release_date: Some("2015-01-01".into()),
        ..SeriesStreamProperties::default()
    }));

    let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
    assert!(changed);
    match new_props {
        StreamProperties::Series(series) => {
            assert_eq!(series.tmdb, Some(12345));
            assert_eq!(series.release_date.as_deref(), Some("2015-01-01"));
        }
        _ => panic!("expected series properties"),
    }
}

#[test]
fn refresh_lease_cleanup_is_idempotent_and_generation_local() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 8);
    let other_paths = fixed_refresh_paths(dir.path(), 9);
    let lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");

    fs::write(&paths.published_database, b"published").expect("published fixture should be written");
    let published_lock = sidecar_lock_path(&paths.published_database);
    fs::write(&published_lock, b"").expect("published lock fixture should be written");
    for artifact in lease.0.database_artifacts.owned_paths() {
        fs::write(artifact, b"staging").expect("staging artifact should be written");
    }
    fs::write(&paths.staging_categories, b"staging").expect("staging category should be written");
    fs::write(&other_paths.staging_database, b"other generation").expect("other generation fixture should be written");

    lease.cleanup_staging_artifacts().expect("first cleanup should succeed");
    lease.cleanup_staging_artifacts().expect("second cleanup should be idempotent");

    assert!(paths.published_database.exists());
    assert!(published_lock.exists());
    assert!(other_paths.staging_database.exists());
    assert!(!paths.staging_database.exists());
    assert!(!paths.staging_categories.exists());
}

#[test]
fn refresh_lease_defers_cleanup_until_last_worker_clone_drops() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 13);
    let guard_path = refresh_generation_guard_path(dir.path(), paths.generation);
    let parent_lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");
    let worker_lease = parent_lease.clone();
    fs::write(&paths.staging_database, b"staging").expect("staging fixture should be written");
    fs::write(&paths.staging_categories, b"categories").expect("category fixture should be written");

    drop(parent_lease);
    assert!(paths.staging_database.exists());
    assert!(paths.staging_categories.exists());
    assert!(guard_path.exists());

    drop(worker_lease);
    assert!(!paths.staging_database.exists());
    assert!(!paths.staging_categories.exists());
    assert!(!guard_path.exists());
}

#[test]
fn active_refresh_between_btree_batches_survives_orphan_cleanup() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 14);
    let guard_path = refresh_generation_guard_path(dir.path(), paths.generation);
    let lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");
    let sidecar = sidecar_lock_path(&paths.staging_database);
    fs::write(&paths.staging_database, b"staging").expect("staging fixture should be written");
    fs::write(&paths.staging_categories, b"categories").expect("category fixture should be written");
    fs::write(&sidecar, b"").expect("sidecar fixture should be written");

    let between_batch_probe =
        fs::OpenOptions::new().read(true).write(true).open(&sidecar).expect("open staging sidecar");
    between_batch_probe.try_lock().expect("staging sidecar should be unlocked between batches");
    between_batch_probe.unlock().expect("release between-batch probe");
    drop(between_batch_probe);

    cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);

    assert!(paths.staging_database.exists());
    assert!(paths.staging_categories.exists());
    assert!(sidecar.exists());
    assert!(guard_path.exists());

    drop(lease);
    assert!(!paths.staging_database.exists());
    assert!(!paths.staging_categories.exists());
    assert!(!sidecar.exists());
    assert!(!guard_path.exists());
}
