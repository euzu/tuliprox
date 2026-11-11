use super::{
    episode::{assign_item_identity, prepare_target_playlist_for_persistence},
    get_target_id_mapping,
    publication::{
        persist_xtream_target_playlist, playlist_has_items, prepare_target_output_playlists,
        validate_force_empty_output_filters, validate_target_playlist_persistence, TargetPersistenceMode,
    },
    replacement::{OrdinaryPublication, StagePluginOutputs, TargetReplacement, TargetReplacementDeadline},
    target::load_target_memory_cache_snapshot,
    PlaylistStorageState, TargetPlaylistPersistOptions,
};
use crate::{
    ensure_target_storage_path,
    epg_repository::epg_write_for_target_tracked,
    lineup_guide_index::{lineup_guide_index_remove_tracked, lineup_guide_index_write_tracked},
    lineup_guide_index_carry_over, lineup_guide_index_view, load_input_local_library_playlist,
    m3u_lineup_guide_index_path,
    m3u_repository::m3u_write_playlist_tracked,
    persist_input_library_playlist,
    strm_repository::write_strm_playlist_tracked,
    xtream_get_file_path, xtream_get_storage_path, xtream_lineup_guide_index_path,
    xtream_repository::target_publish::TargetEmptyReplacementMode,
    FailoverGroupBatch, FailoverGroupRecord, FailoverGroupStore, GroupMember, LineupGuideCarryOver, LineupGuideView,
    MemberArchive, PendingFailoverGroup, TargetIdMapping,
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{archive_info, ClusterFlags, PlaylistGroup, UUIDType, VirtualId, XtreamCluster},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, Config, ConfigTarget, Epg, TargetOutput},
    utils::FileWriteGuard,
};

pub async fn persist_playlist(
    app_config: &Arc<AppConfig>,
    playlist: &mut [PlaylistGroup],
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
) -> Result<(), Vec<TuliproxError>> {
    persist_playlist_with_mode(
        app_config,
        playlist,
        epg,
        target,
        playlist_state,
        options,
        TargetPersistenceMode::Persist,
    )
    .await
}

