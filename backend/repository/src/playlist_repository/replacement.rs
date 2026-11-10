use crate::{
    ensure_xtream_storage_path,
    epg_group_index::{epg_group_channels_path, epg_groups_path},
    epg_repository::stage_replacement_epg,
    get_file_path_for_db_index, get_target_id_mapping_file,
    target_id_mapping::get_uuid_index_path,
    xtream_category_view_lock_path, xtream_get_epg_file_path_for_target,
    xtream_repository::target_publish::{
        publish_target_file_replacements, stage_complete_xtream_playlist, write_target_staging,
        TargetEmptyReplacementMode, TargetFileReplacement,
    },
    BPlusTree, FailoverGroupStagedPublication, PlaylistStorageState, TargetIdMapping, TargetPlaylistStorage,
};
use shared::{error::TuliproxError, model::PlaylistGroup};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::{
    sync::Notify,
    time::{timeout_at, Instant},
};
use tuliprox_core::{
    model::{AppConfig, ConfigTarget, Epg},
    utils::FileWriteGuard,
};
use uuid::Uuid;

#[derive(Debug, Default)]
struct PublicationState {
    closed: bool,
    active: usize,
    published: Vec<String>,
    target_failures: BTreeMap<String, String>,
    failed_publications: Vec<(String, String)>,
}

/// Coordinates the processing deadline with the irreversible part of each target publication.
#[derive(Debug)]
pub struct TargetReplacementDeadline {
    at: Instant,
    publication: Mutex<PublicationState>,
    drained: Notify,
}

impl TargetReplacementDeadline {
    pub fn new(at: Instant) -> Self { Self { at, publication: Mutex::default(), drained: Notify::new() } }

    fn state(&self) -> MutexGuard<'_, PublicationState> {
        self.publication.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn record_target_failure(&self, target: &str, reason: &str) {
        self.state().target_failures.entry(target.to_owned()).or_insert_with(|| reason.to_owned());
    }

    /// Take failure details for terminal reporting, including commit failures that outlived processing.
    pub fn take_target_failures(&self) -> BTreeMap<String, String> {
        let mut state = self.state();
        let mut failures = std::mem::take(&mut state.target_failures);
        for (target, reason) in std::mem::take(&mut state.failed_publications) {
            failures.entry(target).or_insert(reason);
        }
        failures
    }

    fn try_begin(&self) -> io::Result<ActivePublication<'_>> {
        let mut state = self.state();
        if state.closed || Instant::now() >= self.at {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "playlist update deadline expired before publication"));
        }
        state.active += 1;
        Ok(ActivePublication { deadline: self, result: None })
    }

    async fn close_and_drain(&self) -> Vec<String> {
        loop {
            {
                let mut state = self.state();
                state.closed = true;
                if state.active == 0 {
                    return std::mem::take(&mut state.published);
                }
            }
            // notify_one retains a permit if the last commit finishes before we wait.
            self.drained.notified().await;
        }
    }

    /// Cancel pending processing at the deadline, then wait only for active commits.
    /// On timeout, return the names of targets that successfully published.
    pub async fn run<F: std::future::Future>(&self, operation: F) -> Result<F::Output, Vec<String>> {
        let result = timeout_at(self.at, operation).await;
        match result {
            Ok(output) => Ok(output),
            Err(_) => Err(self.close_and_drain().await),
        }
    }
}

struct ActivePublication<'a> {
    deadline: &'a TargetReplacementDeadline,
    result: Option<(String, Result<(), String>)>,
}

impl Drop for ActivePublication<'_> {
    fn drop(&mut self) {
        let mut state = self.deadline.state();
        match self.result.take() {
            Some((target, Ok(()))) => state.published.push(target),
            Some((target, Err(reason))) => state.failed_publications.push((target, reason)),
            None => {}
        }
        state.active -= 1;
        if state.closed && state.active == 0 {
            self.deadline.drained.notify_one();
        }
    }
}

pub(super) struct TargetReplacement {
    files: Vec<TargetFileReplacement>,
    staged_mapping: PathBuf,
    token: uuid::fmt::Simple,
}

impl TargetReplacement {
    fn error(target: &str, error: impl std::fmt::Display) -> TuliproxError {
        TuliproxError::RepositoryPlaylist(format!("Target '{target}' replacement failed: {error}"))
    }

