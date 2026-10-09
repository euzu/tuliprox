use super::{
    preserve_details_input_xtream_playlist_cluster_to_disk, publish_staged_file_same_directory,
    save_xtream_categories_to_file, xtream_get_file_path, PreserveDetailsOutcome, XtreamClusterPublishBatchResult,
    XtreamClusterPublishOutcome, XtreamClusterQualityPolicy, XtreamClusterRefreshRequest, XtreamRefreshPaths,
    BATCH_SIZE,
};
use crate::{
    bplustree::{
        publish_staged_database, BPlusTree, BPlusTreeError, BPlusTreeQuery, BPlusTreeStagingArtifacts, BPlusTreeUpdate,
        FlushPolicy,
    },
    storage::{ensure_input_storage_path, XtreamRefreshGenerationGuard},
};
use log::error;
use shared::{
    error::TuliproxError,
    model::{xtream_const::XTREAM_CLUSTER, XtreamCluster, XtreamPlaylistItem},
};
use std::{
    collections::HashSet,
    io,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{
        evaluate_update_quality, AppConfig, ClusterForceUpdate, ClusterUpdateAcceptance, ClusterUpdateRejection,
        ConfigInput, ConfigInputFlags, UpdateQualityDecision,
    },
    utils::{remove_file_if_exists, request::DynReader, FileWriteGuard},
};
use tuliprox_parser::xtream;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct XtreamClusterEvaluationReport {
    pub(super) quality_acceptance: Option<ClusterUpdateAcceptance>,
    pub(super) quality_rejection: Option<ClusterUpdateRejection>,
    pub(super) force_update: Option<ClusterForceUpdate>,
}

#[derive(Debug, Clone)]
pub(super) struct XtreamRefreshLease(pub(super) Arc<XtreamRefreshLeaseInner>);

#[derive(Debug)]
pub(super) struct XtreamRefreshLeaseInner {
    paths: XtreamRefreshPaths,
    pub(super) database_artifacts: BPlusTreeStagingArtifacts,
    _generation_guard: XtreamRefreshGenerationGuard,
}

impl XtreamRefreshLease {
    pub(super) fn new(paths: XtreamRefreshPaths) -> Result<Self, TuliproxError> {
        let database_artifacts = BPlusTreeStagingArtifacts::new(&paths.published_database, &paths.staging_database)
            .map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Invalid Xtream staging artifacts for generation {}: {error}",
                    paths.generation
                ))
            })?;
        let storage_path = paths.staging_database.parent().ok_or_else(|| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream staging database has no storage directory: {}",
                paths.staging_database.display()
            ))
        })?;
        let generation_guard =
            XtreamRefreshGenerationGuard::acquire(storage_path, paths.generation).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to acquire Xtream refresh generation guard {} in {}: {error}",
                    paths.generation,
                    storage_path.display()
                ))
            })?;
        Ok(Self(Arc::new(XtreamRefreshLeaseInner { paths, database_artifacts, _generation_guard: generation_guard })))
    }

    pub(super) fn paths(&self) -> &XtreamRefreshPaths { &self.0.paths }

    pub(super) fn cleanup_staging_artifacts(&self) -> io::Result<()> {
        let database_result = self.0.database_artifacts.remove_owned_staging_artifacts();
        let categories_result = remove_file_if_exists(&self.0.paths.staging_categories);
        match (database_result, categories_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(database_error), Err(categories_error)) => Err(io::Error::new(
                database_error.kind(),
                format!("{database_error}; category staging cleanup also failed: {categories_error}"),
            )),
        }
    }
}

