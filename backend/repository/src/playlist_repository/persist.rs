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
    ensure_target_storage_path, epg_write_for_target, load_input_local_library_playlist, m3u_write_playlist,
    persist_input_library_playlist, write_strm_playlist, ChannelGroupRecord, ChannelGroupStore, GroupMember,
    MemberArchive, PendingChannelGroup, TargetIdMapping,
};
use shared::{
    error::TuliproxError,
    model::{PlaylistGroup, StreamProperties, VirtualId},
};
use std::{path::Path, sync::Arc};
use tuliprox_core::model::{AppConfig, ConfigTarget, Epg, TargetOutput};

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
        &mut [],
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
    channel_groups: &mut [PendingChannelGroup],
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
        channel_groups,
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
        &mut [],
    )
    .await
}

fn collect_channel_group_aliases(
    playlist: &[PlaylistGroup],
    mapping: &TargetIdMapping,
    aliases: &mut Vec<(VirtualId, VirtualId)>,
) {
    for group in playlist {
        for channel in &group.channels {
            if let Some(cg_uuid) = channel.header.channel_group_uuid {
                if channel.header.uuid != cg_uuid {
                    if let Some(group_vid) = mapping.get_virtual_id_by_uuid(&cg_uuid) {
                        aliases.push((channel.header.virtual_id, group_vid));
                    }
                }
            }
        }
    }
}

fn build_channel_group_records(
    channel_groups: &[PendingChannelGroup],
    mapping: &TargetIdMapping,
) -> Vec<ChannelGroupRecord> {
    channel_groups
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
                        provider_id: shared::utils::get_provider_id(&member.header.id, &member.header.url)
                            .unwrap_or_default(),
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
            ChannelGroupRecord {
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
    channel_groups: &mut [PendingChannelGroup],
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

    for pending in channel_groups.iter_mut() {
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

    let records = build_channel_group_records(channel_groups, &target_id_mapping);
    let mut aliases = Vec::new();
    collect_channel_group_aliases(base_playlist, &target_id_mapping, &mut aliases);
    if let Some(xtream_view) = xtream_playlist.as_deref() {
        collect_channel_group_aliases(xtream_view, &target_id_mapping, &mut aliases);
    }

    let staged_groups = match ChannelGroupStore::write_staged(&target_path, &records, &aliases) {
        Ok(staged) => staged,
        Err(err) => {
            target_id_mapping.discard_unpersisted_changes();
            drop(target_id_mapping);
            drop(file_lock);
            return Err(vec![TuliproxError::RepositoryPlaylist(format!(
                "Failed to stage channel groups for target '{}': {err}",
                target.name
            ))]);
        }
    };

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

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => {
                persist_xtream_target_playlist(
                    app_config,
                    target,
                    pl,
                    options.accepted_empty_clusters | library_empty.replacement_clusters() | curation_empty_clusters,
                    persistence_mode,
                )
                .await
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
                "Failed to publish channel groups for target '{}': {err}",
                target.name
            )));
        } else if let Some(playlist_storage) = playlist_state {
            playlist_storage.bump_channel_group_generation();
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
