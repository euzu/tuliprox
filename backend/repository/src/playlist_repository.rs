use super::playlist_mem_cache::{
    PlaylistM3uStorage, PlaylistStorageState, PlaylistXtreamStorage, TargetPlaylistStorage,
};
use crate::{
    ensure_target_storage_path, epg_write_for_target, get_input_storage_path, get_target_id_mapping_file,
    get_target_storage_path, load_input_local_library_playlist, load_input_m3u_playlist, load_input_xtream_playlist,
    m3u_get_file_path_for_db, m3u_write_playlist, persist_input_library_playlist, persist_input_m3u_playlist,
    stalker_repository::get_stalker_storage_path, write_strm_playlist, xtream_get_file_path, xtream_get_storage_path,
    xtream_write_playlist, BPlusTree, LocalLibraryDiskPlaylistSource, M3uDiskPlaylistSource,
    MediaServerDiskPlaylistSource, MemoryPlaylistSource, PlaylistSource, StalkerDiskPlaylistSource, TargetIdMapping,
    VirtualIdRecord, XtreamDiskPlaylistSource, FILE_SUFFIX_DB,
};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{
        xtream_const::XTREAM_CLUSTER, ClusterFlags, ConfigTargetOptions, InputPersistence, M3uPlaylistItem,
        PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, StreamProperties, UUIDType, VirtualId,
        XtreamCluster, XtreamPlaylistItem,
    },
    utils::{is_dash_url, is_hls_url, Internable},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{apply_filter_to_playlist, AppConfig, ConfigInput, ConfigTarget, Epg, TargetOutput},
    utils::{self, fold_epg_id_arc, normalized_source_ordinal},
};

struct LocalEpisodeKey {
    path: Arc<str>,
    virtual_id: u32,
}

fn playlist_has_items(playlist: &[PlaylistGroup]) -> bool { playlist.iter().any(|group| !group.channels.is_empty()) }

#[derive(Debug, Clone, Copy, Default)]
enum TargetPersistenceMode {
    #[default]
    Persist,
    #[cfg(test)]
    FailCacheReloadAt(TargetCacheReloadStage),
    #[cfg(test)]
    FailEmptyReplacementAt(crate::TargetEmptyReplacementFailure),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetCacheReloadStage {
    IdMapping,
    XtreamStorage,
}

/// Scoped persistence behavior derived from technically successful forced clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPlaylistPersistOptions {
    pub accepted_empty_clusters: ClusterFlags,
}

impl Default for InputPlaylistPersistOptions {
    fn default() -> Self { Self { accepted_empty_clusters: ClusterFlags::empty() } }
}

/// Empty Library contribution proven by successful input jobs, before target transforms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryEmptyPublication {
    #[default]
    None,
    /// Empty Library contribution without permission to clear the complete target/output.
    Contribution,
    /// All inputs are ready, non-Library inputs are populated, and no forced-empty cluster is involved.
    /// Configured transformations may remove the remaining entries from a target or individual output.
    FilterableContribution,
    /// All required inputs are empty Library catalogs, or the proven complete inputs
    /// have been reduced to an empty target/output by the configured transformations.
    CompleteTarget,
}

impl LibraryEmptyPublication {
    #[must_use]
    pub const fn replaces_empty_target(self) -> bool { matches!(self, Self::CompleteTarget) }

    /// Resolve the input-level authorization against the actual prepared target/output.
    /// A plain contribution never authorizes a fully empty result from a foreign input.
    #[must_use]
    pub fn for_filtered_playlist(self, playlist: &[PlaylistGroup]) -> Self {
        match self {
            Self::FilterableContribution if !playlist_has_items(playlist) => Self::CompleteTarget,
            Self::None | Self::Contribution | Self::FilterableContribution | Self::CompleteTarget => self,
        }
    }

    fn replacement_clusters(self) -> ClusterFlags {
        match self {
            Self::None => ClusterFlags::empty(),
            Self::Contribution | Self::FilterableContribution => ClusterFlags::Vod | ClusterFlags::Series,
            Self::CompleteTarget => ClusterFlags::all(),
        }
    }
}

/// Target persistence behavior authorized by successful input results, not by Quality inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPlaylistPersistOptions {
    pub accepted_empty_clusters: ClusterFlags,
    pub library_empty: LibraryEmptyPublication,
}

impl Default for TargetPlaylistPersistOptions {
    fn default() -> Self {
        Self { accepted_empty_clusters: ClusterFlags::empty(), library_empty: LibraryEmptyPublication::None }
    }
}

fn validate_target_playlist_persistence(
    target: &ConfigTarget,
    playlist_is_empty: bool,
    options: TargetPlaylistPersistOptions,
) -> Result<(), TuliproxError> {
    if !playlist_is_empty || options.library_empty.replaces_empty_target() {
        return Ok(());
    }
    if options.accepted_empty_clusters.is_empty() {
        return Err(TuliproxError::RepositoryPlaylist(format!(
            "Refusing to persist empty playlist for target '{}'; existing data was retained",
            target.name
        )));
    }
    if target.output.iter().any(|output| !matches!(output, TargetOutput::Xtream(_))) {
        return Err(TuliproxError::RepositoryPlaylist(format!(
            "Target '{}' has non-Xtream outputs that cannot safely publish a fully empty forced result; existing data was retained",
            target.name
        )));
    }
    Ok(())
}

fn prepare_target_output_playlists(
    target: &ConfigTarget,
    playlist: &[PlaylistGroup],
) -> Vec<Option<Vec<PlaylistGroup>>> {
    target
        .output
        .iter()
        .map(|output| output.filter().map(|filter| apply_filter_to_playlist(playlist, filter)))
        .collect()
}

fn validate_force_empty_output_filters(
    target: &ConfigTarget,
    prepared_outputs: &[Option<Vec<PlaylistGroup>>],
    options: TargetPlaylistPersistOptions,
) -> Result<(), TuliproxError> {
    if options.accepted_empty_clusters.is_empty() {
        return Ok(());
    }

    debug_assert_eq!(target.output.len(), prepared_outputs.len());
    for (output, prepared_output) in target.output.iter().zip(prepared_outputs) {
        let output_name = match output {
            TargetOutput::M3u(_) => "M3U",
            TargetOutput::Strm(_) => "STRM",
            TargetOutput::Xtream(_) | TargetOutput::HdHomeRun(_) => continue,
        };
        let Some(filtered_playlist) = prepared_output else {
            continue;
        };
        if !playlist_has_items(filtered_playlist) {
            return Err(TuliproxError::RepositoryPlaylist(format!(
                "Refusing to publish force-empty {output_name} output for target '{}' after its output filter; existing data was retained",
                target.name
            )));
        }
    }

    Ok(())
}

async fn persist_xtream_target_playlist(
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    accepted_empty_clusters: ClusterFlags,
    mode: TargetPersistenceMode,
) -> Result<(), TuliproxError> {
    #[cfg(test)]
    if let TargetPersistenceMode::FailEmptyReplacementAt(failure) = mode {
        return crate::xtream_write_playlist_with_injected_empty_replacement_failure(
            app_config,
            target,
            playlist,
            accepted_empty_clusters,
            failure,
        )
        .await;
    }
    #[cfg(not(test))]
    let _ = mode;

    xtream_write_playlist(app_config, target, playlist, accepted_empty_clusters).await
}

pub struct ProviderEpisodeKey {
    pub provider_id: u32,
    pub virtual_id: u32,
}

fn normalize_target_playlist_epg_ids(playlist: &mut [PlaylistGroup], target_options: Option<&ConfigTargetOptions>) {
    if !target_options.is_some_and(ConfigTargetOptions::lowercase_epg_ids) {
        return;
    }

    for group in playlist {
        for channel in &mut group.channels {
            let Some(epg_id) = channel.header.epg_channel_id.as_mut() else {
                continue;
            };
            if epg_id.is_empty() {
                continue;
            }

            *epg_id = fold_epg_id_arc(epg_id);
        }
    }
}

#[allow(clippy::too_many_lines)]
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

