use super::publication::playlist_has_items;
#[cfg(unix)]
use crate::xtream_repository::target_publish::check_target_replacement_filesystems;
use crate::{
    ensure_xtream_storage_path,
    epg_group_index::{epg_group_channels_path, epg_groups_path},
    epg_repository::stage_replacement_epg,
    get_file_path_for_db_index, get_target_id_mapping_file,
    lineup_guide_index::stage_lineup_guide_index,
    lineup_guide_index_view, m3u_get_epg_file_path_for_target, m3u_get_file_path_for_db, m3u_lineup_guide_index_path,
    m3u_repository::stage_m3u_playlist,
    strm_repository::stage_strm_playlist,
    target_id_mapping::get_uuid_index_path,
    xtream_category_view_lock_path, xtream_get_epg_file_path_for_target, xtream_lineup_guide_index_path,
    xtream_repository::target_publish::{
        publish_synced_target_file_replacements, stage_complete_xtream_playlist, sync_staged_target_files,
        TargetEmptyReplacementMode, TargetFileReplacement,
    },
    BPlusTree, FailoverGroupStagedPublication, LineupGuideView, PlaylistM3uStorage, PlaylistStorageState,
    PlaylistXtreamStorage, TargetIdMapping, TargetPlaylistStorage,
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
    model::{AppConfig, ConfigTarget, Epg, TargetOutput},
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
    export_ownership_lock: Option<FileWriteGuard>,
}

/// The processing layer renders a plugin output here before the repository publishes it.
pub struct StagedPluginOutput {
    file: TargetFileReplacement,
    _scratch: TargetFileReplacement,
}

impl StagedPluginOutput {
    pub fn new(published: &Path) -> Self {
        let token = Uuid::new_v4().simple();
        let file = TargetFileReplacement::staged_file(published, token);
        let path = file.staging_path();
        let scratch = tuliprox_core::utils::tmp_path_for(path);
        let scratch = TargetFileReplacement::new(path, Some(&scratch), token);
        Self { file, _scratch: scratch }
    }

    pub fn path(&self) -> &Path { self.file.staging_path() }
}

pub type StagePluginOutputs<'a> = dyn Fn(&[PlaylistGroup], &Path) -> futures::future::BoxFuture<'static, Result<Vec<StagedPluginOutput>, TuliproxError>>
    + Sync
    + 'a;