impl Drop for XtreamRefreshLeaseInner {
    fn drop(&mut self) {
        let database_result = self.database_artifacts.remove_owned_staging_artifacts();
        let categories_result = remove_file_if_exists(&self.paths.staging_categories);
        if let Err(error) = database_result {
            log::warn!(
                "Failed to clean Xtream staging database artifacts for generation {}: {error}",
                self.paths.generation
            );
        }
        if let Err(error) = categories_result {
            log::warn!(
                "Failed to clean Xtream staging categories for generation {} at {}: {error}",
                self.paths.generation,
                self.paths.staging_categories.display()
            );
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct XtreamClusterStageOperations {
    pub(super) preserve_details: fn(&Path, &Path) -> Result<PreserveDetailsOutcome, TuliproxError>,
}

impl Default for XtreamClusterStageOperations {
    fn default() -> Self { Self { preserve_details: preserve_details_input_xtream_playlist_cluster_to_disk } }
}

struct StagedXtreamClusterRefresh {
    refresh_lease: XtreamRefreshLease,
    publish_lock: Arc<FileWriteGuard>,
    storage_path: PathBuf,
    input_name: Arc<str>,
    cluster: XtreamCluster,
    raw_groups: Vec<String>,
    item_count: usize,
    evaluation: XtreamClusterEvaluationReport,
}

enum StagedXtreamClusterOutcome {
    Ready(StagedXtreamClusterRefresh),
    RetainedPrevious(ClusterUpdateRejection),
    Failed { evaluation: XtreamClusterEvaluationReport, error: TuliproxError },
}

impl StagedXtreamClusterOutcome {
    pub(super) const fn evaluation(&self) -> XtreamClusterEvaluationReport {
        match self {
            Self::Ready(refresh) => refresh.evaluation,
            Self::RetainedPrevious(rejection) => XtreamClusterEvaluationReport {
                quality_acceptance: None,
                quality_rejection: Some(*rejection),
                force_update: None,
            },
            Self::Failed { evaluation, .. } => *evaluation,
        }
    }
}

pub(super) fn count_xtream_tree_entries(path: &Path) -> io::Result<Option<usize>> {
    let mut query = match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(path) {
        Ok(query) => query,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    query.len().map(Some).map_err(BPlusTreeError::to_io)
}

pub(super) fn evaluate_staged_xtream_cluster_quality(
    paths: &XtreamRefreshPaths,
    cluster: XtreamCluster,
    threshold: u8,
) -> Result<UpdateQualityDecision, TuliproxError> {
    if threshold == 0 {
        return Ok(UpdateQualityDecision::Disabled);
    }

    let candidate_count = count_xtream_tree_entries(&paths.staging_database)
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to count staging Xtream tree {} for {cluster} quality evaluation: {error}",
                paths.staging_database.display()
            ))
        })?
        .ok_or_else(|| {
            TuliproxError::RepositoryXtream(format!(
                "Staging Xtream tree {} disappeared before {cluster} quality evaluation",
                paths.staging_database.display()
            ))
        })?;
    let current_count = count_xtream_tree_entries(&paths.published_database).map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Failed to count published Xtream tree {} for {cluster} quality evaluation: {error}",
            paths.published_database.display()
        ))
    })?;

    Ok(evaluate_update_quality(current_count, candidate_count, threshold))
}

