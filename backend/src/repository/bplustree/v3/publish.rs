use super::{
    tree::{publish_database, BPlusTreeUpdate},
    wal::{
        invalidate_sorted_index, recover_pending_under_existing_lock, sync_parent_directory, wal_path,
        wal_temporary_path, ExclusiveSidecarGuard,
    },
};
use crate::repository::{
    bplustree::common::{ensure_distinct_sidecar_lock_domains, sidecar_lock_path},
    storage::get_file_path_for_db_index,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Eq, PartialEq)]
struct BPlusTreeArtifactPaths {
    database: PathBuf,
    index: PathBuf,
    wal: PathBuf,
    wal_temporary: PathBuf,
    sidecar_lock: PathBuf,
}

impl BPlusTreeArtifactPaths {
    fn for_database(database: &Path) -> Self {
        Self {
            database: database.to_path_buf(),
            index: get_file_path_for_db_index(database),
            wal: wal_path(database),
            wal_temporary: wal_temporary_path(database),
            sidecar_lock: sidecar_lock_path(database),
        }
    }

    fn remove_all(&self) -> io::Result<()> {
        let mut first_error = None;
        for path in [&self.database, &self.index, &self.wal, &self.wal_temporary, &self.sidecar_lock] {
            if let Err(error) = remove_file_if_exists(path) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BPlusTreeStagingArtifacts(BPlusTreeArtifactPaths);

impl BPlusTreeStagingArtifacts {
    pub(crate) fn new(published: &Path, staging: &Path) -> io::Result<Self> {
        ensure_distinct_sidecar_lock_domains(published, staging)?;
        Ok(Self(BPlusTreeArtifactPaths::for_database(staging)))
    }

    pub(crate) fn remove_owned_staging_artifacts(&self) -> io::Result<()> { self.0.remove_all() }

    #[cfg(test)]
    pub(crate) fn owned_paths(&self) -> [&Path; 5] {
        [
            &self.0.database,
            &self.0.index,
            &self.0.wal,
            &self.0.wal_temporary,
            &self.0.sidecar_lock,
        ]
    }
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("failed to remove B+Tree staging artifact {}: {error}", path.display()),
        )),
    }
}

fn same_parent_directory(left: &Path, right: &Path) -> bool {
    let left_parent = left
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let right_parent = right
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    left_parent == right_parent
}

fn publish_staged_database_inner<K, V>(
    staging: &Path,
    published: &Path,
    acquire_final_lock: impl FnOnce(&Path) -> io::Result<ExclusiveSidecarGuard>,
) -> io::Result<()>
where
    K: Ord + Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned,
{
    let staging_tree = BPlusTreeUpdate::<K, V>::try_new(staging).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to verify staging B+Tree {} before publish: {error}", staging.display()),
        )
    })?;
    drop(staging_tree);

    let final_guard = acquire_final_lock(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to acquire published B+Tree sidecar lock for {}: {error}", published.display()),
        )
    })?;
    recover_pending_under_existing_lock(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to recover published B+Tree {} before replacement: {error}", published.display()),
        )
    })?;
    publish_database(staging, published, sync_parent_directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to atomically publish staging B+Tree {} as {}: {error}",
                staging.display(),
                published.display()
            ),
        )
    })?;
    invalidate_sorted_index(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "B+Tree {} may already be published, but its previous sorted index could not be invalidated: {error}",
                published.display()
            ),
        )
    })?;
    sync_parent_directory(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "B+Tree {} may already be published, but the parent directory could not be synchronized: {error}",
                published.display()
            ),
        )
    })?;
    drop(final_guard);
    Ok(())
}

fn publish_staged_database_with_lock_acquirer<K, V>(
    staging: &Path,
    published: &Path,
    acquire_final_lock: impl FnOnce(&Path) -> io::Result<ExclusiveSidecarGuard>,
) -> io::Result<()>
where
    K: Ord + Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned,
{
    let staging_artifacts = BPlusTreeStagingArtifacts::new(published, staging)?;
    if !same_parent_directory(staging, published) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staging database {} and published database {} must share one parent directory",
                staging.display(),
                published.display()
            ),
        ));
    }
    let publish_result =
        publish_staged_database_inner::<K, V>(staging, published, acquire_final_lock);
    let cleanup_result = staging_artifacts.remove_owned_staging_artifacts();
    match (publish_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(cleanup_error)) => Err(io::Error::new(
            cleanup_error.kind(),
            format!(
                "B+Tree {} was published, but staging cleanup failed: {cleanup_error}",
                published.display()
            ),
        )),
        (Err(publish_error), Ok(())) => Err(publish_error),
        (Err(publish_error), Err(cleanup_error)) => Err(io::Error::new(
            publish_error.kind(),
            format!("{publish_error}; staging cleanup also failed: {cleanup_error}"),
        )),
    }
}