#[allow(clippy::too_many_lines)]
async fn persist_playlist_with_mode(
    app_config: &Arc<AppConfig>,
    playlist: &mut [PlaylistGroup],
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    options: TargetPlaylistPersistOptions,
    persistence_mode: TargetPersistenceMode,
) -> Result<(), Vec<TuliproxError>> {
    let playlist_is_empty = !playlist_has_items(playlist);
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

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    let mut provider_series = HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new();
    let mut media_server_series = HashMap::<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>::new();

    let mut source_ordinal: u32 = 0;
    // Virtual IDs assignment
    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let header = &mut channel.header;
            source_ordinal += 1;
            header.source_ordinal = source_ordinal;
            let provider_id = header.get_provider_id().unwrap_or_default();
            if provider_id == 0 {
                header.item_type = match (is_hls_url(&header.url), header.item_type) {
                    (true, _) => PlaylistItemType::LiveHls,
                    (false, PlaylistItemType::Live) => {
                        if is_dash_url(&header.url) {
                            PlaylistItemType::LiveDash
                        } else {
                            PlaylistItemType::LiveUnknown
                        }
                    }
                    _ => header.item_type,
                };
            }

            let uuid = header.get_uuid();
            let item_type = header.item_type;
            let parent_virtual_id = if item_type.is_series() {
                target_id_mapping.get_parent_virtual_id_by_uuid(uuid).unwrap_or_default()
            } else {
                VirtualId::default()
            };
            header.virtual_id =
                target_id_mapping.get_and_update_virtual_id(uuid, provider_id, item_type, parent_virtual_id);
        }
    }

    rewrite_series_episode_parent_virtual_ids(playlist, &mut target_id_mapping);

    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let header = &mut channel.header;
            let item_type = header.item_type;
            if item_type == PlaylistItemType::LocalSeries {
                assign_local_series_info_episode_key(&mut local_library_series, header, item_type);
            } else if is_media_server_series_episode_header(header) {
                assign_media_server_series_info_episode(&mut media_server_series, header);
            } else if item_type == PlaylistItemType::Series {
                assign_provider_series_info_episode_key(&mut provider_series, header, item_type);
            }
        }
    }

    materialize_media_server_series_info_episodes(playlist, &media_server_series);
    rewrite_series_info_episode_virtual_id(playlist, &local_library_series, &provider_series);
    drop(local_library_series);
    drop(provider_series);
    drop(media_server_series);

    normalize_target_playlist_epg_ids(playlist, target.options.as_ref());

    let mut prepared_outputs = prepare_target_output_playlists(target, playlist);
    if let Err(error) = validate_force_empty_output_filters(target, &prepared_outputs, options) {
        target_id_mapping.discard_unpersisted_changes();
        drop(target_id_mapping);
        drop(file_lock);
        return Err(vec![error]);
    }

    for (output, prepared_output) in target.output.iter().zip(&mut prepared_outputs) {
        let pl: &mut [PlaylistGroup] = if let Some(filtered_playlist) = prepared_output.as_mut() {
            filtered_playlist.as_mut_slice()
        } else {
            &mut *playlist
        };
        let library_empty = options.library_empty.for_filtered_playlist(pl);

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => {
                persist_xtream_target_playlist(
                    app_config,
                    target,
                    pl,
                    options.accepted_empty_clusters | library_empty.replacement_clusters(),
                    persistence_mode,
                )
                .await
            }
            TargetOutput::M3u(m3u_output) => {
                m3u_write_playlist(app_config, target, m3u_output, &target_path, pl, library_empty).await
            }
            TargetOutput::Strm(strm_output) => {
                write_strm_playlist(app_config, target, strm_output, pl, library_empty).await
            }
            TargetOutput::HdHomeRun(_hdhomerun_output) => Ok(()),
        };

        match result {
            Ok(()) => {
                if !pl.is_empty() {
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

fn assign_local_series_info_episode_key(
    local_library_series: &mut HashMap<Arc<str>, Vec<LocalEpisodeKey>>,
    header: &mut PlaylistItemHeader,
    item_type: PlaylistItemType,
) {
    // we need to rewrite local series info with the new virtual ids
    if item_type == PlaylistItemType::LocalSeries {
        local_library_series
            .entry(header.parent_code.clone())
            .or_default()
            .push(LocalEpisodeKey { path: header.url.clone(), virtual_id: header.virtual_id.get() });
    }
}

fn assign_provider_series_info_episode_key(
    provider_series: &mut HashMap<Arc<str>, Vec<ProviderEpisodeKey>>,
    header: &mut PlaylistItemHeader,
    item_type: PlaylistItemType,
) {
    // we need to rewrite local series info with the new virtual ids
    if item_type == PlaylistItemType::Series {
        provider_series.entry(header.parent_code.clone()).or_default().push(ProviderEpisodeKey {
            provider_id: header.get_provider_id().unwrap_or_default(),
            virtual_id: header.virtual_id.get(),
        });
    }
}

fn is_media_server_item_header(header: &PlaylistItemHeader) -> bool {
    header.id.starts_with("media-server:") || header.url.starts_with("media-server://")
}

fn is_media_server_series_info_header(header: &PlaylistItemHeader) -> bool {
    header.item_type == PlaylistItemType::SeriesInfo && is_media_server_item_header(header)
}

fn is_media_server_series_episode_header(header: &PlaylistItemHeader) -> bool {
    header.item_type == PlaylistItemType::Series
        && !header.parent_code.is_empty()
        && is_media_server_item_header(header)
}

fn assign_media_server_series_info_episode(
    media_server_series: &mut HashMap<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>,
    header: &PlaylistItemHeader,
) {
    if let Some(episode) = media_server_series_episode_detail(header) {
        media_server_series.entry(header.parent_code.clone()).or_default().push(episode);
    }
}

fn media_server_series_episode_detail(header: &PlaylistItemHeader) -> Option<SeriesStreamDetailEpisodeProperties> {
    if !is_media_server_series_episode_header(header) {
        return None;
    }

    let Some(StreamProperties::Episode(episode)) = header.additional_properties.as_ref() else {
        return None;
    };
    let episode = episode.as_ref();

    Some(SeriesStreamDetailEpisodeProperties {
        id: header.virtual_id.get(),
        episode_num: episode.episode,
        season: episode.season,
        title: non_blank_arc(&header.title)
            .unwrap_or_else(|| non_blank_arc(&header.name).unwrap_or_else(|| "Episode".intern())),
        container_extension: episode.container_extension.clone(),
        custom_sid: None,
        added: episode.added.clone().unwrap_or_else(|| "".intern()),
        direct_source: "".intern(),
        tmdb: episode.tmdb,
        release_date: episode.release_date.clone().unwrap_or_else(|| "".intern()),
        series_release_date: episode.series_release_date.clone(),
        plot: episode.plot.clone(),
        crew: None,
        duration_secs: 0,
        duration: "".intern(),
        movie_image: episode.movie_image.clone(),
        bitrate: 0,
        rating: None,
        video: episode.video.clone(),
        audio: episode.audio.clone(),
    })
}

fn non_blank_arc(value: &Arc<str>) -> Option<Arc<str>> { (!value.trim().is_empty()).then(|| Arc::clone(value)) }

fn source_series_info_episode_key(channel: &PlaylistItem) -> Option<Arc<str>> {
    match channel.header.item_type {
        PlaylistItemType::SeriesInfo => Some(channel.get_uuid().intern()),
        PlaylistItemType::LocalSeriesInfo => Some(channel.header.id.clone()),
        _ => None,
    }
}

fn header_uuid_episode_key(header: &PlaylistItemHeader) -> Option<Arc<str>> {
    (header.uuid != UUIDType::default()).then(|| header.uuid.intern())
}

fn push_unique_key(keys: &mut Vec<Arc<str>>, key: Arc<str>) {
    if !key.is_empty() && !keys.iter().any(|existing| existing.as_ref() == key.as_ref()) {
        keys.push(key);
    }
}

fn series_info_episode_lookup_keys(channel: &PlaylistItem) -> Vec<Arc<str>> {
    let mut keys = Vec::with_capacity(2);
    if let Some(alias_key) = header_uuid_episode_key(&channel.header) {
        push_unique_key(&mut keys, alias_key);
    }
    if let Some(source_key) = source_series_info_episode_key(channel) {
        push_unique_key(&mut keys, source_key);
    }
    keys
}

fn series_info_parent_keys(channel: &PlaylistItem) -> Vec<(Arc<str>, bool)> {
    let Some(source_key) = source_series_info_episode_key(channel) else { return Vec::new() };
    let Some(alias_key) = header_uuid_episode_key(&channel.header) else { return vec![(source_key, true)] };
    if alias_key.as_ref() == source_key.as_ref() {
        vec![(source_key, true)]
    } else {
        vec![(alias_key, true), (source_key, false)]
    }
}

#[allow(clippy::implicit_hasher)]
fn materialize_media_server_series_info_episodes(
    playlist: &mut [PlaylistGroup],
    media_server_series: &HashMap<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>,
) {
    if media_server_series.is_empty() {
        return;
    }

    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            if !is_media_server_series_info_header(&channel.header) {
                continue;
            }
            let lookup_keys = series_info_episode_lookup_keys(channel);
            let Some(episodes) = lookup_keys.iter().find_map(|key| media_server_series.get(key)) else { continue };
            let Some(StreamProperties::Series(series)) = channel.header.additional_properties.as_mut() else {
                continue;
            };
            let details = series.details.get_or_insert(SeriesStreamDetailProperties {
                year: None,
                seasons: None,
                episodes: None,
            });
            let mut episodes = episodes.clone();
            episodes.sort_by_key(|episode| (episode.season, episode.episode_num, episode.id));
            details.episodes = Some(episodes);
        }
    }
}

#[allow(clippy::implicit_hasher)]
fn rewrite_local_series_info_episode_virtual_id(
    pli: &mut PlaylistItem,
    local_library_series: &HashMap<Arc<str>, Vec<LocalEpisodeKey>>,
) {
    // local_library_series keys are the Series UUID or a category alias UUID.
    // For LocalSeriesInfo items, header.id is the source Series UUID; category
    // aliases use header.uuid so cloned episode rows can point at the alias.
    let lookup_keys = if pli.header.item_type == PlaylistItemType::LocalSeries {
        vec![pli.header.parent_code.clone()]
    } else {
        series_info_episode_lookup_keys(pli)
    };

    if let Some(episode_keys) = lookup_keys.iter().find_map(|key| local_library_series.get(key)) {
        if let Some(StreamProperties::Series(series)) = pli.header.additional_properties.as_mut() {
            if let Some(episodes) = series.details.as_mut().and_then(|d| d.episodes.as_mut()) {
                for episode in episodes.iter_mut() {
                    for episode_key in episode_keys {
                        if episode.direct_source == episode_key.path {
                            episode.id = episode_key.virtual_id;
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::implicit_hasher)]
pub fn rewrite_provider_series_info_episode_virtual_id<P>(
    pli: &mut P,
    provider_series: &HashMap<Arc<str>, Vec<ProviderEpisodeKey>>,
) where
    P: PlaylistEntry,
{
    let lookup_key = pli.get_uuid().intern();
    if let Some(episode_keys) = provider_series.get(&lookup_key) {
        if let Some(properties) = pli.get_additional_properties_mut() {
            apply_provider_episode_keys(properties, episode_keys);
        }
    }
}

#[allow(clippy::implicit_hasher)]
fn rewrite_provider_playlist_item_series_info_episode_virtual_id(
    pli: &mut PlaylistItem,
    provider_series: &HashMap<Arc<str>, Vec<ProviderEpisodeKey>>,
) {
    let lookup_keys = series_info_episode_lookup_keys(pli);
    if let Some(episode_keys) = lookup_keys.iter().find_map(|key| provider_series.get(key)) {
        if let Some(properties) = pli.get_additional_properties_mut() {
            apply_provider_episode_keys(properties, episode_keys);
        }
    }
}

fn apply_provider_episode_keys(properties: &mut StreamProperties, episode_keys: &[ProviderEpisodeKey]) {
    if let StreamProperties::Series(series) = properties {
        if let Some(episodes) = series.details.as_mut().and_then(|d| d.episodes.as_mut()) {
            for episode in episodes.iter_mut() {
                for episode_key in episode_keys {
                    if episode.id == episode_key.provider_id {
                        episode.id = episode_key.virtual_id;
                        break;
                    }
                }
            }
        }
    }
}

fn rewrite_series_info_episode_virtual_id(
    playlist: &mut [PlaylistGroup],
    local_library_series: &HashMap<Arc<str>, Vec<LocalEpisodeKey>>,
    provider_series: &HashMap<Arc<str>, Vec<ProviderEpisodeKey>>,
) {
    if local_library_series.is_empty() && provider_series.is_empty() {
        return;
    }
    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let item_type = channel.header.item_type;
            if item_type == PlaylistItemType::SeriesInfo {
                rewrite_provider_playlist_item_series_info_episode_virtual_id(channel, provider_series);
            } else if item_type == PlaylistItemType::LocalSeriesInfo {
                rewrite_local_series_info_episode_virtual_id(channel, local_library_series);
            } else if item_type == PlaylistItemType::LocalSeries {
                channel.header.parent_code = "".intern();
            }
        }
    }
}

fn rewrite_series_episode_parent_virtual_ids(playlist: &mut [PlaylistGroup], target_id_mapping: &mut TargetIdMapping) {
    let mut series_parent_virtual_ids = HashMap::<Arc<str>, u32>::new();

    for group in playlist.iter() {
        for channel in &group.channels {
            for (parent_key, overwrite) in series_info_parent_keys(channel) {
                if overwrite {
                    series_parent_virtual_ids.insert(parent_key, channel.header.virtual_id.get());
                } else {
                    series_parent_virtual_ids.entry(parent_key).or_insert(channel.header.virtual_id.get());
                }
            }
        }
    }

    if series_parent_virtual_ids.is_empty() {
        return;
    }

    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let header = &mut channel.header;
            if header.item_type.is_series() {
                if let Some(parent_virtual_id) = series_parent_virtual_ids.get(&header.parent_code) {
                    let provider_id = header.get_provider_id().unwrap_or_default();
                    let item_type = header.item_type;
                    let uuid = header.get_uuid();
                    header.virtual_id = target_id_mapping.get_and_update_virtual_id(
                        uuid,
                        provider_id,
                        item_type,
                        VirtualId::new(*parent_virtual_id),
                    );
                }
            }
        }
    }
}

pub async fn get_target_id_mapping(
    cfg: &AppConfig,
    target_path: &Path,
    use_memory_cache: bool,
) -> Result<(TargetIdMapping, utils::FileWriteGuard), TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let file_lock = cfg.file_locks.write_lock(&target_id_mapping_file).await;
    let mapping_path = target_id_mapping_file.clone();
    let mapping =
        tokio::task::spawn_blocking(move || TargetIdMapping::new(&mapping_path, use_memory_cache)).await.map_err(
            |err| TuliproxError::Config(format!("spawn_blocking failed while creating TargetIdMapping: {err}")),
        )??;

    Ok((mapping, file_lock))
}

async fn load_target_id_mapping_as_tree(
    app_config: &AppConfig,
    target_path: &Path,
    target: &ConfigTarget,
) -> Result<BPlusTree<VirtualId, VirtualIdRecord>, TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;

    // Move B+Tree load to spawn_blocking to avoid blocking tokio runtime
    let path_clone = target_id_mapping_file.clone();
    let target_name = target.name.clone();
    tokio::task::spawn_blocking(move || BPlusTree::<VirtualId, VirtualIdRecord>::load(&path_clone))
        .await
        .map_err(|e| TuliproxError::Config(format!("Blocking task failed: {e}")))?
        .map_err(|err| TuliproxError::Config(format!("Could not find path for target {target_name} err:{err}")))
}