#[allow(clippy::too_many_lines)]
async fn stage_input_xtream_playlist_cluster_to_disk(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    request: XtreamClusterRefreshRequest,
    operations: XtreamClusterStageOperations,
) -> Result<StagedXtreamClusterOutcome, TuliproxError> {
    let XtreamClusterRefreshRequest { cluster, quality: quality_policy, categories, streams } = request;
    let cfg = app_config.config.load();
    let storage_path = ensure_input_storage_path(&cfg, &input.name).await?;
    drop(cfg);
    let refresh_lease = XtreamRefreshLease::new(XtreamRefreshPaths::new(&storage_path, cluster)?)?;

    // Channel for transferring items from Parser (Async Task) to Consumer (Blocking Task)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<XtreamPlaylistItem>(BATCH_SIZE * 2);
    let input_clone = input.clone();

    // 1. Parser Task: Runs the async parsing logic
    // We move the readers into this task.
    let parse_task = tokio::spawn(async move {
        let tx_for_closure = tx.clone();
        let res = xtream::parse_xtream_streaming(&input_clone, cluster, categories, streams, move |item| {
            // Copy needed data before moving the item into the channel.
            let item_id = item.virtual_id;

            // We use blocking_send because the closure provided by the parser library is synchronous.
            // This is safe here because it runs within its own tokio::spawn task.
            if let Err(e) = tx_for_closure.blocking_send(item) {
                error!("Channel closed while processing {cluster} for item {item_id}: {e}");
                return Err(TuliproxError::RepositoryXtream(format!("Channel closed while processing {cluster}")));
            }
            Ok(())
        })
        .await;

        // CRITICAL: Explicitly drop the sender to signal rx.blocking_recv() to stop.
        // This prevents the consumer from waiting forever if the parser fails.
        drop(tx);
        res
    });

    // 2. Consumer Task: Handles heavy Disk I/O (BPlusTree updates)
    let consumer_lease = refresh_lease.clone();
    let consumer_task = tokio::task::spawn_blocking(move || {
        let staging_path = &consumer_lease.paths().staging_database;

        BPlusTree::<u32, XtreamPlaylistItem>::new().store(staging_path).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to initialize staging Xtream tree {} for {cluster}: {error}",
                staging_path.display()
            ))
        })?;

        let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new_with_backoff(staging_path)
            .map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to open staging Xtream tree {} for {cluster}: {error}",
                    staging_path.display()
                ))
            })?;
        tree.set_flush_policy(FlushPolicy::Batch);

        let mut buffer = Vec::with_capacity(BATCH_SIZE);
        let mut seen_groups: HashSet<String> = HashSet::new();
        let mut item_count = 0_usize;

        // This loop exits when all 'tx' clones are dropped (signaling end of stream)
        while let Some(item) = rx.blocking_recv() {
            item_count = item_count.saturating_add(1);
            if !seen_groups.contains(item.group.as_ref()) {
                seen_groups.insert(item.group.to_string());
            }
            buffer.push(item);
            if buffer.len() >= BATCH_SIZE {
                let batch: Vec<(&u32, &XtreamPlaylistItem)> = buffer.iter().map(|i| (&i.provider_id, i)).collect();
                let prepared =
                    BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch).map_err(|error| {
                        TuliproxError::RepositoryXtream(format!(
                            "Failed to prepare staging batch for {cluster} at {}: {error}",
                            staging_path.display()
                        ))
                    })?;
                tree.upsert_batch_encoded(prepared).map_err(|error| {
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to write staging batch for {cluster} at {}: {error}",
                        staging_path.display()
                    ))
                })?;
                // Commit per batch so the write transaction's dirty-page map stays bounded.
                // Holding it open for the whole cluster buffered ~48k pages (196 MB); the
                // import writes into a .tmp file that is renamed on success, so atomicity
                // comes from the rename, not from a single transaction.
                tree.commit().map_err(|e| {
                    error!("Batch commit failed for cluster {cluster} at {}: {e}", staging_path.display());
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to commit staging batch for {cluster} at {}: {e}",
                        staging_path.display()
                    ))
                })?;
                buffer.clear();
            }
        }

        // Final batch processing
        if !buffer.is_empty() {
            let batch: Vec<(&u32, &XtreamPlaylistItem)> = buffer.iter().map(|i| (&i.provider_id, i)).collect();
            let prepared =
                BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch).map_err(|error| {
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to prepare final staging batch for {cluster} at {}: {error}",
                        staging_path.display()
                    ))
                })?;
            tree.upsert_batch_encoded(prepared).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to write final staging batch for {cluster} at {}: {error}",
                    staging_path.display()
                ))
            })?;
        }

        tree.commit().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to commit staging Xtream tree for {cluster} at {}: {error}",
                staging_path.display()
            ))
        })?;
        Ok::<(Vec<String>, usize), TuliproxError>((seen_groups.into_iter().collect(), item_count))
    });

    // 3. Robust Joining of both tasks
    // try_join! returns immediately if any task returns an error or panics.
    let (parse_res, consumer_res) = tokio::try_join!(parse_task, consumer_task).map_err(|e| {
        TuliproxError::RepositoryXtream(format!("Task join error during cluster {cluster} update: {e}"))
    })?;

    // Handle internal errors from the tasks
    let parsed_categories = parse_res?;
    let (raw_groups, item_count) = consumer_res?;

    save_xtream_categories_to_file(refresh_lease.clone(), &parsed_categories).await?;

    // Lock order for the publish phase is always FileLockManager(final) followed by B+Tree sidecars. No B+Tree
    // handle escapes its blocking closure, so none is held while this async lock is acquired.
    let publish_lock = Arc::new(app_config.file_locks.write_lock(&refresh_lease.paths().published_database).await);

    let quality_lease = refresh_lease.clone();
    let quality_lock = Arc::clone(&publish_lock);
    let (evaluation, rejection_cleanup_error) = tokio::task::spawn_blocking(move || {
        let _publish_guard = quality_lock;
        let evaluation = match quality_policy {
            XtreamClusterQualityPolicy::Enforce { threshold } => {
                let decision = evaluate_staged_xtream_cluster_quality(quality_lease.paths(), cluster, threshold)?;
                XtreamClusterEvaluationReport {
                    quality_acceptance: decision.acceptance(cluster),
                    quality_rejection: decision.rejection(cluster),
                    force_update: None,
                }
            }
            XtreamClusterQualityPolicy::Bypass { configured_threshold } => {
                let candidate_count = count_xtream_tree_entries(&quality_lease.paths().staging_database)
                    .map_err(|error| {
                        TuliproxError::RepositoryXtream(format!(
                            "Failed to count staging Xtream tree {} for forced {cluster} publication: {error}",
                            quality_lease.paths().staging_database.display()
                        ))
                    })?
                    .ok_or_else(|| {
                        TuliproxError::RepositoryXtream(format!(
                            "Staging Xtream tree {} disappeared before forced {cluster} publication",
                            quality_lease.paths().staging_database.display()
                        ))
                    })?;
                XtreamClusterEvaluationReport {
                    quality_acceptance: None,
                    quality_rejection: None,
                    force_update: Some(ClusterForceUpdate { cluster, candidate_count, configured_threshold }),
                }
            }
        };
        let rejection_cleanup_error = if evaluation.quality_rejection.is_some() {
            quality_lease.cleanup_staging_artifacts().err().map(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to clean rejected Xtream staging artifacts for {cluster}: {error}"
                ))
            })
        } else {
            None
        };
        Ok::<_, TuliproxError>((evaluation, rejection_cleanup_error))
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Quality-evaluation task failed to join during {cluster} refresh: {error}"
        ))
    })??;

    if let Some(error) = rejection_cleanup_error {
        return Ok(StagedXtreamClusterOutcome::Failed { evaluation, error });
    }

    if let Some(rejection) = evaluation.quality_rejection {
        drop(publish_lock);
        log::debug!(
            "Xtream cluster candidate rejected; retained active cluster: cluster={cluster} generation={} rejection={rejection:?}",
            refresh_lease.paths().generation
        );
        return Ok(StagedXtreamClusterOutcome::RetainedPrevious(rejection));
    }

    let merge_lease = refresh_lease.clone();
    let merge_lock = Arc::clone(&publish_lock);
    let preserve_details = operations.preserve_details;
    let merge_result = tokio::task::spawn_blocking(move || {
        let _publish_guard = merge_lock;
        preserve_details(&merge_lease.paths().published_database, &merge_lease.paths().staging_database)
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Detail-preservation task failed to join during {cluster} refresh: {error}"
        ))
    });
    let merge_outcome = match merge_result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) | Err(error) => return Ok(StagedXtreamClusterOutcome::Failed { evaluation, error }),
    };
    log::debug!(
        "Xtream cluster detail preservation completed: cluster={cluster} generation={} outcome={merge_outcome:?}",
        refresh_lease.paths().generation
    );

    let compact_lease = refresh_lease.clone();
    let compact_lock = Arc::clone(&publish_lock);
    let compact_result = tokio::task::spawn_blocking(move || {
        let _publish_guard = compact_lock;
        let staging_path = &compact_lease.paths().staging_database;
        let mut tree =
            BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(staging_path).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to open staging Xtream tree {} for compaction: {error}",
                    staging_path.display()
                ))
            })?;
        tree.compact().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to compact staging Xtream tree {}: {error}",
                staging_path.display()
            ))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!("Compaction task failed to join during {cluster} refresh: {error}"))
    });
    match compact_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) | Err(error) => return Ok(StagedXtreamClusterOutcome::Failed { evaluation, error }),
    }

    Ok(StagedXtreamClusterOutcome::Ready(StagedXtreamClusterRefresh {
        refresh_lease,
        publish_lock,
        storage_path,
        input_name: Arc::clone(&input.name),
        cluster,
        raw_groups,
        item_count,
        evaluation,
    }))
}

