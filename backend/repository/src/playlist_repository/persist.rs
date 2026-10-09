use super::{
    episode::prepare_target_playlist_for_persistence,
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
    persist_input_library_playlist, write_strm_playlist,
};
use shared::{error::TuliproxError, model::PlaylistGroup};
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
    persist_playlist_views_with_mode(
        app_config,
        base_playlist,
        xtream_playlist,
        epg,
        target,
        playlist_state,
        options,
        TargetPersistenceMode::Persist,
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
    persist_playlist_views_with_mode(app_config, playlist, None, epg, target, playlist_state, options, persistence_mode)
        .await
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

    prepare_target_playlist_for_persistence(base_playlist, target, &mut target_id_mapping);
    if let Some(xtream_view) = xtream_playlist.as_deref_mut() {
        prepare_target_playlist_for_persistence(xtream_view, target, &mut target_id_mapping);
    }

    let mut prepared_outputs = prepare_target_output_playlists(target, base_playlist, xtream_playlist.as_deref());
    if let Err(error) = validate_force_empty_output_filters(target, &prepared_outputs, options) {
        target_id_mapping.discard_unpersisted_changes();
        drop(target_id_mapping);
        drop(file_lock);
        return Err(vec![error]);
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