    pub(super) async fn stage_mapping(
        app: &AppConfig,
        target_path: &Path,
        target: &ConfigTarget,
    ) -> Result<(TargetIdMapping, FileWriteGuard, Self), TuliproxError> {
        let mapping_path = get_target_id_mapping_file(target_path);
        let lock = app.file_locks.write_lock(&mapping_path).await;
        let target_name = target.name.clone();
        let cache = target.use_memory_cache;
        tokio::task::spawn_blocking(move || {
            let token = Uuid::new_v4().simple();
            let staged_mapping = mapping_path.with_file_name(format!(".id-mapping-replace-{token}.db"));
            let index = get_uuid_index_path(&mapping_path);
            let staged_index = get_uuid_index_path(&staged_mapping);
            let files = vec![
                TargetFileReplacement::database(&mapping_path, &staged_mapping, token),
                TargetFileReplacement::database(&index, &staged_index, token),
            ]
            .into_iter()
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| Self::error(&target_name, error))?;
            let replacement = Self { files, staged_mapping, token };
            for (published, staging) in [(&mapping_path, &replacement.staged_mapping), (&index, &staged_index)] {
                match std::fs::copy(published, staging) {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(Self::error(&target_name, error)),
                }
            }
            // A staged mapping can allocate IDs freely. The published mapping stays untouched
            // until the same transaction that installs the prepared streams commits.
            let mut mapping = TargetIdMapping::new(&replacement.staged_mapping, cache)?;
            mapping.require_explicit_persist();
            Ok((mapping, lock, replacement))
        })
        .await
        .map_err(|error| Self::error(&target.name, error))?
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn publish(
        mut self,
        app: &AppConfig,
        target: &ConfigTarget,
        playlist: &mut [PlaylistGroup],
        epg: Option<&Epg>,
        state: Option<&Arc<PlaylistStorageState>>,
        mut mapping: TargetIdMapping,
        mapping_lock: FileWriteGuard,
        groups: FailoverGroupStagedPublication,
        mode: TargetEmptyReplacementMode,
        deadline: Option<Arc<TargetReplacementDeadline>>,
    ) -> Result<(), TuliproxError> {
        let error = |error: &dyn std::fmt::Display| Self::error(&target.name, error);
        let storage = ensure_xtream_storage_path(&app.config.load(), &target.name).await?;
        let mut lock_paths = Vec::new();
        let xtream = stage_complete_xtream_playlist(
            &storage,
            playlist,
            target.use_memory_cache,
            &mut self.files,
            &mut lock_paths,
        )
        .await
        .map_err(|err| error(&err))?;
        let cache = match (target.use_memory_cache, state, xtream) {
            (true, Some(state), Some(xtream)) => Some((Arc::clone(&state.data), xtream)),
            (false, _, _) => None,
            _ => return Err(Self::error(&target.name, "configured memory cache is unavailable")),
        };
        lock_paths.push(self.stage_epg(target, &storage, playlist, epg).await?);
        self.files.extend(
            groups.files().map(|(published, staging)| TargetFileReplacement::new(&published, staging, self.token)),
        );

        let mapping_file = self.staged_mapping.clone();
        let keep_mapping = cache.is_some();
        let mapping_name = target.name.clone();
        let (replacement, id_mapping) = write_target_staging(self, move || {
            let mapping_error = |error| Self::error(&mapping_name, error);
            mapping.persist().map_err(mapping_error)?;
            drop(mapping);
            if keep_mapping {
                BPlusTree::load(&mapping_file).map(Some).map_err(mapping_error)
            } else {
                Ok(None)
            }
        })
        .await?;
        // Match the reader's snapshot-before-catalog lock order.
        let category_view = app.file_locks.write_lock(&xtream_category_view_lock_path(&storage)).await;
        // Acquire every reader lock before touching any published path.
        lock_paths.sort_unstable();
        let mut locks = Vec::with_capacity(lock_paths.len());
        for path in lock_paths {
            locks.push(app.file_locks.write_lock(&path).await);
        }
        let new_cache = match cache {
            Some((data, xtream)) => {
                Some((data.write_owned().await, TargetPlaylistStorage { xtream: Some(xtream), m3u: None, id_mapping }))
            }
            None => None,
        };
        let target_name = target.name.clone();
        let state = state.cloned();
        // The same task owns every guard until both disk publication and cache swap are complete.
        tokio::task::spawn_blocking(move || {
            let mut publication = deadline.as_deref().map(TargetReplacementDeadline::try_begin).transpose()?;
            let result = (|| {
                let _guards = (mapping_lock, category_view, locks, groups);
                let mut replacement = replacement;
                let cache = new_cache;
                publish_target_file_replacements(&mut replacement.files, &mode)?;
                if let Some((mut cache, storage)) = cache {
                    cache.insert(target_name.clone(), storage);
                }
                if let Some(state) = state {
                    state.bump_failover_group_generation(&target_name);
                }
                Ok::<_, io::Error>(())
            })();
            if let Some(publication) = publication.as_mut() {
                publication.result = Some((target_name, result.as_ref().copied().map_err(ToString::to_string)));
            }
            result
        })
        .await
        .map_err(|err| error(&err))?
        .map_err(|err| error(&err))
    }
    async fn stage_epg(
        &mut self,
        target: &ConfigTarget,
        storage: &Path,
        playlist: &[PlaylistGroup],
        epg: Option<&Epg>,
    ) -> Result<PathBuf, TuliproxError> {
        let error = |error: &dyn std::fmt::Display| Self::error(&target.name, error);
        let epg_path = xtream_get_epg_file_path_for_target(storage);
        if let Some(epg) = epg.filter(|epg| !epg.children.is_empty()) {
            let database = storage.join(format!(".epg-replace-{}.db", self.token));
            let group_index = storage.join(format!(".epg-groups-replace-{}.db", self.token));
            let group_channels = storage.join(format!(".epg-channels-replace-{}.db", self.token));
            let files = [
                TargetFileReplacement::database(&epg_path, &database, self.token),
                TargetFileReplacement::database(&epg_groups_path(&epg_path), &group_index, self.token),
                TargetFileReplacement::database(&epg_group_channels_path(&epg_path), &group_channels, self.token),
                Ok(TargetFileReplacement::new(
                    &get_file_path_for_db_index(&epg_path),
                    Some(&get_file_path_for_db_index(&database)),
                    self.token,
                )),
            ];
            let files = files.into_iter().collect::<io::Result<Vec<_>>>().map_err(|err| error(&err))?;
            self.files.extend(
                stage_replacement_epg(target, epg, playlist, database, group_index, group_channels, files).await?,
            );
        } else {
            self.files.extend(
                [
                    epg_path.clone(),
                    epg_groups_path(&epg_path),
                    epg_group_channels_path(&epg_path),
                    get_file_path_for_db_index(&epg_path),
                ]
                .iter()
                .map(|path| TargetFileReplacement::new(path, None, self.token)),
            );
        }
        Ok(epg_path)
    }
}