async fn publish_staged_xtream_cluster(
    app_config: &AppConfig,
    staged: StagedXtreamClusterRefresh,
) -> Result<(), TuliproxError> {
    let StagedXtreamClusterRefresh {
        refresh_lease,
        publish_lock,
        storage_path,
        input_name,
        cluster,
        raw_groups,
        item_count: _,
        evaluation: _,
    } = staged;

    let database_publish_lease = refresh_lease.clone();
    let database_publish_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = database_publish_lock;
        publish_staged_database::<u32, XtreamPlaylistItem>(
            &database_publish_lease.paths().staging_database,
            &database_publish_lease.paths().published_database,
        )
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!("Failed to publish staging Xtream database for {cluster}: {error}"))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!("Database publish task failed to join for {cluster}: {error}"))
    })??;

    let category_publish_lease = refresh_lease.clone();
    let category_publish_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = category_publish_lock;
        publish_staged_file_same_directory(
            &category_publish_lease.paths().staging_categories,
            &category_publish_lease.paths().published_categories,
        )
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream database for {cluster} was published, but category publication failed: {error}"
            ))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Xtream database for {cluster} was published, but the category publish task failed to join: {error}"
        ))
    })??;

    if let Err(publish_err) =
        crate::publish_raw_group_catalog(&storage_path, &input_name, cluster, raw_groups, &app_config.file_locks).await
    {
        log::warn!(
            "Xtream data for input '{input_name}' cluster {cluster} was published, but its raw group catalog could not be published: {publish_err}"
        );
    }

    let cleanup_lease = refresh_lease.clone();
    let cleanup_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = cleanup_lock;
        cleanup_lease.cleanup_staging_artifacts()
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Xtream refresh for {cluster} was published, but cleanup task failed to join: {error}"
        ))
    })?
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Xtream refresh for {cluster} was published, but staging cleanup failed: {error}"
        ))
    })?;

    drop(publish_lock);
    log::debug!(
        "Xtream cluster updated successfully: cluster={cluster} generation={}",
        refresh_lease.paths().generation
    );
    Ok(())
}

