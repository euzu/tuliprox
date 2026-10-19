use super::{
    episode::{assign_item_identity, prepare_target_playlist_for_persistence},
    get_target_id_mapping,
    publication::{
        persist_xtream_target_playlist, playlist_has_items, prepare_target_output_playlists,
        validate_force_empty_output_filters, validate_target_playlist_persistence, TargetPersistenceMode,
    },
    target::load_target_memory_cache_snapshot,
    PlaylistStorageState, TargetPlaylistPersistOptions,
};
use crate::{
    ensure_target_storage_path, epg_write_for_target, lineup_guide_index_carry_over, lineup_guide_index_remove,
    lineup_guide_index_view, lineup_guide_index_write, load_input_local_library_playlist, m3u_lineup_guide_index_path,
    m3u_write_playlist, persist_input_library_playlist, write_strm_playlist, xtream_get_file_path,
    xtream_get_storage_path, xtream_lineup_guide_index_path, FailoverGroupBatch, FailoverGroupRecord,
    FailoverGroupStore, GroupMember, LineupGuideCarryOver, LineupGuideView, MemberArchive, PendingFailoverGroup,
    TargetIdMapping,
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{ClusterFlags, PlaylistGroup, StreamProperties, VirtualId, XtreamCluster},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::model::{AppConfig, Config, ConfigTarget, Epg, TargetOutput};

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
    )
    .await
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
    )
    .await
}

fn collect_failover_group_aliases(
    playlist: &[PlaylistGroup],
    mapping: &TargetIdMapping,
    aliases: &mut Vec<(VirtualId, VirtualId)>,
) {
    for group in playlist {
        for channel in &group.channels {
            if let Some(cg_uuid) = channel.header.failover_group_uuid {
                if channel.header.uuid != cg_uuid {
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
) -> Vec<FailoverGroupRecord> {
    failover_groups
        .iter()
        .map(|pending| {
            let group_vid = mapping.get_virtual_id_by_uuid(&pending.group_uuid).unwrap_or_default();
            let members = pending
                .members
                .iter()
                .map(|member| {
                    let archive = if let Some(StreamProperties::Live(live)) = &member.header.additional_properties {
                        live.tv_archive_duration
                            .and_then(|d| u32::try_from(d).ok())
                            .map(|duration_days| MemberArchive { duration_days })
                    } else {
                        None
                    };
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
                    }
                })
                .collect();
            FailoverGroupRecord {
                group_vid,
                key: Arc::clone(&pending.key),
                origin: pending.origin,
                policy: pending.policy,
                members,
                orphaned: pending.orphaned.clone(),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn persist_playlist_views_with_mode(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    mut xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    persistence_mode: TargetPersistenceMode,
    failover_groups: &mut FailoverGroupBatch,
) -> Result<(), Vec<TuliproxError>> {
    let playlist_is_empty = !playlist_has_items(base_playlist)
        && xtream_playlist.as_deref().is_none_or(|playlist| !playlist_has_items(playlist));
    if let Err(error) = validate_target_playlist_persistence(target, playlist_is_empty, options) {
        return Err(vec![error]);
    }
    let mut errors = vec![];
    let config = &app_config.config.load();
    let target_path = match ensure_target_storage_path(config, &target.name).await {
        Ok(path) => path,
        Err(err) => return Err(vec![err]),
    };

    let (mut target_id_mapping, file_lock) =
        match get_target_id_mapping(app_config, &target_path, target.use_memory_cache).await {
            Ok(result) => result,
            Err(err) => return Err(vec![err]),
        };

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
    if let Err(error) = validate_force_empty_output_filters(target, &prepared_outputs, options) {
        target_id_mapping.discard_unpersisted_changes();
        drop(target_id_mapping);
        drop(file_lock);
        return Err(vec![error]);
    }

    let records = build_failover_group_records(&failover_groups.groups, &target_id_mapping);
    let mut aliases = Vec::new();
    collect_failover_group_aliases(base_playlist, &target_id_mapping, &mut aliases);
    if let Some(xtream_view) = xtream_playlist.as_deref() {
        collect_failover_group_aliases(xtream_view, &target_id_mapping, &mut aliases);
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
    if let Err(err) = lineup_guide_index_remove(&app_config.file_locks, &outdated_indexes).await {
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
            if let Err(err) = lineup_guide_index_remove(&app_config.file_locks, std::slice::from_ref(path)).await {
                // The rebuild below still replaces it. Only if that fails too does the old index
                // stay, which is why both failures are logged.
                warn!("HDHomeRun guide index of target {} not removed: {err}", target.name);
            }
        }

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => {
                persist_xtream_target_playlist(app_config, target, pl, replace_empty_clusters, persistence_mode).await
            }
            TargetOutput::M3u(m3u_output) => {
                m3u_write_playlist(app_config, target, m3u_output, &target_path, pl, allows_empty_base).await
            }
            TargetOutput::Strm(strm_output) => {
                write_strm_playlist(app_config, target, strm_output, pl, allows_empty_base).await
            }
            TargetOutput::HdHomeRun(_hdhomerun_output) => Ok(()),
        };

        match result {
            Ok(()) => {
                if let Some((view, path, GuideIndexAction::Rebuild(carry_over))) = &mut guide_index {
                    if let Err(err) =
                        lineup_guide_index_write(&app_config.file_locks, pl, *view, path, carry_over.take()).await
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
                    if let Err(err) =
                        epg_write_for_target(config, target, &target_path, epg, output, Some(epg_pl)).await
                    {
                        errors.push(err);
                    }
                }
            }
            Err(err) => errors.push(err),
        }
    }

    if let Err(err) = target_id_mapping.persist() {
        errors.push(TuliproxError::Config(format!("{err}")));
    }
    // Keep lock until all outputs are persisted to prevent concurrent writers
    // from interleaving mapping and output state for the same target.
    // We must release it before loading caches below (which may acquire read locks).
    drop(target_id_mapping);
    drop(file_lock);

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

    if errors.is_empty() && target.use_memory_cache {
        match playlist_state {
            Some(playlist_storage) => {
                match load_target_memory_cache_snapshot(app_config, target, persistence_mode).await {
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