pub(crate) fn publish_staged_database<K, V>(staging: &Path, published: &Path) -> io::Result<()>
where
    K: Ord + Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned,
{
    publish_staged_database_with_lock_acquirer::<K, V>(
        staging,
        published,
        ExclusiveSidecarGuard::acquire,
    )
}

#[cfg(test)]
fn publish_staged_database_after_observed_final_lock_contention<K, V>(
    staging: &Path,
    published: &Path,
    on_contention: impl FnOnce() -> io::Result<()>,
) -> io::Result<()>
where
    K: Ord + Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned,
{
    publish_staged_database_with_lock_acquirer::<K, V>(staging, published, |database| {
        ExclusiveSidecarGuard::acquire_after_observed_contention(database, on_contention)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        publish_staged_database, publish_staged_database_after_observed_final_lock_contention,
        BPlusTreeArtifactPaths, BPlusTreeStagingArtifacts,
    };
    use super::super::wal::{leave_uncommitted_test_wal_after_database_write, wal_path};
    use crate::repository::{get_file_path_for_db_index, BPlusTree, BPlusTreeError, BPlusTreeQuery};
    use std::{
        env, fs, io,
        process::{Child, Command, ExitStatus, Stdio},
        thread,
        time::{Duration, Instant},
    };

    struct KillChildOnDrop(Option<Child>);

    impl KillChildOnDrop {
        fn spawn(command: &mut Command) -> io::Result<Self> { command.spawn().map(|child| Self(Some(child))) }

        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            self.0.as_mut().map_or(Ok(None), Child::try_wait)
        }

        fn wait_until(&mut self, deadline: Instant) -> io::Result<ExitStatus> {
            loop {
                if let Some(status) = self.try_wait()? {
                    self.0 = None;
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "publish child did not finish"));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for KillChildOnDrop {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }

    fn wait_for_path(path: &std::path::Path, deadline: Instant) -> io::Result<()> {
        while !path.exists() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "publish child did not reach lock boundary"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    fn publish_staged_database_child() -> io::Result<()> {
        let Some(staging) = env::var_os("TULIPROX_BPLUS_PUBLISH_STAGING") else {
            return Ok(());
        };
        let published = env::var_os("TULIPROX_BPLUS_PUBLISH_FINAL")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing published path"))?;
        let marker = env::var_os("TULIPROX_BPLUS_PUBLISH_MARKER")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing marker path"))?;
        publish_staged_database_after_observed_final_lock_contention::<u32, String>(
            std::path::Path::new(&staging),
            std::path::Path::new(&published),
            || fs::write(marker, b"contended"),
        )
    }

    #[test]
    fn publish_waits_for_final_reader_then_replaces_database_and_cleans_staging() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let published = directory.path().join("live.db");
        let staging = directory.path().join("live.refresh-fixed.db");
        let marker = directory.path().join("publish.final-lock-contended");

        let mut final_tree = BPlusTree::new();
        final_tree.insert(1u32, String::from("published"));
        final_tree.store(&published)?;
        fs::write(get_file_path_for_db_index(&published), b"stale index")?;

        let mut staged_tree = BPlusTree::new();
        staged_tree.insert(2u32, String::from("staged"));
        staged_tree.store(&staging)?;
        let staging_artifacts = BPlusTreeArtifactPaths::for_database(&staging);
        fs::write(&staging_artifacts.wal_temporary, b"abandoned")?;
        fs::write(&staging_artifacts.index, b"staging index")?;

        let mut final_query = BPlusTreeQuery::<u32, String>::try_new(&published)?;
        let published_artifacts = BPlusTreeArtifactPaths::for_database(&published);
        fs::write(&published_artifacts.wal_temporary, b"abandoned final WAL staging file")?;
        let mut child = KillChildOnDrop::spawn(
            Command::new(env::current_exe()?)
                .arg("--exact")
                .arg("repository::bplustree::v3::publish::tests::publish_staged_database_child")
                .arg("--nocapture")
                .env("TULIPROX_BPLUS_PUBLISH_STAGING", &staging)
                .env("TULIPROX_BPLUS_PUBLISH_FINAL", &published)
                .env("TULIPROX_BPLUS_PUBLISH_MARKER", &marker)
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )?;
        wait_for_path(&marker, Instant::now() + Duration::from_secs(5))?;
        assert_eq!(
            final_query.query(&1).map_err(BPlusTreeError::to_io)?,
            Some(String::from("published"))
        );
        assert!(staging.exists(), "staging database must exist while the final lock is contended");
        assert!(child.try_wait()?.is_none(), "publish must wait for the final shared sidecar lock");

        drop(final_query);
        let status = child.wait_until(Instant::now() + Duration::from_secs(5))?;
        assert!(status.success(), "publish child failed with {status}");

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&published)?;
        assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
        assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(String::from("staged")));
        assert!(!get_file_path_for_db_index(&published).exists());
        assert!(!published_artifacts.wal_temporary.exists());
        for artifact in [
            staging_artifacts.database,
            staging_artifacts.index,
            staging_artifacts.wal,
            staging_artifacts.wal_temporary,
            staging_artifacts.sidecar_lock,
        ] {
            assert!(!artifact.exists(), "staging artifact survived: {}", artifact.display());
        }
        Ok(())
    }

    #[test]
    fn publish_recovers_active_final_wal_before_replacing_database() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let published = directory.path().join("live.db");
        let staging = directory.path().join("live.refresh-wal.db");

        let mut published_tree = BPlusTree::new();
        published_tree.insert(1u32, String::from("published"));
        published_tree.store(&published)?;
        let mut staging_tree = BPlusTree::new();
        staging_tree.insert(2u32, String::from("staged"));
        staging_tree.store(&staging)?;

        leave_uncommitted_test_wal_after_database_write(&published)?;
        assert!(wal_path(&published).exists(), "test setup must leave an active final WAL");

        publish_staged_database::<u32, String>(&staging, &published)?;

        assert!(!wal_path(&published).exists(), "active final WAL must be recovered before publish");
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&published)?;
        assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
        assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(String::from("staged")));
        Ok(())
    }

    #[test]
    fn sequential_staged_publications_leave_only_the_final_lock_domain() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let published = directory.path().join("live.db");
        let first_staging = directory.path().join("live.refresh-first.db");
        let second_staging = directory.path().join("live.refresh-second.db");

        let mut published_tree = BPlusTree::new();
        published_tree.insert(1u32, String::from("published"));
        published_tree.store(&published)?;
        let mut first_tree = BPlusTree::new();
        first_tree.insert(2u32, String::from("first"));
        first_tree.store(&first_staging)?;
        publish_staged_database::<u32, String>(&first_staging, &published)?;

        let mut first_query = BPlusTreeQuery::<u32, String>::try_new(&published)?;
        assert_eq!(
            first_query.query(&2).map_err(BPlusTreeError::to_io)?,
            Some(String::from("first"))
        );
        drop(first_query);

        let mut second_tree = BPlusTree::new();
        second_tree.insert(3u32, String::from("second"));
        second_tree.store(&second_staging)?;
        publish_staged_database::<u32, String>(&second_staging, &published)?;

        let mut second_query = BPlusTreeQuery::<u32, String>::try_new(&published)?;
        assert_eq!(second_query.query(&2).map_err(BPlusTreeError::to_io)?, None);
        assert_eq!(
            second_query.query(&3).map_err(BPlusTreeError::to_io)?,
            Some(String::from("second"))
        );
        assert!(BPlusTreeArtifactPaths::for_database(&published).sidecar_lock.exists());
        assert!(!BPlusTreeArtifactPaths::for_database(&first_staging).sidecar_lock.exists());
        assert!(!BPlusTreeArtifactPaths::for_database(&second_staging).sidecar_lock.exists());
        Ok(())
    }

    #[test]
    fn colliding_publish_path_fails_without_cleaning_published_artifacts() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let published = directory.path().join("live.db");
        let colliding_staging = directory.path().join("live.tmp");
        let published_artifacts = BPlusTreeArtifactPaths::for_database(&published);

        let mut published_tree = BPlusTree::new();
        published_tree.insert(1u32, String::from("published"));
        published_tree.store(&published)?;
        fs::write(&published_artifacts.index, b"published index")?;
        fs::write(&colliding_staging, b"unverified staging")?;

        let error = publish_staged_database::<u32, String>(&colliding_staging, &published)
            .expect_err("colliding sidecar domains must fail before cleanup");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(published.exists());
        assert!(published_artifacts.index.exists());
        assert!(published_artifacts.sidecar_lock.exists());
        assert!(colliding_staging.exists());
        Ok(())
    }

    #[test]
    fn staging_artifacts_reject_a_published_lock_domain_alias() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let published = directory.path().join("live.db");
        let colliding_staging = directory.path().join("live.tmp");

        let error = BPlusTreeStagingArtifacts::new(&published, &colliding_staging)
            .expect_err("staging artifacts must not represent the published lock domain");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        Ok(())
    }
}