fn input_cluster_enabled(input: &ConfigInput, cluster: XtreamCluster) -> bool {
    match cluster {
        XtreamCluster::Live => !input.has_flag(ConfigInputFlags::SkipLive),
        XtreamCluster::Video => !input.has_flag(ConfigInputFlags::SkipVod),
        XtreamCluster::Series => !input.has_flag(ConfigInputFlags::SkipSeries),
    }
}

async fn published_xtream_cluster_has_items(
    app_config: &AppConfig,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<bool, TuliproxError> {
    let path = xtream_get_file_path(storage_path, cluster);
    let lock = app_config.file_locks.read_lock(&path).await;
    tokio::task::spawn_blocking(move || {
        let _guard = lock;
        match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&path) {
            Ok(mut query) => query.iter().next().transpose().map(|entry| entry.is_some()).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to inspect published Xtream cluster {cluster} at {}: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(TuliproxError::RepositoryXtream(format!(
                "Failed to open published Xtream cluster {cluster} at {}: {error}",
                path.display()
            ))),
        }
    })
    .await
    .map_err(|error| TuliproxError::Task(format!("Failed to join Xtream cluster inspection: {error}")))?
}

pub async fn persist_input_xtream_playlist_clusters_to_disk(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster_readers: Vec<XtreamClusterRefreshRequest>,
) -> XtreamClusterPublishBatchResult {
    persist_input_xtream_playlist_clusters_to_disk_with_operations(
        app_config,
        input,
        cluster_readers,
        XtreamClusterStageOperations::default(),
    )
    .await
}