async fn load_xtream_playlist_as_tree(
    app_config: &AppConfig,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<BPlusTree<u32, XtreamPlaylistItem>, TuliproxError> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
    // Move B+Tree query and iteration to spawn_blocking to avoid blocking tokio runtime
    let path_clone = xtream_path.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        BPlusTree::<u32, XtreamPlaylistItem>::load(&path_clone)
    })
    .await
    {
        Ok(Ok(tree)) => Ok(tree),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("No xtream {cluster} storage at {}, serving empty playlist", xtream_path.display());
            Ok(BPlusTree::new())
        }
        Ok(Err(err)) => Err(TuliproxError::RepositoryXtream(format!(
            "Failed to load xtream {cluster} storage {}: {err}",
            xtream_path.display()
        ))),
        Err(join_err) => Err(TuliproxError::RepositoryXtream(format!(
            "Failed to join xtream {cluster} storage load task {}: {join_err}",
            xtream_path.display()
        ))),
    }
}

async fn load_id_mapping_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<BPlusTree<VirtualId, VirtualIdRecord>, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str())
        .ok_or_else(|| TuliproxError::Config(format!("Could not find path for target {}", target.name)))?;

    load_target_id_mapping_as_tree(app_config, &target_path, target).await
}

pub async fn load_xtream_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<PlaylistXtreamStorage, TuliproxError> {
    let config = app_config.config.load();

    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(|| {
        TuliproxError::Config(format!("Could not find path for target {} xtream output", target.name))
    })?;

    let live_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Live).await?;
    let vod_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Video).await?;
    let series_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Series).await?;

    Ok(PlaylistXtreamStorage { live: live_storage, vod: vod_storage, series: series_storage })
}

pub async fn load_m3u_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<PlaylistM3uStorage, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str())
        .ok_or_else(|| TuliproxError::Config(format!("Could not find path for target {}", target.name)))?;

    let m3u_path = m3u_get_file_path_for_db(&target_path);
    let file_lock = app_config.file_locks.read_lock(&m3u_path).await;

    let path_clone = m3u_path.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        BPlusTree::<u32, M3uPlaylistItem>::load(&path_clone)
    })
    .await
    {
        Ok(Ok(tree)) => Ok(tree),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("No m3u storage at {}, serving empty playlist", m3u_path.display());
            Ok(BPlusTree::new())
        }
        Ok(Err(err)) => {
            Err(TuliproxError::RepositoryM3u(format!("Failed to load m3u storage {}: {err}", m3u_path.display())))
        }
        Err(join_err) => Err(TuliproxError::RepositoryM3u(format!(
            "Failed to join m3u storage load task {}: {join_err}",
            m3u_path.display()
        ))),
    }
}

fn target_cache_reload_error(target: &ConfigTarget, component: &str, error: impl std::fmt::Display) -> TuliproxError {
    TuliproxError::RepositoryPlaylist(format!(
        "Target '{}' was persisted but its {component} could not be reloaded into the memory cache: {error}",
        target.name
    ))
}

async fn load_target_memory_cache_snapshot(
    app_config: &AppConfig,
    target: &ConfigTarget,
    mode: TargetPersistenceMode,
) -> Result<TargetPlaylistStorage, TuliproxError> {
    #[cfg(not(test))]
    let _ = mode;

    #[cfg(test)]
    if matches!(mode, TargetPersistenceMode::FailCacheReloadAt(TargetCacheReloadStage::IdMapping)) {
        return Err(target_cache_reload_error(target, "ID mapping", "injected reload failure"));
    }
    let id_mapping = load_id_mapping_target_storage(app_config, target)
        .await
        .map_err(|error| target_cache_reload_error(target, "ID mapping", error))?;

    let xtream = if target.output.iter().any(|output| matches!(output, TargetOutput::Xtream(_))) {
        #[cfg(test)]
        if matches!(mode, TargetPersistenceMode::FailCacheReloadAt(TargetCacheReloadStage::XtreamStorage)) {
            return Err(target_cache_reload_error(target, "Xtream storage", "injected reload failure"));
        }
        Some(
            load_xtream_target_storage(app_config, target)
                .await
                .map_err(|error| target_cache_reload_error(target, "Xtream storage", error))?,
        )
    } else {
        None
    };

    let m3u = if target.output.iter().any(|output| matches!(output, TargetOutput::M3u(_))) {
        Some(
            load_m3u_target_storage(app_config, target)
                .await
                .map_err(|error| target_cache_reload_error(target, "M3U storage", error))?,
        )
    } else {
        None
    };

    Ok(TargetPlaylistStorage { xtream, m3u, id_mapping: Some(id_mapping) })
}