#[derive(Default)]
pub(super) struct StagedTargetViews {
    xtream: Option<PlaylistXtreamStorage>,
    m3u: Option<PlaylistM3uStorage>,
    lock_paths: Vec<PathBuf>,
    category_view: Option<PathBuf>,
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
            let replacement = Self { files, staged_mapping, token, export_ownership_lock: None };
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
    pub(super) async fn stage_outputs(
        &mut self,
        app: &Arc<AppConfig>,
        target_path: &Path,
        target: &ConfigTarget,
        base: &mut [PlaylistGroup],
        mut xtream: Option<&mut [PlaylistGroup]>,
        prepared: &mut [Option<Vec<PlaylistGroup>>],
        epg: Option<&Epg>,
        stage_plugins: Option<&StagePluginOutputs<'_>>,
    ) -> Result<StagedTargetViews, TuliproxError> {
        let mut staged = StagedTargetViews::default();
        let lineup = lineup_guide_index_view(target);
        let config = app.config.load_full();
        // One guard covers both export formats through publication. Later targets can then
        // recognize aliases to files created by an earlier target on the same volume.
        if target.output.iter().any(|output| match output {
            TargetOutput::M3u(output) => output.filename.is_some(),
            TargetOutput::Strm(_) => true,
            _ => false,
        }) {
            self.export_ownership_lock = Some(app.file_locks.write_lock(Path::new(".target-export-replacement")).await);
        }
        for (output, filtered) in target.output.iter().zip(prepared) {
            let source = match output {
                TargetOutput::Xtream(_) => xtream.as_deref_mut().unwrap_or(base),
                _ => &mut *base,
            };
            let playlist = filtered.as_deref_mut().unwrap_or(source);
            let view = match output {
                TargetOutput::Xtream(_) => {
                    let storage = ensure_xtream_storage_path(&config, &target.name).await?;
                    staged.category_view = Some(xtream_category_view_lock_path(&storage));
                    staged.xtream = stage_complete_xtream_playlist(
                        &storage,
                        playlist,
                        target.use_memory_cache,
                        &mut self.files,
                        &mut staged.lock_paths,
                    )
                    .await?;
                    let epg_path = xtream_get_epg_file_path_for_target(&storage);
                    self.stage_epg(target, &epg_path, playlist, epg).await?;
                    staged.lock_paths.push(epg_path);
                    LineupGuideView::Xtream
                }
                TargetOutput::M3u(output) => {
                    let (m3u, files) =
                        stage_m3u_playlist(app, target, output, target_path, playlist, self.token).await?;
                    staged.m3u = target.use_memory_cache.then_some(m3u);
                    self.files.extend(files);
                    staged.lock_paths.push(m3u_get_file_path_for_db(target_path));
                    let epg_path = m3u_get_epg_file_path_for_target(target_path);
                    self.stage_epg(target, &epg_path, playlist, epg).await?;
                    staged.lock_paths.push(epg_path);
                    LineupGuideView::M3u
                }
                TargetOutput::Strm(output) => {
                    let (index, files) =
                        stage_strm_playlist(app, target, output, target_path, playlist, self.token).await?;
                    staged.lock_paths.push(index);
                    self.files.extend(files);
                    continue;
                }
                TargetOutput::HdHomeRun(_) | TargetOutput::Plugin(_) => continue,
            };
            if lineup == Some(view) {
                let path = match view {
                    LineupGuideView::M3u => m3u_lineup_guide_index_path(target_path),
                    LineupGuideView::Xtream => xtream_lineup_guide_index_path(&config, &target.name)
                        .ok_or_else(|| Self::error(&target.name, "Xtream lineup path unavailable"))?,
                };
                self.files.push(stage_lineup_guide_index(playlist, view, &path, self.token).await?);
                staged.lock_paths.push(path);
            }
        }
        for (view, path) in [
            (LineupGuideView::M3u, Some(m3u_lineup_guide_index_path(target_path))),
            (LineupGuideView::Xtream, xtream_lineup_guide_index_path(&config, &target.name)),
        ] {
            if let Some(path) = path.filter(|path| lineup != Some(view) && path.exists()) {
                self.files.push(TargetFileReplacement::new(&path, None, self.token));
                staged.lock_paths.push(path);
            }
        }
        if target.plugin_outputs().next().is_some() {
            let stage = stage_plugins.ok_or_else(|| Self::error(&target.name, "plugin output renderer unavailable"))?;
            self.files.extend(stage(base, target_path).await?.into_iter().map(|output| output.file));
        }
        Ok(staged)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn publish(
        mut self,
        app: &AppConfig,
        target: &ConfigTarget,
        outputs: StagedTargetViews,
        state: Option<&Arc<PlaylistStorageState>>,
        mut mapping: TargetIdMapping,
        mapping_lock: FileWriteGuard,
        groups: FailoverGroupStagedPublication,
        mode: TargetEmptyReplacementMode,
        deadline: Option<Arc<TargetReplacementDeadline>>,
    ) -> Result<(), TuliproxError> {
        let error = |error: &dyn std::fmt::Display| Self::error(&target.name, error);
        let StagedTargetViews { xtream, m3u, mut lock_paths, category_view } = outputs;
        let cache = if target.use_memory_cache {
            Some(Arc::clone(
                &state.ok_or_else(|| Self::error(&target.name, "configured memory cache is unavailable"))?.data,
            ))
        } else {
            None
        };
        self.files.extend(
            groups.files().map(|(published, staging)| TargetFileReplacement::new(&published, staging, self.token)),
        );

        let mapping_file = self.staged_mapping.clone();
        let keep_mapping = cache.is_some();
        let mapping_name = target.name.clone();
        // Complete staging I/O before acquiring the locks shared with readers.
        let (replacement, id_mapping, groups) = tokio::task::spawn_blocking(move || {
            let mapping_error = |error| Self::error(&mapping_name, error);
            #[cfg(unix)]
            check_target_replacement_filesystems(&self.files).map_err(mapping_error)?;
            mapping.persist().map_err(mapping_error)?;
            drop(mapping);
            let id_mapping =
                if keep_mapping { Some(BPlusTree::load(&mapping_file).map_err(mapping_error)?) } else { None };
            sync_staged_target_files(&self.files).map_err(mapping_error)?;
            Ok::<_, TuliproxError>((self, id_mapping, groups))
        })
        .await
        .map_err(|err| error(&err))??;
        // Match the reader's snapshot-before-catalog lock order.
        let category_view =
            if let Some(path) = category_view { Some(app.file_locks.write_lock(&path).await) } else { None };
        // Acquire every reader lock before touching any published path.
        lock_paths.sort_unstable();
        lock_paths.dedup();
        let mut locks = Vec::with_capacity(lock_paths.len());
        for path in lock_paths {
            locks.push(app.file_locks.write_lock(&path).await);
        }
        let new_cache = if let Some(data) = cache {
            Some((data.write_owned().await, TargetPlaylistStorage { xtream, m3u, id_mapping }))
        } else {
            None
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
                publish_synced_target_file_replacements(&mut replacement.files, &mode)?;
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
        epg_path: &Path,
        playlist: &[PlaylistGroup],
        epg: Option<&Epg>,
    ) -> Result<(), TuliproxError> {
        let error = |error: &dyn std::fmt::Display| Self::error(&target.name, error);
        if let Some(epg) = epg.filter(|epg| !epg.children.is_empty() && playlist_has_items(playlist)) {
            let database = epg_path.with_file_name(format!(".epg-replace-{}.db", self.token));
            let group_index = epg_path.with_file_name(format!(".epg-groups-replace-{}.db", self.token));
            let group_channels = epg_path.with_file_name(format!(".epg-channels-replace-{}.db", self.token));
            let files = [
                TargetFileReplacement::database(epg_path, &database, self.token),
                TargetFileReplacement::database(&epg_groups_path(epg_path), &group_index, self.token),
                TargetFileReplacement::database(&epg_group_channels_path(epg_path), &group_channels, self.token),
                Ok(TargetFileReplacement::new(
                    &get_file_path_for_db_index(epg_path),
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
                    epg_path.to_path_buf(),
                    epg_groups_path(epg_path),
                    epg_group_channels_path(epg_path),
                    get_file_path_for_db_index(epg_path),
                ]
                .iter()
                .map(|path| TargetFileReplacement::new(path, None, self.token)),
            );
        }
        Ok(())
    }
}