pub(super) async fn persist_input_xtream_playlist_clusters_to_disk_with_operations(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster_readers: Vec<XtreamClusterRefreshRequest>,
    operations: XtreamClusterStageOperations,
) -> XtreamClusterPublishBatchResult {
    let mut result = XtreamClusterPublishBatchResult::default();
    let mut staged = Vec::with_capacity(cluster_readers.len());
    for request in cluster_readers {
        let cluster = request.cluster;
        let outcome = match stage_input_xtream_playlist_cluster_to_disk(app_config, input, request, operations).await {
            Ok(outcome) => outcome,
            Err(error) => {
                result.record_cluster_error(cluster, error);
                return result;
            }
        };
        result.record_evaluation(outcome.evaluation());
        match outcome {
            StagedXtreamClusterOutcome::Failed { error, .. } => {
                result.record_cluster_error(cluster, error);
                return result;
            }
            ready_or_retained => staged.push(ready_or_retained),
        }
    }

    let staged_clusters: HashSet<XtreamCluster> = staged
        .iter()
        .filter_map(|outcome| match outcome {
            StagedXtreamClusterOutcome::Ready(refresh) => Some(refresh.cluster),
            StagedXtreamClusterOutcome::RetainedPrevious(_) | StagedXtreamClusterOutcome::Failed { .. } => None,
        })
        .collect();
    let has_publishable_clusters = !staged_clusters.is_empty();
    let mut has_items = staged
        .iter()
        .any(|outcome| matches!(outcome, StagedXtreamClusterOutcome::Ready(refresh) if refresh.item_count > 0));
    if has_publishable_clusters && !has_items {
        let cfg = app_config.config.load();
        let storage_path = match ensure_input_storage_path(&cfg, &input.name).await {
            Ok(storage_path) => storage_path,
            Err(error) => {
                drop(cfg);
                result.errors.push(error);
                return result;
            }
        };
        drop(cfg);
        for cluster in XTREAM_CLUSTER {
            if input_cluster_enabled(input, cluster) && !staged_clusters.contains(&cluster) {
                match published_xtream_cluster_has_items(app_config, &storage_path, cluster).await {
                    Ok(true) => {
                        has_items = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        result.errors.push(error);
                        return result;
                    }
                }
            }
        }
    }

    let all_publishable_clusters_are_forced = has_publishable_clusters
        && staged.iter().all(
            |outcome| matches!(outcome, StagedXtreamClusterOutcome::Ready(refresh) if refresh.evaluation.force_update.is_some()),
        );
    if has_publishable_clusters && !has_items && !all_publishable_clusters_are_forced {
        result.errors.push(TuliproxError::RepositoryPlaylist(format!(
            "Refusing to publish empty disk-based Xtream playlist for input '{}'; existing data was retained",
            input.name
        )));
        return result;
    }

    for outcome in staged {
        match outcome {
            StagedXtreamClusterOutcome::Ready(refresh) => {
                let evaluation = refresh.evaluation;
                let cluster = refresh.cluster;
                if let Err(error) = publish_staged_xtream_cluster(app_config, refresh).await {
                    result.record_cluster_error(cluster, error);
                    break;
                }
                result.outcomes.push(if let Some(force_update) = evaluation.force_update {
                    XtreamClusterPublishOutcome::ForcePublished(force_update)
                } else if let Some(quality_acceptance) = evaluation.quality_acceptance {
                    XtreamClusterPublishOutcome::QualityAccepted(quality_acceptance)
                } else {
                    XtreamClusterPublishOutcome::Published
                });
            }
            StagedXtreamClusterOutcome::RetainedPrevious(rejection) => {
                result.outcomes.push(XtreamClusterPublishOutcome::RetainedPrevious(rejection));
            }
            StagedXtreamClusterOutcome::Failed { error, .. } => {
                result.errors.push(error);
                break;
            }
        }
    }
    result
}

pub async fn persist_input_xtream_playlist_cluster_to_disk(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster: XtreamCluster,
    quality_threshold: u8,
    categories: DynReader,
    streams: DynReader,
) -> Result<XtreamClusterPublishOutcome, TuliproxError> {
    let mut result = persist_input_xtream_playlist_clusters_to_disk(
        app_config,
        input,
        vec![XtreamClusterRefreshRequest {
            cluster,
            quality: XtreamClusterQualityPolicy::Enforce { threshold: quality_threshold },
            categories,
            streams,
        }],
    )
    .await;
    if let Some(error) = result.errors.pop() {
        return Err(error);
    }
    result.outcomes.pop().ok_or_else(|| {
        TuliproxError::RepositoryXtream(format!(
            "Missing Xtream publish outcome for input '{}' cluster {cluster}",
            input.name
        ))
    })
}
