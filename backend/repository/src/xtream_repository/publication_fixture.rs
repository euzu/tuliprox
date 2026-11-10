//! Controlled real disk publication for dependent crates' integration tests.
use super::{
    get_collection_path, persist_input_xtream_playlist_clusters_to_disk, xtream_cluster_category_collection,
    XtreamClusterPublishBatchResult, XtreamClusterQualityPolicy, XtreamClusterRefreshRequest,
};
use shared::{
    error::TuliproxError,
    model::{UpdateQualityPolicy, XtreamCluster},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, sync::oneshot};
use tuliprox_core::{
    model::{AppConfig, ConfigInput},
    utils::request::DynReader,
};

#[derive(Clone)]
pub struct TargetStagingFiles {
    pub database: PathBuf,
    pub index: PathBuf,
    pub categories: PathBuf,
    pub sidecars: [PathBuf; 3],
}

impl TargetStagingFiles {
    pub fn existing(&self) -> Vec<PathBuf> {
        [&self.database, &self.index, &self.categories]
            .into_iter()
            .chain(&self.sidecars)
            .filter(|path| path.exists())
            .cloned()
            .collect()
    }
}

pub struct TargetStagingPause {
    pub entered: oneshot::Receiver<TargetStagingFiles>,
    pub resume: mpsc::Sender<()>,
    pub written: oneshot::Receiver<Vec<PathBuf>>,
    pub cleaned: oneshot::Receiver<()>,
}

pub(super) struct TargetStagingProbe {
    entered: Option<oneshot::Sender<TargetStagingFiles>>,
    resume: mpsc::Receiver<()>,
    written: oneshot::Sender<Vec<PathBuf>>,
    pub(super) cleanup: Option<TargetStagingCleanup>,
    fail_after_write: bool,
}

pub(super) struct TargetStagingCleanup(Option<oneshot::Sender<()>>);

impl Drop for TargetStagingCleanup {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

static STAGING_PROBES: LazyLock<Mutex<HashMap<(PathBuf, XtreamCluster), TargetStagingProbe>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Stop the next cluster writer for this fixture's target after categories
/// have been written and before it writes the database and sorted index.
///
/// # Panics
/// Panics if the test has already registered a pause for this storage and cluster.
pub fn pause_target_staging(storage: &Path, cluster: XtreamCluster, fail_after_write: bool) -> TargetStagingPause {
    let (entered, started) = oneshot::channel();
    let (resume, can_write) = mpsc::channel();
    let (written, completed_write) = oneshot::channel();
    let (cleanup, cleaned) = oneshot::channel();
    let previous = STAGING_PROBES.lock().unwrap().insert(
        (storage.to_path_buf(), cluster),
        TargetStagingProbe {
            entered: Some(entered),
            resume: can_write,
            written,
            cleanup: Some(TargetStagingCleanup(Some(cleanup))),
            fail_after_write,
        },
    );
    assert!(previous.is_none(), "only one staging probe per test target and cluster");
    TargetStagingPause { entered: started, resume, written: completed_write, cleaned }
}

impl TargetStagingProbe {
    pub(super) fn take(storage: &Path, cluster: XtreamCluster) -> Option<Self> {
        STAGING_PROBES.lock().unwrap().remove(&(storage.to_path_buf(), cluster))
    }

    pub(super) fn before_database(
        &mut self,
        database: &Path,
        index: &Path,
        categories: &Path,
    ) -> Result<TargetStagingFiles, TuliproxError> {
        let files = TargetStagingFiles {
            database: database.to_path_buf(),
            index: index.to_path_buf(),
            categories: categories.to_path_buf(),
            sidecars: [
                crate::wal_path(database),
                crate::wal_temporary_path(database),
                crate::sidecar_lock_path(database),
            ],
        };
        self.entered
            .take()
            .unwrap()
            .send(files.clone())
            .map_err(|_| TuliproxError::RepositoryXtream("target staging probe receiver dropped"))?;
        self.resume.recv_timeout(Duration::from_secs(30)).map_err(|error| {
            TuliproxError::RepositoryXtream(format!("target staging probe did not resume: {error}"))
        })?;
        Ok(files)
    }