pub async fn persist_playlist_views(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
) -> Result<(), Vec<TuliproxError>> {
    persist_playlist_views_with_groups(
        app_config,
        base_playlist,
        xtream_playlist,
        epg,
        target,
        playlist_state,
        options,
        &mut FailoverGroupBatch::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn persist_playlist_views_with_groups(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    failover_groups: &mut FailoverGroupBatch,
) -> Result<(), Vec<TuliproxError>> {
    persist_playlist_views_with_outputs(
        app_config,
        base_playlist,
        xtream_playlist,
        epg,
        target,
        playlist_state,
        options,
        failover_groups,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn persist_playlist_views_with_outputs(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    failover_groups: &mut FailoverGroupBatch,
    stage_plugins: Option<&StagePluginOutputs<'_>>,
) -> Result<(), Vec<TuliproxError>> {
    persist_playlist_views_with_mode(
        app_config,
        base_playlist,
        xtream_playlist,
        epg,
        target,
        playlist_state,
        options,
        TargetPersistenceMode::Persist,
        failover_groups,
        stage_plugins,
    )
    .await
}

type PlaylistViews = (Vec<PlaylistGroup>, Option<Vec<PlaylistGroup>>);
type PublicationResult = Result<(), Vec<TuliproxError>>;

/// Publish ordinary views and plugin outputs with one deadline and target mapping guard.
#[allow(clippy::too_many_arguments)]
pub async fn persist_ordinary_playlist_views<Plugins, F>(
    app_config: &Arc<AppConfig>,
    mut views: PlaylistViews,
    epg: Option<Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    mut failover_groups: FailoverGroupBatch,
    deadline: &Arc<TargetReplacementDeadline>,
    publish_plugins: Plugins,
) -> (PlaylistViews, PublicationResult)
where
    Plugins: FnOnce(Vec<PlaylistGroup>, PublicationResult) -> F + Send + 'static,
    F: std::future::Future<Output = (Vec<PlaylistGroup>, PublicationResult)> + Send + 'static,
{
    let playlist_is_empty =
        !playlist_has_items(&views.0) && views.1.as_deref().is_none_or(|playlist| !playlist_has_items(playlist));
    let locked = match validate_target_playlist_persistence(target, playlist_is_empty, &options) {
        Ok(()) => lock_target_for_publication(app_config, target, false).await,
        Err(error) => Err(error),
    };
    let (app_config, target, playlist_state) = (Arc::clone(app_config), target.clone(), playlist_state.cloned());
    match deadline
        .publish_ordinary(move |publication| async move {
            let _mapping_lock = locked.as_ref().ok().map(|locked| Arc::clone(&locked.mapping_lock));
            let mut result = match locked {
                Ok(locked) => {
                    persist_locked_playlist_views(
                        &app_config,
                        &mut views.0,
                        views.1.as_deref_mut(),
                        epg.as_ref(),
                        &target,
                        playlist_state.as_ref(),
                        options,
                        TargetPersistenceMode::Persist,
                        &mut failover_groups,
                        None,
                        locked,
                        Some(&publication),
                    )
                    .await
                }
                Err(error) => Err(vec![error]),
            };
            if target.plugin_outputs().next().is_some() {
                if let Err(error) = OrdinaryPublication::begin(Some(&publication)) {
                    let mut errors = result.err().unwrap_or_default();
                    errors.push(error);
                    return (views, Err(errors));
                }
                (views.0, result) = publish_plugins(views.0, result).await;
            }
            (views, result)
        })
        .await
    {
        Ok(completed) => completed,
        Err(error) => (
            (Vec::new(), None),
            Err(vec![TuliproxError::RepositoryPlaylist(format!(
                "Failed to complete ordinary target publication: {error}",
            ))]),
        ),
    }
}

pub(super) async fn persist_playlist_with_mode(
    app_config: &Arc<AppConfig>,
    playlist: &mut [PlaylistGroup],
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    persistence_mode: TargetPersistenceMode,
) -> Result<(), Vec<TuliproxError>> {
    persist_playlist_views_with_mode(
        app_config,
        playlist,
        None,
        epg,
        target,
        playlist_state,
        options,
        persistence_mode,
        &mut FailoverGroupBatch::default(),
        None,
    )
    .await
}

fn collect_published_group_uuids(playlist: &[PlaylistGroup], published: &mut HashSet<UUIDType>) {
    for group in playlist {
        for channel in &group.channels {
            if let Some(cg_uuid) = channel.header.failover_group_uuid {
                if channel.header.uuid == cg_uuid {
                    published.insert(cg_uuid);
                }
            }
        }
    }
}

fn collect_failover_group_aliases(
    playlist: &[PlaylistGroup],
    mapping: &TargetIdMapping,
    published_group_uuids: &HashSet<UUIDType>,
    aliases: &mut Vec<(VirtualId, VirtualId)>,
) {
    for group in playlist {
        for channel in &group.channels {
            if let Some(cg_uuid) = channel.header.failover_group_uuid {
                if channel.header.uuid != cg_uuid && published_group_uuids.contains(&cg_uuid) {
                    if let Some(group_vid) = mapping.get_virtual_id_by_uuid(&cg_uuid) {
                        aliases.push((channel.header.virtual_id, group_vid));
                    }
                }
            }
        }
    }
}

fn build_failover_group_records(
    failover_groups: &[PendingFailoverGroup],
    mapping: &TargetIdMapping,
    published_group_uuids: &HashSet<UUIDType>,
) -> Vec<FailoverGroupRecord> {
    failover_groups
        .iter()
        .filter(|pending| published_group_uuids.contains(&pending.group_uuid))
        .filter_map(|pending| {
            let group_vid = mapping.get_virtual_id_by_uuid(&pending.group_uuid)?;
            if group_vid.is_unset() {
                return None;
            }
            let members = pending
                .members
                .iter()
                .map(|member| {
                    const DEFAULT_ARCHIVE_DURATION_DAYS: u32 = 1;
                    let (has_archive, duration_days) =
                        archive_info(member.header.additional_properties.as_ref(), member.header.time_shift.as_ref());
                    // Default to 1 day (minimum standard replay window) when duration is unspecified.
                    let archive = has_archive.then(|| MemberArchive {
                        duration_days: duration_days.unwrap_or(DEFAULT_ARCHIVE_DURATION_DAYS),
                    });
                    GroupMember {
                        member_vid: member.header.virtual_id,
                        input_name: Arc::clone(&member.header.input_name),
                        provider_id: member.header.provider_id_or_default(),
                        input_stream_id: Arc::clone(&member.header.input_stream_id),
                        url: Arc::clone(&member.header.url),
                        item_type: member.header.item_type,
                        cluster: member.header.xtream_cluster,
                        title: Arc::clone(&member.header.title),
                        archive,
                        epg_channel_id: member.header.epg_channel_id.as_ref().map(Arc::clone),
                        additional_properties: member.header.additional_properties.clone(),
                        input_url: Arc::clone(&member.header.input_url),
                        upstream_user_agent: member.header.upstream_user_agent.as_ref().map(Arc::clone),
                    }
                })
                .collect();
            Some(FailoverGroupRecord {
                group_vid,
                key: Arc::clone(&pending.key),
                origin: pending.origin,
                policy: pending.policy,
                members,
                orphaned: pending.orphaned.clone(),
            })
        })
        .collect()
}

struct LockedTargetPublication {
    config: Arc<Config>,
    path: PathBuf,
    mapping: TargetIdMapping,
    mapping_lock: Arc<FileWriteGuard>,
    replacement: Option<TargetReplacement>,
}

async fn lock_target_for_publication(
    app_config: &AppConfig,
    target: &ConfigTarget,
    replace_target_contents: bool,
) -> Result<LockedTargetPublication, TuliproxError> {
    let config = app_config.config.load_full();
    let path = ensure_target_storage_path(&config, &target.name).await?;
    let (mapping, mapping_lock, replacement) = if replace_target_contents {
        let (mapping, lock, replacement) = TargetReplacement::stage_mapping(app_config, &path, target).await?;
        (mapping, lock, Some(replacement))
    } else {
        let (mapping, lock) = get_target_id_mapping(app_config, &path, target.use_memory_cache).await?;
        (mapping, lock, None)
    };
    Ok(LockedTargetPublication { config, path, mapping, mapping_lock: Arc::new(mapping_lock), replacement })
}

#[allow(clippy::too_many_arguments)]
async fn persist_playlist_views_with_mode(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    persistence_mode: TargetPersistenceMode,
    failover_groups: &mut FailoverGroupBatch,
    stage_plugins: Option<&StagePluginOutputs<'_>>,
) -> Result<(), Vec<TuliproxError>> {
    let playlist_is_empty = !playlist_has_items(base_playlist)
        && xtream_playlist.as_deref().is_none_or(|playlist| !playlist_has_items(playlist));
    if let Err(error) = validate_target_playlist_persistence(target, playlist_is_empty, &options) {
        return Err(vec![error]);
    }
    let locked = lock_target_for_publication(app_config, target, options.replace_target_contents)
        .await
        .map_err(|error| vec![error])?;
    persist_locked_playlist_views(
        app_config,
        base_playlist,
        xtream_playlist,
        epg,
        target,
        playlist_state,
        options,
        persistence_mode,
        failover_groups,
        stage_plugins,
        locked,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn persist_locked_playlist_views(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    mut xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    persistence_mode: TargetPersistenceMode,
    failover_groups: &mut FailoverGroupBatch,
    stage_plugins: Option<&StagePluginOutputs<'_>>,
    locked: LockedTargetPublication,
    publication: Option<&OrdinaryPublication>,
) -> Result<(), Vec<TuliproxError>> {
    let LockedTargetPublication {
        config,
        path: target_path,
        mapping: mut target_id_mapping,
        mapping_lock: file_lock,
        replacement,
    } = locked;
    let mut errors = vec![];
    let config = &config;
    if publication.is_some() {
        // Dropping a job still waiting for its first output lock must not write its pending IDs.
        target_id_mapping.require_explicit_persist();
    }
    let mut source_ordinal = 0u32;
    prepare_target_playlist_for_persistence(base_playlist, target, &mut target_id_mapping, &mut source_ordinal);
    if let Some(xtream_view) = xtream_playlist.as_deref_mut() {
        prepare_target_playlist_for_persistence(xtream_view, target, &mut target_id_mapping, &mut source_ordinal);
    }

    for pending in &mut failover_groups.groups {
        for member in &mut pending.members {
            assign_item_identity(&mut member.header, &mut source_ordinal, &mut target_id_mapping);
        }
    }

    let mut prepared_outputs = prepare_target_output_playlists(target, base_playlist, xtream_playlist.as_deref());
    if let Err(error) = validate_force_empty_output_filters(target, &prepared_outputs, &options) {
        target_id_mapping.discard_unpersisted_changes();
        drop(target_id_mapping);
        drop(file_lock);
        return Err(vec![error]);
    }

    // A group is only playable through the view that publishes its group channel. Output filters can
    // drop that channel from every view while the mapping still knows its id.
    let published_group_uuids = {
        let mut published = HashSet::new();
        for (output, prepared_output) in target.output.iter().zip(&prepared_outputs) {
            let source: &[PlaylistGroup] = match output {
                TargetOutput::Xtream(_) => xtream_playlist.as_deref().unwrap_or(&*base_playlist),
                _ => base_playlist,
            };
            collect_published_group_uuids(prepared_output.as_deref().unwrap_or(source), &mut published);
        }
        published
    };
    let records = build_failover_group_records(&failover_groups.groups, &target_id_mapping, &published_group_uuids);
    let mut aliases = Vec::new();
    collect_failover_group_aliases(base_playlist, &target_id_mapping, &published_group_uuids, &mut aliases);
    if let Some(xtream_view) = xtream_playlist.as_deref() {
        collect_failover_group_aliases(xtream_view, &target_id_mapping, &published_group_uuids, &mut aliases);
    }

    let staged_groups =
        match FailoverGroupStore::write_staged(&target_path, &records, &aliases, failover_groups.side_files()) {
            Ok(staged) => staged,
            Err(err) => {
                target_id_mapping.discard_unpersisted_changes();
                drop(target_id_mapping);
                drop(file_lock);
                return Err(vec![TuliproxError::RepositoryPlaylist(format!(
                    "Failed to stage failover groups for target '{}': {err}",
                    target.name
                ))]);
            }
        };

    if let Some(mut replacement) = replacement {
        let outputs = replacement
            .stage_outputs(
                app_config,
                &target_path,
                target,
                base_playlist,
                xtream_playlist,
                &mut prepared_outputs,
                epg,
                stage_plugins,
            )
            .await
            .map_err(|err| vec![err])?;
        let mode = match persistence_mode {
            #[cfg(test)]
            TargetPersistenceMode::FailEmptyReplacementAt(failure) => TargetEmptyReplacementMode::FailAt(failure),
            #[cfg(test)]
            TargetPersistenceMode::PauseDuringReplacement(hook) => {
                TargetEmptyReplacementMode::PauseDuringPublication(hook)
            }
            _ => TargetEmptyReplacementMode::Persist,
        };
        return replacement
            .publish(
                app_config,
                target,
                outputs,
                playlist_state,
                target_id_mapping,
                file_lock,
                staged_groups,
                mode,
                options.replacement_deadline,
            )
            .await
            .map_err(|err| vec![err]);
    }

    // Only the view `lineup.json` is published from keeps a guide index. Any other index would
    // describe a playlist the reader no longer asks, and a later re-configuration could mistake it
    // for a current one.
    //
    // The guide index is a sidecar: none of its failures may fail the playlist publication or skip
    // the memory cache reload below. A missing index only makes the tuners scan the playlist.
    let lineup_view = lineup_guide_index_view(target);
    let outdated_indexes: Vec<PathBuf> = [LineupGuideView::M3u, LineupGuideView::Xtream]
        .into_iter()
        .filter(|view| lineup_view != Some(*view))
        .filter_map(|view| lineup_guide_index_path(config, target, view, &target_path))
        .collect();
    if let Err(err) = lineup_guide_index_remove_tracked(&app_config.file_locks, &outdated_indexes, publication).await {
        warn!("Outdated HDHomeRun guide index of target {} not removed: {err}", target.name);
    }

    for (output, prepared_output) in target.output.iter().zip(&mut prepared_outputs) {
        let output_publication_plan = options.publication_plan.with_output_filter(output.filter().is_some());
        let source_playlist = match output {
            TargetOutput::Xtream(_) => xtream_playlist.as_deref_mut().unwrap_or(base_playlist),
            _ => &mut *base_playlist,
        };
        let pl: &mut [PlaylistGroup] = if let Some(filtered_playlist) = prepared_output.as_mut() {
            filtered_playlist.as_mut_slice()
        } else {
            source_playlist
        };
        let library_empty = options.library_empty.for_filtered_playlist(pl);
        let curation_empty_clusters = output_publication_plan.replacement_clusters();
        let allows_empty_base =
            library_empty.replaces_empty_target() || output_publication_plan.allows_empty_base_output();

        let replace_empty_clusters =
            options.accepted_empty_clusters | library_empty.replacement_clusters() | curation_empty_clusters;

        // Guide index of the HDHomeRun lineup. It describes the playlist db of this output, so it
        // follows what the writer below does with that db:
        // - rewritten: the old index goes before the db and the new one after it. A reader that
        //   loses the index falls back to its playlist scan, while one that kept an index of another
        //   playlist would resolve guide numbers to channels this target no longer serves.
        // - kept (an empty update that may not replace it): the old index still describes it.
        // - partly rewritten (one Xtream cluster kept): the entries of the kept cluster are carried
        //   over from the old index, read before it goes. Without an old index to read them from,
        //   the index goes and readers scan until a full update brings it back.
        let mut guide_index = match (lineup_view, output) {
            (Some(view @ LineupGuideView::M3u), TargetOutput::M3u(_)) => {
                // `m3u_write_playlist` keeps the db of an empty playlist it may not publish.
                let action = if pl.is_empty() && !allows_empty_base {
                    GuideIndexAction::Keep
                } else {
                    GuideIndexAction::Rebuild(None)
                };
                lineup_guide_index_path(config, target, view, &target_path).map(|path| (view, path, action))
            }
            (Some(view @ LineupGuideView::Xtream), TargetOutput::Xtream(_)) => {
                match lineup_guide_index_path(config, target, view, &target_path) {
                    Some(path) => {
                        let action =
                            xtream_guide_index_action(app_config, target, pl, replace_empty_clusters, &path).await;
                        Some((view, path, action))
                    }
                    None => None,
                }
            }
            _ => None,
        };
        if let Some((_, path, GuideIndexAction::Rebuild(_) | GuideIndexAction::Remove)) = &guide_index {
            if let Err(err) =
                lineup_guide_index_remove_tracked(&app_config.file_locks, std::slice::from_ref(path), publication).await
            {
                // The rebuild below still replaces it. Only if that fails too does the old index
                // stay, which is why both failures are logged.
                warn!("HDHomeRun guide index of target {} not removed: {err}", target.name);
            }
        }

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => {
                persist_xtream_target_playlist(
                    app_config,
                    target,
                    pl,
                    replace_empty_clusters,
                    persistence_mode.clone(),
                    publication,
                )
                .await
            }
            TargetOutput::M3u(m3u_output) => {
                m3u_write_playlist_tracked(
                    app_config,
                    target,
                    m3u_output,
                    &target_path,
                    pl,
                    allows_empty_base,
                    publication,
                )
                .await
            }
            TargetOutput::Strm(strm_output) => {
                write_strm_playlist_tracked(app_config, target, strm_output, pl, allows_empty_base, publication).await
            }
            // Rendered afterwards by the processing crate, which owns the plugin host (plan §18
            // *W6a seam freeze*, output providers); like HdHomeRun there is nothing to persist here.
            TargetOutput::HdHomeRun(_) | TargetOutput::Plugin(_) => Ok(()),
        };

        match result {
            Ok(()) => {
                if let Some((view, path, GuideIndexAction::Rebuild(carry_over))) = &mut guide_index {
                    if let Err(err) = lineup_guide_index_write_tracked(
                        &app_config.file_locks,
                        pl,
                        *view,
                        path,
                        carry_over.take(),
                        publication,
                    )
                    .await
                    {
                        warn!(
                            "HDHomeRun guide index of target {} not written, tuners scan the playlist: {err}",
                            target.name
                        );
                    }
                }
                let allows_empty_output = match output {
                    TargetOutput::Xtream(_) => {
                        !curation_empty_clusters.is_empty()
                            || !library_empty.replacement_clusters().is_empty()
                            || !options.accepted_empty_clusters.is_empty()
                    }
                    _ => allows_empty_base,
                };
                if !pl.is_empty() || allows_empty_output {
                    let epg_pl: &[PlaylistGroup] = pl;
                    if let Err(err) = epg_write_for_target_tracked(
                        config,
                        &app_config.file_locks,
                        target,
                        &target_path,
                        epg,
                        output,
                        Some(epg_pl),
                        publication,
                    )
                    .await
                    {
                        errors.push(err);
                    }
                }
            }
            Err(err) => errors.push(err),
        }
    }

    OrdinaryPublication::begin(publication).map_err(|error| vec![error])?;
    if let Err(err) = target_id_mapping.persist() {
        errors.push(TuliproxError::Config(format!("{err}")));
    }

    // Publish while the write guard is held: playback readers take the read lock on the same file
    // and must not observe a half-replaced failover store.
    if errors.is_empty() {
        if let Err(err) = staged_groups.publish() {
            errors.push(TuliproxError::RepositoryPlaylist(format!(
                "Failed to publish failover groups for target '{}': {err}",
                target.name
            )));
        } else if let Some(playlist_storage) = playlist_state {
            playlist_storage.bump_failover_group_generation(&target.name);
        }
    } else {
        staged_groups.discard();
    }

    drop(target_id_mapping);

    // A later update may only start after this target's disk state and cache agree.
    if errors.is_empty() && target.use_memory_cache {
        match playlist_state {
            Some(playlist_storage) => {
                match load_target_memory_cache_snapshot(app_config, &target_path, target, persistence_mode).await {
                    Ok(storage) => playlist_storage.replace_target(&target.name, storage).await,
                    Err(error) => errors.push(error),
                }
            }
            None => errors.push(TuliproxError::RepositoryPlaylist(format!(
                "Target '{}' was persisted but its configured memory cache is unavailable",
                target.name
            ))),
        }
    }
    drop(file_lock);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub async fn persist_input_media_server_playlist(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Result<(), TuliproxError>) {
    persist_input_library_playlist(app_config, file_path, playlist).await
}

pub async fn load_input_media_server_playlist(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    load_input_local_library_playlist(app_config, file_path).await
}

/// What a persistence step does with the guide index of the output it writes.
#[derive(Debug)]
enum GuideIndexAction {
    /// The playlist db is rewritten from `pl`, so is the index, plus the entries of the clusters the
    /// db keeps.
    Rebuild(Option<LineupGuideCarryOver>),
    /// The playlist db stays as it is, and so does the index describing it.
    Keep,
    /// Only part of the db is rewritten and no old index describes the rest.
    Remove,
}

/// Guide index action of the Xtream view, from what `xtream_write_playlist` does per cluster: a
/// cluster without items keeps its db unless it may be replaced by an empty one. A cluster that has
/// no db yet keeps nothing, so `pl` still describes it. The entries of a kept cluster are read from
/// the index at `index_path` now, before that index is removed for the rewrite.
async fn xtream_guide_index_action(
    app_config: &AppConfig,
    target: &ConfigTarget,
    pl: &[PlaylistGroup],
    replace_empty_clusters: ClusterFlags,
    index_path: &Path,
) -> GuideIndexAction {
    // The writer sorts items into clusters by their own cluster, not by their group's, so the items
    // are checked. One pass answers both clusters and stops once both are seen.
    let (mut has_live, mut has_video) = (false, false);
    for item in pl.iter().flat_map(|group| &group.channels) {
        match item.header.xtream_cluster {
            XtreamCluster::Live => has_live = true,
            XtreamCluster::Video => has_video = true,
            XtreamCluster::Series => {}
        }
        if has_live && has_video {
            break;
        }
    }

    let storage_path = xtream_get_storage_path(&app_config.config.load(), &target.name);
    let mut rewritten = false;
    let mut kept = Vec::new();
    for (cluster, flag, has_items) in
        [(XtreamCluster::Live, ClusterFlags::Live, has_live), (XtreamCluster::Video, ClusterFlags::Vod, has_video)]
    {
        if has_items || replace_empty_clusters.contains(flag) {
            rewritten = true;
            continue;
        }
        let has_db = match &storage_path {
            Some(path) => tokio::fs::try_exists(xtream_get_file_path(path, cluster)).await.unwrap_or(true),
            None => false,
        };
        if has_db {
            kept.push(cluster);
        }
    }
    if kept.is_empty() {
        GuideIndexAction::Rebuild(None)
    } else if !rewritten {
        GuideIndexAction::Keep
    } else {
        match lineup_guide_index_carry_over(&app_config.file_locks, index_path, &kept).await {
            Some(carry_over) => GuideIndexAction::Rebuild(Some(carry_over)),
            None => GuideIndexAction::Remove,
        }
    }
}

/// Guide index file of `view` for `target`.
fn lineup_guide_index_path(
    config: &Config,
    target: &ConfigTarget,
    view: LineupGuideView,
    target_path: &Path,
) -> Option<PathBuf> {
    match view {
        LineupGuideView::M3u => Some(m3u_lineup_guide_index_path(target_path)),
        LineupGuideView::Xtream => xtream_lineup_guide_index_path(config, &target.name),
    }
}