async fn publish_all_cluster_catalogs(
    app_config: &AppConfig,
    storage_path: &Path,
    input_name: &str,
    persisted_playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let mut live_groups = Vec::new();
    let mut vod_groups = Vec::new();
    let mut series_groups = Vec::new();
    for group in &persisted_playlist {
        match group.xtream_cluster {
            XtreamCluster::Live => live_groups.push(group.title.to_string()),
            XtreamCluster::Video => vod_groups.push(group.title.to_string()),
            XtreamCluster::Series => series_groups.push(group.title.to_string()),
        }
    }
    for (cluster, groups) in
        [(XtreamCluster::Live, live_groups), (XtreamCluster::Video, vod_groups), (XtreamCluster::Series, series_groups)]
    {
        if let Err(publish_err) =
            crate::publish_raw_group_catalog(storage_path, input_name, cluster, groups, &app_config.file_locks).await
        {
            warn!(
                "Playlist data for input '{input_name}' was persisted, but publishing its raw group catalog for cluster {cluster:?} failed: {publish_err}"
            );
        }
    }
    (persisted_playlist, None)
}

pub async fn persist_input_playlist(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    persist_input_playlist_with_options(app_config, input, playlist, InputPlaylistPersistOptions::default()).await
}

pub async fn persist_input_playlist_with_options(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    mut playlist: Vec<PlaylistGroup>,
    options: InputPlaylistPersistOptions,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let persistence = input.get_download_input_type().persistence();
    let accepts_empty = persistence == InputPersistence::Library
        || (!options.accepted_empty_clusters.is_empty()
            && matches!(persistence, InputPersistence::Xtream | InputPersistence::Stalker));
    if !playlist_has_items(&playlist) && !accepts_empty {
        let empty_error = TuliproxError::RepositoryPlaylist(format!(
            "Refusing to persist empty playlist for input '{}'; existing data was retained",
            input.name
        ));
        warn!("{empty_error}");
        return match load_input_playlist(app_config, input, None).await {
            Ok(mut previous) => {
                if previous.is_empty() {
                    (playlist, Some(empty_error))
                } else {
                    (previous.take_groups(), Some(empty_error))
                }
            }
            Err(load_err) => (
                playlist,
                Some(TuliproxError::RepositoryPlaylist(format!(
                    "{empty_error}; failed to load the retained input playlist: {load_err}"
                ))),
            ),
        };
    }
    if persistence == InputPersistence::Stalker {
        // The Stalker processor (`processor::stalker::download_stalker_playlist`)
        // is the single writer of the per-cluster B+Tree and raw group catalogs.
        // Re-encoding the `PlaylistItem` runtime projection back into `StalkerPlaylistItem`
        // would destroy the canonical `cmd`/`playback_descriptor`/capability
        // flags the processor just persisted — including the field the
        // runtime 4xx-re-resolve hook relies on. The disk layout is
        // already in sync; nothing to do here.
        return (playlist, None);
    }
    playlist.iter_mut().for_each(PlaylistGroup::on_load);
    let cfg = app_config.config.load();
    let storage_path = match get_input_storage_path(&input.name, &cfg.storage_dir).await {
        Ok(storage_path) => storage_path,
        Err(err) => {
            return (
                playlist,
                Some(TuliproxError::Config(format!(
                    "Error creating input storage directory for input '{}' failed: {err}",
                    input.name
                ))),
            );
        }
    };

    let (persisted_playlist, err) = match persistence {
        InputPersistence::Xtream => {
            crate::persist_input_xtream_playlist_with_empty_replacements(
                app_config,
                &storage_path,
                playlist,
                options.accepted_empty_clusters,
            )
            .await
        }

        InputPersistence::M3u => {
            // Persist M3U
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if let Err(err) = persist_input_m3u_playlist(app_config, &file_path, &playlist).await {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::Library => {
            // Persist local library playlist
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            let (playlist, result) = persist_input_library_playlist(app_config, &file_path, playlist).await;
            if let Err(err) = result {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::MediaServer => {
            let file_path = get_input_media_server_playlist_file_path(&storage_path, &input.name);
            let (playlist, result) = persist_input_media_server_playlist(app_config, &file_path, playlist).await;
            if let Err(err) = result {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::Stalker => unreachable!("handled above"),
    };

    if err.is_none() {
        return publish_all_cluster_catalogs(app_config, &storage_path, &input.name, persisted_playlist).await;
    }

    (persisted_playlist, err)
}

pub async fn load_input_playlist(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    clusters: Option<&[XtreamCluster]>,
) -> Result<PlaylistSource, TuliproxError> {
    let cfg = app_config.config.load();
    let storage_path = get_input_storage_path(&input.name, &cfg.storage_dir)
        .await
        .map_err(|e| TuliproxError::Config(format!("Error getting input path: {e}")))?;
    let disk_based_processing = cfg.disk_based_processing;

    match input.get_download_input_type().persistence() {
        InputPersistence::Xtream => {
            let clusters_to_load = clusters.unwrap_or(&XTREAM_CLUSTER);
            if disk_based_processing {
                let source =
                    PlaylistSource::xtream_disk(XtreamDiskPlaylistSource::new(app_config, &storage_path).await?);
                Ok(PlaylistSource::filtered(source, skipped_clusters(clusters_to_load)))
            } else {
                let groups = load_input_xtream_playlist(app_config, &storage_path, clusters_to_load).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::M3u => {
            // Load M3U
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::m3u_disk(M3uDiskPlaylistSource::new(app_config, &file_path).await?))
            } else {
                let groups = load_input_m3u_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::Library => {
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::local_library_disk(
                    LocalLibraryDiskPlaylistSource::new(app_config, &file_path).await?,
                ))
            } else {
                let groups = load_input_local_library_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::MediaServer => {
            let file_path = get_input_media_server_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::media_server_disk(MediaServerDiskPlaylistSource::new(app_config, &file_path).await?))
            } else {
                let groups = load_input_media_server_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::Stalker => {
            let clusters_to_load = clusters.unwrap_or(&XTREAM_CLUSTER);
            let stalker_path = get_stalker_storage_path(&storage_path);
            let stalker_config = input.stalker.as_ref().ok_or_else(|| {
                TuliproxError::ConfigInput(format!("Stalker input '{}' has no Stalker configuration", input.name))
            })?;
            let portal_url = input.resolve_url(&input.url)?.into_owned();
            let manifest = crate::stalker_generation_repository::load_active_manifest(
                &stalker_path,
                stalker_config.identity_fingerprint(&portal_url),
            )
            .await?;
            if disk_based_processing {
                let source = PlaylistSource::stalker_disk(
                    StalkerDiskPlaylistSource::new(app_config, &stalker_path, Arc::clone(&input.name), manifest)
                        .await?,
                );
                Ok(PlaylistSource::filtered(source, skipped_clusters(clusters_to_load)))
            } else {
                let groups = load_input_stalker_playlist(app_config, &input.name, clusters_to_load, &manifest).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
    }
}

fn skipped_clusters(clusters_to_load: &[XtreamCluster]) -> HashSet<XtreamCluster> {
    XTREAM_CLUSTER.iter().copied().filter(|cluster| !clusters_to_load.contains(cluster)).collect()
}

pub fn get_input_m3u_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("m3u_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

pub fn get_input_local_library_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("lib_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

pub fn get_input_media_server_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("media_server_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

/// Load a Stalker input's playlist into memory. The on-disk B+Tree is the
/// source of truth; we stream every per-cluster tree and bucket the items
/// by cluster to build the runtime `PlaylistGroup`s. `input_name` seeds the
/// canonical `PlaylistItem::from_stalker` conversion so item identity matches
/// the download path.
pub async fn load_input_stalker_playlist(
    app_config: &Arc<AppConfig>,
    input_name: &str,
    clusters: &[XtreamCluster],
    manifest: &crate::stalker_generation_repository::StalkerActiveManifest,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups_map: indexmap::IndexMap<(XtreamCluster, u32), PlaylistGroup> = indexmap::IndexMap::new();
    for &cluster in clusters {
        let mut batches = Vec::new();
        match cluster {
            XtreamCluster::Live => {
                if let Some(files) = manifest.live.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.data).await?);
                }
            }
            XtreamCluster::Video => {
                if let Some(files) = manifest.vod.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.data).await?);
                }
            }
            XtreamCluster::Series => {
                if let Some(files) = manifest.series.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.roots).await?);
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.episodes).await?);
                }
            }
        }
        for batch in batches {
            for item in batch {
                let category_id = item.category_id;
                let playlist_item = PlaylistItem::from_stalker(&item, input_name);
                groups_map
                    .entry((cluster, category_id))
                    .or_insert_with(|| PlaylistGroup {
                        id: category_id,
                        title: Arc::clone(&playlist_item.header.group),
                        channels: Vec::new(),
                        xtream_cluster: cluster,
                    })
                    .channels
                    .push(playlist_item);
            }
        }
    }
    let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
    for group in &mut groups {
        group.channels.sort_by_key(|item| normalized_source_ordinal(item.header.source_ordinal));
    }
    groups.sort_by_key(|group| {
        group.channels.first().map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal))
    });
    Ok(groups)
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

#[cfg(test)]
mod tests {
    use super::{
        assign_local_series_info_episode_key, assign_media_server_series_info_episode,
        get_input_media_server_playlist_file_path, materialize_media_server_series_info_episodes,
        normalize_target_playlist_epg_ids, persist_playlist_with_mode, playlist_has_items,
        rewrite_local_series_info_episode_virtual_id, rewrite_series_episode_parent_virtual_ids,
        rewrite_series_info_episode_virtual_id, skipped_clusters, validate_target_playlist_persistence,
        LocalEpisodeKey, ProviderEpisodeKey, TargetCacheReloadStage, TargetPersistenceMode,
        TargetPlaylistPersistOptions,
    };
    use crate::{load_xtream_target_storage, BPlusTreeQuery, PlaylistStorageState, TargetIdMapping, VirtualIdRecord};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        foundation::get_filter,
        model::{
            ClusterFlags, ConfigPaths, ConfigTargetOptions, EpgOutputOptions, EpisodeStreamProperties, M3uPlaylistItem,
            PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, ProcessingOrder,
            SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, SeriesStreamDetailSeasonProperties,
            SeriesStreamProperties, StreamProperties, StrmExportStyle, UUIDType, VirtualId, XtreamCluster,
            XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::{collections::HashMap, path::Path, sync::Arc};
    use tuliprox_core::{
        model::{
            ApiProxyConfig, AppConfig, Config, ConfigTarget, CustomStreamResponse, HdHomeRunConfig, M3uTargetOutput,
            MediaToolCapabilities, SourcesConfig, StagedFilter, StrmTargetFlagsSet, StrmTargetOutput,
            TargetExecutionPlan, TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
        },
        utils::FileLockManager,
    };

    fn target_with_outputs(output: Vec<TargetOutput>) -> ConfigTarget {
        target_with_options("target-empty-guard", output, false)
    }

    fn target_with_options(name: &str, output: Vec<TargetOutput>, use_memory_cache: bool) -> ConfigTarget {
        ConfigTarget {
            id: 1,
            enabled: true,
            name: name.to_string(),
            options: None,
            sort: None,
            filter: StagedFilter::default(),
            output,
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::new(None)),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            execution_plan: TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache,
        }
    }

    fn test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                storage_dir: storage_dir.to_string_lossy().into_owned(),
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::<HdHomeRunConfig>::default()),
            api_proxy: Arc::new(ArcSwapOption::<ApiProxyConfig>::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::<CustomStreamResponse>::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        })
    }

    fn target_group(cluster: XtreamCluster, id: u32, name: &str) -> PlaylistGroup {
        let name = name.intern();
        let mut header = PlaylistItemHeader {
            id: id.to_string().intern(),
            input_stream_id: id.to_string().intern(),
            name: Arc::clone(&name),
            title: Arc::clone(&name),
            group: Arc::clone(&name),
            input_name: "target-test-input".intern(),
            item_type: PlaylistItemType::from(cluster),
            xtream_cluster: cluster,
            category_id: id,
            ..PlaylistItemHeader::default()
        };
        header.gen_uuid();
        PlaylistGroup { id, title: name, channels: vec![PlaylistItem { header }], xtream_cluster: cluster }
    }

    fn xtream_target(name: &str, use_memory_cache: bool) -> ConfigTarget {
        target_with_options(
            name,
            vec![TargetOutput::Xtream(XtreamTargetOutput {
                flags: XtreamTargetFlagsSet::new(),
                trakt: None,
                filter: None,
            })],
            use_memory_cache,
        )
    }

    async fn cached_target_signature(
        playlist_state: &PlaylistStorageState,
        target_name: &str,
    ) -> (usize, Vec<(XtreamCluster, u32, Arc<str>)>) {
        let cache = playlist_state.data.read().await;
        let target = cache.get(target_name).expect("target cache");
        let mapping_len = target.id_mapping.as_ref().expect("cached ID mapping").len();
        let xtream = target.xtream.as_ref().expect("cached Xtream storage");
        let mut items = Vec::new();
        for (cluster, storage) in [
            (XtreamCluster::Live, &xtream.live),
            (XtreamCluster::Video, &xtream.vod),
            (XtreamCluster::Series, &xtream.series),
        ] {
            for virtual_id in 1..=16 {
                if let Some(item) = storage.query(&virtual_id) {
                    items.push((cluster, virtual_id, Arc::clone(&item.name)));
                }
            }
        }
        (mapping_len, items)
    }

    async fn seed_memory_cached_xtream_target(
        app_config: &Arc<AppConfig>,
        playlist_state: &Arc<PlaylistStorageState>,
        target: &ConfigTarget,
    ) {
        let mut baseline = vec![
            target_group(XtreamCluster::Live, 101, "old-live"),
            target_group(XtreamCluster::Video, 201, "old-vod"),
            target_group(XtreamCluster::Series, 301, "old-series"),
        ];
        persist_playlist_with_mode(
            app_config,
            &mut baseline,
            None,
            target,
            Some(playlist_state),
            TargetPlaylistPersistOptions::default(),
            TargetPersistenceMode::Persist,
        )
        .await
        .expect("baseline target persistence");
    }

    #[test]
    fn target_force_empty_authorization_is_limited_to_xtream_outputs() {
        let xtream_target = target_with_outputs(vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })]);
        let options = TargetPlaylistPersistOptions {
            accepted_empty_clusters: ClusterFlags::Vod,
            ..TargetPlaylistPersistOptions::default()
        };
        assert!(validate_target_playlist_persistence(&xtream_target, true, options).is_ok());

        let mixed_target = target_with_outputs(vec![
            TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None }),
            TargetOutput::M3u(M3uTargetOutput {
                filename: None,
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: None,
            }),
        ]);
        let error = validate_target_playlist_persistence(&mixed_target, true, options)
            .expect_err("fully empty mixed output must fail before persistence");
        assert!(error.to_string().contains("non-Xtream outputs"));
    }

    #[test]
    fn target_empty_playlist_without_force_authorization_remains_rejected() {
        let target = target_with_outputs(vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })]);
        let error = validate_target_playlist_persistence(&target, true, TargetPlaylistPersistOptions::default())
            .expect_err("unauthorized empty target must remain rejected");
        assert!(error.to_string().contains("Refusing to persist empty playlist"));
    }

    #[tokio::test]
    async fn target_cache_reload_failures_return_errors_without_mutating_the_previous_cache() {
        for stage in [TargetCacheReloadStage::IdMapping, TargetCacheReloadStage::XtreamStorage] {
            let directory = tempdir().expect("temporary storage");
            let app_config = test_app_config(directory.path());
            let playlist_state = Arc::new(PlaylistStorageState::new());
            let target = xtream_target(&format!("cache-reload-{stage:?}"), true);
            seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
            let before = cached_target_signature(&playlist_state, &target.name).await;
            let mut candidate = vec![
                target_group(XtreamCluster::Live, 102, "new-live"),
                target_group(XtreamCluster::Series, 302, "new-series"),
            ];

            let errors = persist_playlist_with_mode(
                &app_config,
                &mut candidate,
                None,
                &target,
                Some(&playlist_state),
                TargetPlaylistPersistOptions {
                    accepted_empty_clusters: ClusterFlags::Vod,
                    ..TargetPlaylistPersistOptions::default()
                },
                TargetPersistenceMode::FailCacheReloadAt(stage),
            )
            .await
            .expect_err("injected target cache reload must fail");

            assert!(errors.iter().any(|error| error.to_string().contains("could not be reloaded")));
            assert_eq!(cached_target_signature(&playlist_state, &target.name).await, before);
            let persisted = load_xtream_target_storage(&app_config, &target).await.expect("persisted target storage");
            assert_eq!([persisted.live.len(), persisted.vod.len(), persisted.series.len()], [1, 0, 1]);
        }
    }

    #[tokio::test]
    async fn target_force_empty_reload_replaces_the_complete_memory_cache() {
        let directory = tempdir().expect("temporary storage");
        let app_config = test_app_config(directory.path());
        let playlist_state = Arc::new(PlaylistStorageState::new());
        let target = xtream_target("force-empty-cache-success", true);
        seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
        let mut candidate = vec![
            target_group(XtreamCluster::Live, 102, "new-live"),
            target_group(XtreamCluster::Series, 302, "new-series"),
        ];

        persist_playlist_with_mode(
            &app_config,
            &mut candidate,
            None,
            &target,
            Some(&playlist_state),
            TargetPlaylistPersistOptions {
                accepted_empty_clusters: ClusterFlags::Vod,
                ..TargetPlaylistPersistOptions::default()
            },
            TargetPersistenceMode::Persist,
        )
        .await
        .expect("force-empty target persistence and cache reload");

        let (_, cached_items) = cached_target_signature(&playlist_state, &target.name).await;
        assert_eq!(cached_items.iter().filter(|(cluster, _, _)| *cluster == XtreamCluster::Live).count(), 1);
        assert!(!cached_items.iter().any(|(cluster, _, _)| *cluster == XtreamCluster::Video));
        assert_eq!(cached_items.iter().filter(|(cluster, _, _)| *cluster == XtreamCluster::Series).count(), 1);
        assert!(cached_items.iter().any(|(_, _, name)| name.as_ref() == "new-live"));
        assert!(cached_items.iter().any(|(_, _, name)| name.as_ref() == "new-series"));
    }

    #[tokio::test]
    async fn target_empty_replacement_failures_leave_the_memory_cache_unchanged() {
        for failure in [
            crate::TargetEmptyReplacementFailure::CategoryPersistence,
            crate::TargetEmptyReplacementFailure::BTreePersistence,
            crate::TargetEmptyReplacementFailure::Publication,
        ] {
            let directory = tempdir().expect("temporary storage");
            let app_config = test_app_config(directory.path());
            let playlist_state = Arc::new(PlaylistStorageState::new());
            let target = xtream_target(&format!("empty-persist-{failure:?}"), true);
            seed_memory_cached_xtream_target(&app_config, &playlist_state, &target).await;
            let before = cached_target_signature(&playlist_state, &target.name).await;
            let mut candidate = vec![
                target_group(XtreamCluster::Live, 102, "new-live"),
                target_group(XtreamCluster::Series, 302, "new-series"),
            ];

            let errors = persist_playlist_with_mode(
                &app_config,
                &mut candidate,
                None,
                &target,
                Some(&playlist_state),
                TargetPlaylistPersistOptions {
                    accepted_empty_clusters: ClusterFlags::Vod,
                    ..TargetPlaylistPersistOptions::default()
                },
                TargetPersistenceMode::FailEmptyReplacementAt(failure),
            )
            .await
            .expect_err("injected empty replacement must fail");

            assert!(errors.iter().any(|error| error.to_string().contains("target cluster failed")));
            assert_eq!(cached_target_signature(&playlist_state, &target.name).await, before);
            let persisted = load_xtream_target_storage(&app_config, &target).await.expect("retained target storage");
            assert_eq!(persisted.vod.len(), 1);
        }
    }

    #[tokio::test]
    async fn force_empty_output_filters_reject_m3u_and_strm_before_mutation() {
        let filter = get_filter(r#"EpgId ~ "^Mixed\.Case$""#, None).expect("case-sensitive EPG-ID filter");
        let cases = [
            (
                "filtered-m3u",
                TargetOutput::M3u(M3uTargetOutput {
                    filename: Some("client.m3u".to_string()),
                    include_type_in_url: false,
                    mask_redirect_url: false,
                    filter: Some(filter.clone()),
                }),
                "client.m3u",
            ),
            (
                "filtered-strm",
                TargetOutput::Strm(StrmTargetOutput {
                    directory: "client-strm".to_string(),
                    username: None,
                    style: StrmExportStyle::Jellyfin,
                    flags: StrmTargetFlagsSet::new(),
                    strm_props: None,
                    filter: Some(filter),
                    probe_probe_size_bytes: None,
                    probe_analyze_duration: None,
                }),
                "client-strm/old.strm",
            ),
        ];

        for (target_name, output, artifact) in cases {
            let directory = tempdir().expect("temporary storage");
            let app_config = test_app_config(directory.path());
            let artifact_path = directory.path().join(artifact);
            tokio::fs::create_dir_all(artifact_path.parent().expect("artifact parent"))
                .await
                .expect("artifact directory");
            tokio::fs::write(&artifact_path, b"previous-client-state").await.expect("previous client artifact");
            let mut target = target_with_options(target_name, vec![output], false);
            target.options = Some(epg_normalization_options(true));
            let target_path = {
                let config = app_config.config.load();
                crate::get_target_storage_path(&config, &target.name).expect("target storage path")
            };
            tokio::fs::create_dir_all(&target_path).await.expect("target storage directory");
            let mapping_path = crate::get_target_id_mapping_file(&target_path);
            let mapping_uuid_path = mapping_path.with_extension("uuid.db");
            let seed_group = target_group(XtreamCluster::Live, 100, "mapping-seed");
            {
                let seed_header = &seed_group.channels[0].header;
                let mut mapping = TargetIdMapping::new(&mapping_path, false).expect("seed target ID mapping");
                mapping.get_and_update_virtual_id(
                    seed_header.get_uuid(),
                    0,
                    seed_header.item_type,
                    VirtualId::default(),
                );
                mapping.persist().expect("persist target ID mapping seed");
            }
            let mapping_before = [
                tokio::fs::read(&mapping_path).await.expect("target ID mapping"),
                tokio::fs::read(&mapping_uuid_path).await.expect("target UUID mapping"),
            ];
            let mut candidate = vec![target_group(XtreamCluster::Live, 101, "filtered-after-normalization")];
            candidate[0].channels[0].header.epg_channel_id = Some("Mixed.Case".intern());

            let errors = persist_playlist_with_mode(
                &app_config,
                &mut candidate,
                None,
                &target,
                None,
                TargetPlaylistPersistOptions {
                    accepted_empty_clusters: ClusterFlags::Vod,
                    ..TargetPlaylistPersistOptions::default()
                },
                TargetPersistenceMode::Persist,
            )
            .await
            .expect_err("force-empty filtered output must be rejected");

            assert!(errors.iter().any(|error| error.to_string().contains("after its output filter")));
            assert_eq!(candidate[0].channels[0].header.epg_channel_id.as_deref(), Some("mixed.case"));
            assert_eq!(
                tokio::fs::read(&artifact_path).await.expect("retained client artifact"),
                b"previous-client-state"
            );
            assert_eq!(tokio::fs::read(&mapping_path).await.expect("retained target ID mapping"), mapping_before[0]);
            assert_eq!(
                tokio::fs::read(&mapping_uuid_path).await.expect("retained target UUID mapping"),
                mapping_before[1]
            );
        }
    }

    #[test]
    fn playlist_without_channels_is_empty_for_persistence() {
        assert!(!playlist_has_items(&[]));
        assert!(!playlist_has_items(&[PlaylistGroup {
            id: 1,
            title: "empty".intern(),
            channels: Vec::new(),
            xtream_cluster: XtreamCluster::Live,
        }]));
    }
    use tempfile::tempdir;

    #[test]
    fn media_server_playlist_file_path_uses_separate_prefix() {
        let dir = tempdir().expect("tempdir");
        let path = get_input_media_server_playlist_file_path(dir.path(), &"Media Server Input".intern());

        assert!(path.ends_with("media_server_Media_Server_Input.db"));
    }

    #[test]
    fn skipped_clusters_converts_loaded_clusters_to_exclusions() {
        let skipped = skipped_clusters(&[XtreamCluster::Live, XtreamCluster::Series]);

        assert_eq!(skipped.len(), 1);
        assert!(skipped.contains(&XtreamCluster::Video));
    }

    fn epg_normalization_options(enabled: bool) -> ConfigTargetOptions {
        ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_ids: enabled, ..EpgOutputOptions::default() },
            ..ConfigTargetOptions::default()
        }
    }

    fn epg_normalization_playlist() -> Vec<PlaylistGroup> {
        let channel = |epg_channel_id: Option<&str>, name: &str| PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: "Visible Title".intern(),
                group: "Visible Group".intern(),
                epg_channel_id: epg_channel_id.map(Internable::intern),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..PlaylistItemHeader::default()
            },
        };

        vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![
                channel(Some("Example.Channel"), "Mixed Case"),
                channel(Some("already.lower"), "Lowercase"),
                channel(Some(""), "Empty"),
                channel(None, "Missing"),
            ],
            xtream_cluster: XtreamCluster::Live,
        }]
    }

    #[test]
    fn target_playlist_epg_normalization_is_consistent_for_m3u_and_xtream() {
        let options = epg_normalization_options(true);
        let mut playlist = epg_normalization_playlist();
        let lowercase_id =
            Arc::clone(playlist[0].channels[1].header.epg_channel_id.as_ref().expect("lowercase EPG ID should exist"));

        normalize_target_playlist_epg_ids(&mut playlist, Some(&options));

        let mixed = &playlist[0].channels[0];
        assert_eq!(mixed.header.epg_channel_id.as_deref(), Some("example.channel"));
        assert_eq!(mixed.header.name.as_ref(), "Mixed Case");
        assert_eq!(mixed.header.title.as_ref(), "Visible Title");
        assert_eq!(mixed.header.group.as_ref(), "Visible Group");
        assert!(Arc::ptr_eq(
            playlist[0].channels[1].header.epg_channel_id.as_ref().expect("lowercase EPG ID should remain"),
            &lowercase_id,
        ));
        assert_eq!(playlist[0].channels[2].header.epg_channel_id.as_deref(), Some(""));
        assert!(playlist[0].channels[3].header.epg_channel_id.is_none());

        let m3u = M3uPlaylistItem::from(mixed);
        let xtream = XtreamPlaylistItem::from(mixed);
        assert_eq!(m3u.epg_channel_id.as_deref(), Some("example.channel"));
        assert_eq!(xtream.epg_channel_id.as_deref(), Some("example.channel"));
        assert!(m3u.to_m3u(None, false).contains(r#"tvg-id="example.channel""#));
    }

    #[test]
    fn target_playlist_epg_normalization_preserves_ids_when_disabled() {
        let options = epg_normalization_options(false);
        let mut playlist = epg_normalization_playlist();

        normalize_target_playlist_epg_ids(&mut playlist, Some(&options));

        assert_eq!(playlist[0].channels[0].header.epg_channel_id.as_deref(), Some("Example.Channel"));
    }

    fn make_local_series_info(series_uuid: &str, episodes: Vec<(u32, &str, &str)>) -> PlaylistItem {
        let episode_props = episodes
            .into_iter()
            .map(|(id, title, direct_source)| SeriesStreamDetailEpisodeProperties {
                id,
                episode_num: 0,
                season: 0,
                title: title.intern(),
                container_extension: "".intern(),
                custom_sid: None,
                added: "".intern(),
                direct_source: direct_source.intern(),
                tmdb: None,
                release_date: "".intern(),
                series_release_date: None,
                plot: None,
                crew: None,
                duration_secs: 0,
                duration: "".intern(),
                movie_image: "".intern(),
                bitrate: 0,
                rating: None,
                video: None,
                audio: None,
            })
            .collect();

        PlaylistItem {
            header: PlaylistItemHeader {
                id: series_uuid.intern(),
                item_type: PlaylistItemType::LocalSeriesInfo,
                xtream_cluster: XtreamCluster::Series,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: "Series".intern(),
                    details: Some(SeriesStreamDetailProperties {
                        year: None,
                        seasons: None,
                        episodes: Some(episode_props),
                    }),
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        }
    }

    fn make_local_series_episode(series_uuid: &str, direct_source: &str, virtual_id: u32) -> PlaylistItemHeader {
        PlaylistItemHeader {
            parent_code: series_uuid.intern(),
            url: direct_source.intern(),
            item_type: PlaylistItemType::LocalSeries,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(virtual_id),
            ..PlaylistItemHeader::default()
        }
    }

    fn make_media_server_episode(
        series_uuid: &str,
        item_id: &str,
        virtual_id: u32,
        season: u32,
        episode: u32,
    ) -> PlaylistItemHeader {
        PlaylistItemHeader {
            id: format!("media-server:server:shows:episode:{item_id}").intern(),
            name: format!("Episode {episode}").intern(),
            title: format!("Episode {episode}").intern(),
            parent_code: series_uuid.intern(),
            url: format!("media-server://plex/server/{item_id}?part_key=%2Flibrary%2Fparts%2Fredacted").intern(),
            item_type: PlaylistItemType::Series,
            xtream_cluster: XtreamCluster::Series,
            virtual_id: VirtualId::new(virtual_id),
            additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                episode_id: 0,
                episode,
                season,
                added: Some("1700000000".intern()),
                release_date: Some("2024-02-03".intern()),
                series_release_date: Some("2024-01-01".intern()),
                plot: Some("Episode summary".intern()),
                tmdb: Some(67890),
                movie_image:
                    "media-server://image/plex/server/episode?image_path=%2Flibrary%2Fmetadata%2Fredacted%2Fthumb"
                        .intern(),
                container_extension: "mkv".intern(),
                video: Some(r#"{"codec_name":"h264"}"#.intern()),
                audio: Some(r#"{"codec_name":"aac"}"#.intern()),
            }))),
            ..PlaylistItemHeader::default()
        }
    }

    #[test]
    // Unchanged by the move; rustfmt reflowed it past the 100-line threshold.
    #[allow(clippy::too_many_lines)]
    fn materializes_media_server_series_info_episodes_after_target_virtual_ids_are_assigned() {
        let series_uuid = "123e4567-e89b-12d3-a456-426614174111";
        let mut series_info = PlaylistItem {
            header: PlaylistItemHeader {
                uuid: UUIDType::from_valid_uuid(series_uuid),
                id: "media-server:server:shows:series:series".intern(),
                name: "Media Server Series".intern(),
                title: "Media Server Series".intern(),
                url: "media-server://unavailable/server/shows/series".intern(),
                item_type: PlaylistItemType::SeriesInfo,
                xtream_cluster: XtreamCluster::Series,
                additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                    name: "Media Server Series".intern(),
                    details: Some(SeriesStreamDetailProperties {
                        year: Some(2024),
                        seasons: Some(vec![SeriesStreamDetailSeasonProperties {
                            name: "Season 1".intern(),
                            season_number: 1,
                            episode_count: 2,
                            overview: Some("season summary".intern()),
                            air_date: Some("2024-01-01".intern()),
                            cover: None,
                            cover_tmdb: None,
                            cover_big: None,
                            duration: None,
                        }]),
                        episodes: None,
                    }),
                    ..SeriesStreamProperties::default()
                }))),
                ..PlaylistItemHeader::default()
            },
        };
        let series_parent_code = series_info.header.uuid.to_string();
        let media_episode_two =
            PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-two", 7002, 1, 2) };
        let media_episode_one =
            PlaylistItem { header: make_media_server_episode(&series_parent_code, "episode-one", 7001, 1, 1) };
        let malformed_media_episode = PlaylistItem {
            header: PlaylistItemHeader {
                id: "media-server:server:shows:episode:malformed".intern(),
                parent_code: series_parent_code.clone().intern(),
                url: "media-server://plex/server/malformed?part_key=%2Flibrary%2Fparts%2Fredacted".intern(),
                item_type: PlaylistItemType::Series,
                xtream_cluster: XtreamCluster::Series,
                virtual_id: VirtualId::new(7003),
                additional_properties: None,
                ..PlaylistItemHeader::default()
            },
        };
        let provider_episode = PlaylistItem {
            header: PlaylistItemHeader {
                id: "999".intern(),
                parent_code: series_parent_code.clone().intern(),
                url: "http://provider.example.invalid/series/999.mkv".intern(),
                item_type: PlaylistItemType::Series,
                xtream_cluster: XtreamCluster::Series,
                virtual_id: VirtualId::new(7999),
                ..PlaylistItemHeader::default()
            },
        };

        let mut media_server_series = HashMap::<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>::new();
        assign_media_server_series_info_episode(&mut media_server_series, &media_episode_two.header);
        assign_media_server_series_info_episode(&mut media_server_series, &provider_episode.header);
        assign_media_server_series_info_episode(&mut media_server_series, &malformed_media_episode.header);
        assign_media_server_series_info_episode(&mut media_server_series, &media_episode_one.header);

        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: "Media Server Series".intern(),
            channels: vec![
                series_info,
                media_episode_two,
                provider_episode,
                malformed_media_episode,
                media_episode_one,
            ],
            xtream_cluster: XtreamCluster::Series,
        }];

        materialize_media_server_series_info_episodes(&mut playlist, &media_server_series);
        series_info = playlist[0].channels[0].clone();

        let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
            panic!("missing series properties");
        };
        let details = series.details.as_ref().expect("series details should be present");
        assert_eq!(details.year, Some(2024));
        assert_eq!(details.seasons.as_ref().expect("seasons should be preserved")[0].episode_count, 2);
        let episodes = details.episodes.as_ref().expect("media-server episodes should be materialized");
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].id, 7001);
        assert_eq!(episodes[0].episode_num, 1);
        assert_eq!(episodes[0].season, 1);
        assert_eq!(episodes[0].title.as_ref(), "Episode 1");
        assert_eq!(episodes[0].container_extension.as_ref(), "mkv");
        assert_eq!(episodes[0].release_date.as_ref(), "2024-02-03");
        assert_eq!(episodes[0].series_release_date.as_deref(), Some("2024-01-01"));
        assert_eq!(episodes[0].plot.as_deref(), Some("Episode summary"));
        assert_eq!(episodes[0].tmdb, Some(67890));
        assert_eq!(episodes[0].direct_source.as_ref(), "");
        assert_eq!(
            episodes[0].movie_image.as_ref(),
            "media-server://image/plex/server/episode?image_path=%2Flibrary%2Fmetadata%2Fredacted%2Fthumb"
        );
        assert!(episodes[0].video.as_deref().is_some_and(|video| video.contains("h264")));
        assert!(episodes[0].audio.as_deref().is_some_and(|audio| audio.contains("aac")));
        assert_eq!(episodes[1].id, 7002);
        assert!(!episodes.iter().any(|episode| episode.id == 7003));
    }

    #[test]
    fn rewrite_series_episode_parent_virtual_ids_updates_local_episode_mapping() {
        let series_uuid = "123e4567-e89b-12d3-a456-426614174000";
        let series_uuid_type = UUIDType::from_valid_uuid(series_uuid);
        let episode_uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174001");
        let dir = tempdir().expect("tempdir");
        let mapping_path = dir.path().join("id_mapping.db");
        let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![
                PlaylistItem {
                    header: PlaylistItemHeader {
                        uuid: episode_uuid,
                        id: "101".intern(),
                        parent_code: series_uuid.intern(),
                        url: "/library/episode1.mkv".intern(),
                        item_type: PlaylistItemType::LocalSeries,
                        xtream_cluster: XtreamCluster::Series,
                        ..PlaylistItemHeader::default()
                    },
                },
                PlaylistItem {
                    header: PlaylistItemHeader {
                        uuid: series_uuid_type,
                        id: series_uuid.intern(),
                        item_type: PlaylistItemType::LocalSeriesInfo,
                        xtream_cluster: XtreamCluster::Series,
                        ..PlaylistItemHeader::default()
                    },
                },
            ],
            xtream_cluster: XtreamCluster::Series,
        }];

        for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
            let uuid = channel.header.uuid;
            let provider_id = channel.header.get_provider_id().unwrap_or_default();
            let item_type = channel.header.item_type;
            channel.header.virtual_id =
                target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
            channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
        }

        let series_virtual_id = playlist[0].channels[1].header.virtual_id;
        let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

        rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
        target_id_mapping.persist().expect("persist");

        let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
        let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

        assert_eq!(record.parent_virtual_id, series_virtual_id);
        assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
    }

    #[test]
    fn rewrite_series_episode_parent_virtual_ids_updates_provider_episode_mapping_using_series_info_uuid() {
        let dir = tempdir().expect("tempdir");
        let mapping_path = dir.path().join("id_mapping.db");
        let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

        let input_name = "provider-input".intern();
        let xtream_series_info = XtreamPlaylistItem {
            virtual_id: VirtualId::new(0),
            provider_id: 9001,
            name: "Provider Series".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Provider Series".intern(),
            title: "Provider Series".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example.com/series/user/pass/9001".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: None,
            item_type: PlaylistItemType::SeriesInfo,
            category_id: 0,
            input_name: Arc::clone(&input_name),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "9001".intern(),
            upstream_user_agent: None,
        };
        let provider_parent_code = xtream_series_info.get_uuid().intern();
        let xtream_provider_episode = XtreamPlaylistItem {
            virtual_id: VirtualId::new(0),
            provider_id: 201,
            name: "Episode 1".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "Provider Series".intern(),
            title: "Episode 1".intern(),
            parent_code: provider_parent_code,
            rec: "".intern(),
            url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: None,
            item_type: PlaylistItemType::Series,
            category_id: 0,
            input_name,
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "201".intern(),
            upstream_user_agent: None,
        };
        let provider_episode = PlaylistItem::from(&xtream_provider_episode);
        let mut series_info = PlaylistItem::from(&xtream_series_info);
        series_info.header.uuid = UUIDType::from_valid_uuid("123e4567-e89b-12d3-a456-426614174099");

        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: "Provider Series".intern(),
            channels: vec![provider_episode, series_info],
            xtream_cluster: XtreamCluster::Series,
        }];

        for (idx, channel) in playlist[0].channels.iter_mut().enumerate() {
            let uuid = channel.get_uuid();
            let provider_id = channel.header.get_provider_id().unwrap_or_default();
            let item_type = channel.header.item_type;
            channel.header.virtual_id =
                target_id_mapping.get_and_update_virtual_id(&uuid, provider_id, item_type, VirtualId::new(0));
            channel.header.source_ordinal = u32::try_from(idx + 1).expect("ordinal");
        }

        let series_virtual_id = playlist[0].channels[1].header.virtual_id;
        let episode_virtual_id = playlist[0].channels[0].header.virtual_id;

        rewrite_series_episode_parent_virtual_ids(&mut playlist, &mut target_id_mapping);
        target_id_mapping.persist().expect("persist");

        let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
        let record = query.query_zero_copy(&episode_virtual_id.get()).expect("query ok").expect("record missing");

        assert_eq!(record.parent_virtual_id, series_virtual_id);
        assert_eq!(playlist[0].channels[0].header.virtual_id, episode_virtual_id);
    }

    #[test]
    fn initial_virtual_id_assignment_preserves_existing_parent_for_series_episode_without_parent_match() {
        let dir = tempdir().expect("tempdir");
        let mapping_path = dir.path().join("id_mapping.db");
        let mut target_id_mapping = TargetIdMapping::new(&mapping_path, false).expect("mapping");

        let input_name = "provider-input".intern();
        let mut episode = PlaylistItem {
            header: PlaylistItemHeader {
                id: "201".intern(),
                url: "http://provider.example.com/series/user/pass/201.mkv".intern(),
                input_name,
                item_type: PlaylistItemType::Series,
                xtream_cluster: XtreamCluster::Series,
                ..PlaylistItemHeader::default()
            },
        };

        let provider_id = episode.header.get_provider_id().unwrap_or_default();
        let uuid = *episode.header.get_uuid();
        let original_virtual_id = target_id_mapping.get_and_update_virtual_id(
            &uuid,
            provider_id,
            episode.header.item_type,
            VirtualId::new(77),
        );

        let preserved_parent_virtual_id = target_id_mapping.get_parent_virtual_id_by_uuid(&uuid).unwrap_or_default();
        episode.header.virtual_id = target_id_mapping.get_and_update_virtual_id(
            &uuid,
            provider_id,
            episode.header.item_type,
            preserved_parent_virtual_id,
        );
        target_id_mapping.persist().expect("persist");

        let mut query = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&mapping_path).expect("query");
        let record = query.query_zero_copy(&original_virtual_id.get()).expect("query ok").expect("record missing");

        assert_eq!(record.parent_virtual_id, VirtualId::new(77));
        assert_eq!(episode.header.virtual_id, original_virtual_id);
    }

    #[test]
    fn rewrite_local_series_info_uses_series_uuid_lookup_and_updates_episode_virtual_ids() {
        let series_uuid = "series-uuid";
        let mut series_info = make_local_series_info(
            series_uuid,
            vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
        );
        let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
        local_library_series.insert(
            series_uuid.intern(),
            vec![
                LocalEpisodeKey { path: "/library/episode1.mkv".intern(), virtual_id: 7001 },
                LocalEpisodeKey { path: "/library/episode2.mkv".intern(), virtual_id: 7002 },
            ],
        );

        rewrite_local_series_info_episode_virtual_id(&mut series_info, &local_library_series);

        let Some(StreamProperties::Series(series)) = series_info.header.additional_properties.as_ref() else {
            panic!("missing series properties");
        };
        let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
        assert_eq!(episodes[0].id, 7001);
        assert_eq!(episodes[1].id, 7002);
    }

    #[test]
    fn rewrite_series_info_updates_local_episode_ids_before_parent_code_is_cleared() {
        let series_uuid = "series-uuid";
        let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
        let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

        let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
        assign_local_series_info_episode_key(
            &mut local_library_series,
            &mut episode_one,
            PlaylistItemType::LocalSeries,
        );
        assign_local_series_info_episode_key(
            &mut local_library_series,
            &mut episode_two,
            PlaylistItemType::LocalSeries,
        );

        let series_info = make_local_series_info(
            series_uuid,
            vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
        );
        let local_episode_one = PlaylistItem { header: episode_one };
        let local_episode_two = PlaylistItem { header: episode_two };
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![series_info, local_episode_one, local_episode_two],
            xtream_cluster: XtreamCluster::Series,
        }];

        rewrite_series_info_episode_virtual_id(
            &mut playlist,
            &local_library_series,
            &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
        );

        let Some(StreamProperties::Series(series)) = playlist[0].channels[0].header.additional_properties.as_ref()
        else {
            panic!("missing series properties");
        };
        let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
        assert_eq!(episodes[0].id, 7001);
        assert_eq!(episodes[1].id, 7002);
        assert!(playlist[0].channels[1].header.parent_code.is_empty());
        assert!(playlist[0].channels[2].header.parent_code.is_empty());
    }

    #[test]
    fn rewrite_series_info_updates_local_episode_ids_when_episodes_come_first() {
        // Test with episodes BEFORE series_info to verify iteration-order doesn't matter
        let series_uuid = "series-uuid";
        let mut episode_one = make_local_series_episode(series_uuid, "/library/episode1.mkv", 7001);
        let mut episode_two = make_local_series_episode(series_uuid, "/library/episode2.mkv", 7002);

        let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
        assign_local_series_info_episode_key(
            &mut local_library_series,
            &mut episode_one,
            PlaylistItemType::LocalSeries,
        );
        assign_local_series_info_episode_key(
            &mut local_library_series,
            &mut episode_two,
            PlaylistItemType::LocalSeries,
        );

        let series_info = make_local_series_info(
            series_uuid,
            vec![(101, "Episode 1", "/library/episode1.mkv"), (202, "Episode 2", "/library/episode2.mkv")],
        );
        let local_episode_one = PlaylistItem { header: episode_one };
        let local_episode_two = PlaylistItem { header: episode_two };

        // Episodes FIRST, then series_info (reversed order)
        let mut playlist = vec![PlaylistGroup {
            id: 1,
            title: "Series".intern(),
            channels: vec![local_episode_one, local_episode_two, series_info],
            xtream_cluster: XtreamCluster::Series,
        }];

        rewrite_series_info_episode_virtual_id(
            &mut playlist,
            &local_library_series,
            &HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new(),
        );

        let Some(StreamProperties::Series(series)) = playlist[0].channels[2].header.additional_properties.as_ref()
        else {
            panic!("missing series properties");
        };
        let episodes = series.details.as_ref().and_then(|details| details.episodes.as_ref()).expect("missing episodes");
        assert_eq!(episodes[0].id, 7001);
        assert_eq!(episodes[1].id, 7002);
        assert!(playlist[0].channels[0].header.parent_code.is_empty());
        assert!(playlist[0].channels[1].header.parent_code.is_empty());
    }
}