    pub(super) fn after_database(self, files: &TargetStagingFiles) -> Result<(), TuliproxError> {
        if self.fail_after_write {
            // The full tree write creates its lock but normally leaves no WAL.
            // Exercise cleanup when both attempt-owned WAL names exist as well.
            for path in &files.sidecars[..2] {
                std::fs::write(path, b"test staging sidecar").map_err(|error| {
                    TuliproxError::RepositoryXtream(format!("Failed to create staging probe sidecar: {error}"))
                })?;
            }
        }
        let _ = self.written.send(files.existing());
        if self.fail_after_write {
            return Err(TuliproxError::RepositoryXtream("injected target staging failure"));
        }
        Ok(())
    }
}

fn reader(value: serde_json::Value) -> DynReader {
    let (mut writer, reader) = tokio::io::duplex(65_536);
    tokio::spawn(async move {
        writer.write_all(value.to_string().as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
    });
    Box::pin(reader)
}

fn request(
    cluster: XtreamCluster,
    quality: XtreamClusterQualityPolicy,
    count: usize,
    generation: u32,
) -> XtreamClusterRefreshRequest {
    let category = match cluster {
        XtreamCluster::Live => 1,
        XtreamCluster::Video => 2,
        XtreamCluster::Series => 3,
    };
    let rows = (0..count).map(|i| {
        let id = generation + u32::try_from(i).unwrap();
        match cluster {
            XtreamCluster::Series=>serde_json::json!({"name":format!("series-{id}"),"series_id":id,"category_id":category}),
            _=>serde_json::json!({"name":format!("stream-{id}"),"stream_id":id,"category_id":category,"container_extension":"mp4"}),
        }
    }).collect::<Vec<_>>();
    XtreamClusterRefreshRequest {
        cluster,
        quality,
        categories: reader(serde_json::json!([{"category_id":category,"category_name":"fixture"}])),
        streams: reader(serde_json::json!(rows)),
    }
}

/// Seed actual `BTree`s, then publish a fresh ordered batch. A directory in place
/// of one active categories file causes a real publish-stage I/O failure.
/// The staged candidates still evaluate successfully before that later failure.
///
/// # Panics
/// Panics if fixture I/O or a prerequisite publication fails.
pub async fn publish(
    config: &Arc<AppConfig>,
    input: &ConfigInput,
    requested: &[XtreamCluster],
    failure: Option<XtreamCluster>,
    policy: UpdateQualityPolicy,
) -> XtreamClusterPublishBatchResult {
    let seed = requested
        .iter()
        .map(|cluster| request(*cluster, XtreamClusterQualityPolicy::Enforce { threshold: 0 }, 100, 1_000))
        .collect();
    let seeded = persist_input_xtream_playlist_clusters_to_disk(config, input, seed).await;
    assert!(seeded.errors.is_empty(), "seed publication must succeed");
    assert_eq!(seeded.completed_clusters, requested);
    if let Some(cluster) = failure {
        let storage = crate::get_input_storage_path(&input.name, &config.config.load().storage_dir).await.unwrap();
        let path = get_collection_path(&storage, xtream_cluster_category_collection(cluster));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
    }
    let requests = requested
        .iter()
        .map(|cluster| {
            let threshold = input.options.as_ref().map_or(0, |options| options.update_quality.threshold(*cluster));
            let quality = if policy == UpdateQualityPolicy::Bypass {
                XtreamClusterQualityPolicy::Bypass { configured_threshold: threshold }
            } else {
                XtreamClusterQualityPolicy::Enforce { threshold }
            };
            request(*cluster, quality, 100, 10_000)
        })
        .collect();
    persist_input_xtream_playlist_clusters_to_disk(config, input, requests).await
}
