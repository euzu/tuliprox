use super::playlist_mem_cache::{PlaylistM3uStorage, PlaylistStorage, PlaylistStorageState, PlaylistXtreamStorage};
use crate::{
    ensure_target_storage_path, epg_write_for_target, get_input_storage_path, get_target_id_mapping_file,
    get_target_storage_path, load_input_local_library_playlist, load_input_m3u_playlist, load_input_xtream_playlist,
    m3u_get_file_path_for_db, m3u_write_playlist, persist_input_library_playlist, persist_input_m3u_playlist,
    persist_input_xtream_playlist, stalker_repository::get_stalker_storage_path, write_strm_playlist,
    xtream_get_file_path, xtream_get_storage_path, xtream_write_playlist, BPlusTree, LocalLibraryDiskPlaylistSource,
    M3uDiskPlaylistSource, MediaServerDiskPlaylistSource, MemoryPlaylistSource, PlaylistSource,
    StalkerDiskPlaylistSource, TargetIdMapping, VirtualIdRecord, XtreamDiskPlaylistSource, FILE_SUFFIX_DB,
};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{
        xtream_const::XTREAM_CLUSTER, ConfigTargetOptions, InputPersistence, M3uPlaylistItem, PlaylistEntry,
        PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamDetailEpisodeProperties,
        SeriesStreamDetailProperties, StreamProperties, UUIDType, VirtualId, XtreamCluster, XtreamPlaylistItem,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistPublicationPlan {
    Ordinary,
    CompleteCuration {
        intentionally_empty_base_vod: bool,
        intentionally_empty_base_series: bool,
        intentionally_empty_xtream_vod: bool,
        intentionally_empty_xtream_series: bool,
    },
}

impl PlaylistPublicationPlan {
    pub const fn complete_curation(curated_catalog: bool, xtream_base_suppressed: bool) -> Self {
        Self::complete_curation_with_filter(curated_catalog, xtream_base_suppressed, false)
    }

    pub const fn complete_curation_with_filter(
        curated_catalog: bool,
        xtream_base_suppressed: bool,
        appearance_filter_configured: bool,
    ) -> Self {
        Self::CompleteCuration {
            intentionally_empty_base_vod: curated_catalog || appearance_filter_configured,
            intentionally_empty_base_series: curated_catalog || appearance_filter_configured,
            intentionally_empty_xtream_vod: curated_catalog || xtream_base_suppressed || appearance_filter_configured,
            intentionally_empty_xtream_series: curated_catalog
                || xtream_base_suppressed
                || appearance_filter_configured,
        }
    }

    pub const fn with_output_filter(self, output_filter_configured: bool) -> Self {
        if !output_filter_configured {
            return self;
        }
        match self {
            Self::Ordinary => Self::Ordinary,
            Self::CompleteCuration { .. } => Self::CompleteCuration {
                intentionally_empty_base_vod: true,
                intentionally_empty_base_series: true,
                intentionally_empty_xtream_vod: true,
                intentionally_empty_xtream_series: true,
            },
        }
    }

    pub const fn allows_empty_xtream_cluster(self, cluster: XtreamCluster) -> bool {
        matches!(
            (self, cluster),
            (Self::CompleteCuration { intentionally_empty_xtream_vod: true, .. }, XtreamCluster::Video)
                | (Self::CompleteCuration { intentionally_empty_xtream_series: true, .. }, XtreamCluster::Series)
        )
    }

    pub const fn allows_empty_base_output(self) -> bool {
        matches!(
            self,
            Self::CompleteCuration { intentionally_empty_base_vod: true, .. }
                | Self::CompleteCuration { intentionally_empty_base_series: true, .. }
        )
    }

    pub const fn allows_any_empty_output(self) -> bool {
        self.allows_empty_base_output()
            || self.allows_empty_xtream_cluster(XtreamCluster::Video)
            || self.allows_empty_xtream_cluster(XtreamCluster::Series)
    }
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

fn prepare_target_playlist_for_persistence(
    playlist: &mut [PlaylistGroup],
    target: &ConfigTarget,
    target_id_mapping: &mut TargetIdMapping,
) {
    let mut source_ordinal: u32 = 0;
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

    rewrite_series_episode_parent_virtual_ids(playlist, target_id_mapping);

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    let mut provider_series = HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new();
    let mut media_server_series = HashMap::<Arc<str>, Vec<SeriesStreamDetailEpisodeProperties>>::new();
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
    normalize_target_playlist_epg_ids(playlist, target.options.as_ref());
}

#[allow(clippy::too_many_lines)]
pub async fn persist_playlist(
    app_config: &Arc<AppConfig>,
    base_playlist: &mut [PlaylistGroup],
    mut xtream_playlist: Option<&mut [PlaylistGroup]>,
    epg: Option<&Epg>,
    target: &ConfigTarget,
    playlist_state: Option<&Arc<PlaylistStorageState>>,
    publication_plan: PlaylistPublicationPlan,
) -> Result<(), Vec<TuliproxError>> {
    let has_items = playlist_has_items(base_playlist) || xtream_playlist.as_deref().is_some_and(playlist_has_items);
    if !has_items && !publication_plan.allows_any_empty_output() {
        return Err(vec![TuliproxError::RepositoryPlaylist(format!(
            "Refusing to persist empty playlist for target '{}'; existing data was retained",
            target.name
        ))]);
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

    for output in &target.output {
        let output_publication_plan = publication_plan.with_output_filter(output.filter().is_some());
        let source_playlist = match output {
            TargetOutput::Xtream(_) => xtream_playlist.as_deref_mut().unwrap_or(base_playlist),
            _ => &mut *base_playlist,
        };
        let mut filtered: Option<Vec<PlaylistGroup>> =
            output.filter().map(|flt| apply_filter_to_playlist(source_playlist, flt));

        let pl: &mut [PlaylistGroup] = if let Some(filtered_playlist) = filtered.as_mut() {
            filtered_playlist.as_mut_slice()
        } else {
            source_playlist
        };

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => {
                xtream_write_playlist(app_config, target, pl, output_publication_plan).await
            }
            TargetOutput::M3u(m3u_output) => {
                m3u_write_playlist(
                    app_config,
                    target,
                    m3u_output,
                    &target_path,
                    pl,
                    output_publication_plan.allows_empty_base_output(),
                )
                .await
            }
            TargetOutput::Strm(strm_output) => {
                write_strm_playlist(
                    app_config,
                    target,
                    strm_output,
                    pl,
                    output_publication_plan.allows_empty_base_output(),
                )
                .await
            }
            TargetOutput::HdHomeRun(_hdhomerun_output) => Ok(()),
        };

        match result {
            Ok(()) => {
                let allows_empty_output = match output {
                    TargetOutput::Xtream(_) => {
                        output_publication_plan.allows_empty_xtream_cluster(XtreamCluster::Video)
                            || output_publication_plan.allows_empty_xtream_cluster(XtreamCluster::Series)
                    }
                    _ => output_publication_plan.allows_empty_base_output(),
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

    if target.use_memory_cache {
        if let Some(playlist_storage) = playlist_state {
            if let Ok(id_mapping) = load_id_mapping_target_storage(app_config, target).await {
                playlist_storage.cache_id_mapping(&target.name, id_mapping).await;
            }
            for output in &target.output {
                match output {
                    TargetOutput::Xtream(_) => {
                        if let Ok(storage) = load_xtream_target_storage(app_config, target).await {
                            playlist_storage
                                .cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage)))
                                .await;
                        }
                    }
                    TargetOutput::M3u(_) => {
                        if let Ok(storage) = load_m3u_target_storage(app_config, target).await {
                            playlist_storage
                                .cache_playlist(&target.name, PlaylistStorage::M3uPlaylist(Box::new(storage)))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
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
    mut playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    if !playlist_has_items(&playlist) {
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
    if input.get_download_input_type().persistence() == InputPersistence::Stalker {
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

    let (persisted_playlist, err) = match input.get_download_input_type().persistence() {
        InputPersistence::Xtream => persist_input_xtream_playlist(app_config, &storage_path, playlist).await,

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
        get_input_media_server_playlist_file_path, load_m3u_target_storage, load_xtream_target_storage,
        materialize_media_server_series_info_episodes, normalize_target_playlist_epg_ids, persist_playlist,
        playlist_has_items, rewrite_local_series_info_episode_virtual_id, rewrite_series_episode_parent_virtual_ids,
        rewrite_series_info_episode_virtual_id, skipped_clusters, LocalEpisodeKey, PlaylistPublicationPlan,
        ProviderEpisodeKey,
    };
    use crate::{
        get_series_cat_collection_path, get_target_storage_path, get_vod_cat_collection_path, strm_get_file_paths,
        xtream_get_storage_path, BPlusTreeQuery, TargetIdMapping, VirtualIdRecord,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{
            ConfigPaths, ConfigTargetDto, ConfigTargetOptions, EpgOutputOptions, EpisodeStreamProperties,
            M3uPlaylistItem, M3uTargetOutputDto, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
            PlaylistItemType, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
            SeriesStreamDetailSeasonProperties, SeriesStreamProperties, StreamProperties, StrmTargetOutputDto,
            TargetOutputDto, UUIDType, VirtualId, XtreamCluster, XtreamPlaylistItem, XtreamTargetOutputDto,
        },
        utils::{hash_string_as_hex, Internable},
    };
    use std::{collections::HashMap, path::Path, sync::Arc};

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
    use tuliprox_core::{
        model::{
            ApiProxyConfig, AppConfig, Config, ConfigTarget, CustomStreamResponse, HdHomeRunConfig,
            MediaToolCapabilities, SourcesConfig,
        },
        utils::{normalize_string_path, FileLockManager},
    };

    #[test]
    fn complete_curation_empty_authorization_is_projection_and_cluster_scoped() {
        let compatibility = PlaylistPublicationPlan::complete_curation(false, false);
        assert!(!compatibility.allows_any_empty_output());

        let suppressed_xtream_base = PlaylistPublicationPlan::complete_curation(false, true);
        assert!(!suppressed_xtream_base.allows_empty_base_output());
        assert!(!suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Live));
        assert!(suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Video));
        assert!(suppressed_xtream_base.allows_empty_xtream_cluster(XtreamCluster::Series));

        let persist_filtered = PlaylistPublicationPlan::complete_curation_with_filter(false, false, true);
        assert!(persist_filtered.allows_empty_base_output());
        assert!(persist_filtered.allows_empty_xtream_cluster(XtreamCluster::Video));
        let output_filtered = compatibility.with_output_filter(true);
        assert!(output_filtered.allows_empty_base_output());
        assert_eq!(PlaylistPublicationPlan::Ordinary.with_output_filter(true), PlaylistPublicationPlan::Ordinary);
    }

    fn target_test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
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

    fn mixed_output_target() -> ConfigTarget {
        let mut xtream = XtreamTargetOutputDto::default();
        xtream.t_filter =
            Some(shared::foundation::get_filter(r#"Group = "Curated alias""#, None).expect("Xtream output filter"));
        let mut m3u = M3uTargetOutputDto { filename: Some("curated.m3u".to_string()), ..Default::default() };
        m3u.t_filter =
            Some(shared::foundation::get_filter(r#"Group = "Base movie""#, None).expect("M3U output filter"));
        ConfigTarget::from(&ConfigTargetDto {
            name: "curated-output-test".to_string(),
            output: vec![
                TargetOutputDto::Xtream(xtream),
                TargetOutputDto::M3u(m3u),
                TargetOutputDto::Strm(StrmTargetOutputDto {
                    directory: "strm".to_string(),
                    flat: true,
                    cleanup: true,
                    ..StrmTargetOutputDto::default()
                }),
            ],
            use_memory_cache: true,
            ..ConfigTargetDto::default()
        })
    }

    fn target_video_group(title: &str, uuid: UUIDType) -> PlaylistGroup {
        PlaylistGroup {
            id: 1,
            title: title.intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "1".intern(),
                    name: title.intern(),
                    title: title.intern(),
                    group: title.intern(),
                    url: format!("http://example.invalid/{title}").intern(),
                    uuid,
                    item_type: PlaylistItemType::Video,
                    xtream_cluster: XtreamCluster::Video,
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Video,
        }
    }

    fn strm_files_below(path: &Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = std::fs::read_dir(path) else { return files };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                files.extend(strm_files_below(&entry_path));
            } else if entry_path.extension().is_some_and(|extension| extension == "strm") {
                files.push(entry_path);
            }
        }
        files
    }

    #[tokio::test]
    async fn intentional_empty_output_views_clear_managed_artifacts_without_alias_leakage() {
        let directory = tempdir().expect("tempdir");
        let app_config = target_test_app_config(directory.path());
        let target = mixed_output_target();
        let playlist_state = Arc::new(crate::PlaylistStorageState::new());
        let mut standard =
            vec![target_video_group("Base movie", UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000021"))];
        let mut xtream = vec![target_video_group(
            "Curated alias",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000022"),
        )];

        let first = persist_playlist(
            &app_config,
            &mut standard,
            Some(&mut xtream),
            None,
            &target,
            Some(&playlist_state),
            PlaylistPublicationPlan::complete_curation(true, false),
        )
        .await;
        assert!(first.is_ok(), "initial mixed-output persist failed: {first:?}");

        let m3u = load_m3u_target_storage(&app_config, &target).await.expect("M3U storage");
        let xtream = load_xtream_target_storage(&app_config, &target).await.expect("Xtream storage");
        assert_eq!(m3u.len(), 1);
        assert_eq!(m3u.iter().next().expect("M3U item").1.title.as_ref(), "Base movie");
        assert_eq!(xtream.vod.len(), 1);
        let xtream_item = xtream.vod.iter().next().expect("Xtream item").1;
        assert_eq!(xtream_item.title.as_ref(), "Curated alias");
        assert_ne!(xtream_item.category_id, 0, "base-suppressed unfiltered rows remain category-backed aliases");
        {
            let cache = playlist_state.data.read().await;
            let cached = cache.get(&target.name).expect("target cache");
            assert_eq!(cached.m3u.as_ref().expect("M3U cache").len(), 1);
            assert_eq!(cached.xtream.as_ref().expect("Xtream cache").vod.len(), 1);
        }
        let m3u_text_path = directory.path().join("curated.m3u");
        assert!(std::fs::read_to_string(&m3u_text_path).expect("M3U text").contains("Base movie"));
        let strm_root = directory.path().join("strm");
        let strm_files = strm_files_below(&strm_root);
        assert_eq!(strm_files.len(), 1);
        assert!(std::fs::read_to_string(&strm_files[0]).expect("STRM content").contains("Base movie"));

        let mut empty_standard = Vec::new();
        let mut empty_xtream = Vec::new();
        let retained = persist_playlist(
            &app_config,
            &mut empty_standard,
            Some(&mut empty_xtream),
            None,
            &target,
            Some(&playlist_state),
            PlaylistPublicationPlan::Ordinary,
        )
        .await;
        assert!(retained.is_err(), "untrusted empty input must retain the published snapshot");
        assert_eq!(load_m3u_target_storage(&app_config, &target).await.expect("retained M3U").len(), 1);
        assert_eq!(load_xtream_target_storage(&app_config, &target).await.expect("retained Xtream").vod.len(), 1);
        {
            let cache = playlist_state.data.read().await;
            let cached = cache.get(&target.name).expect("retained target cache");
            assert_eq!(cached.m3u.as_ref().expect("retained M3U cache").len(), 1);
            assert_eq!(cached.xtream.as_ref().expect("retained Xtream cache").vod.len(), 1);
        }
        assert_eq!(strm_files_below(&strm_root).len(), 1, "untrusted empty refresh must retain STRM files");

        let published = persist_playlist(
            &app_config,
            &mut empty_standard,
            Some(&mut empty_xtream),
            None,
            &target,
            Some(&playlist_state),
            PlaylistPublicationPlan::complete_curation(true, false),
        )
        .await;
        assert!(published.is_ok(), "trusted empty snapshot failed: {published:?}");
        assert!(load_m3u_target_storage(&app_config, &target).await.expect("empty M3U").is_empty());
        assert_eq!(std::fs::read_to_string(&m3u_text_path).expect("empty M3U text"), "#EXTM3U\n");
        let empty_xtream = load_xtream_target_storage(&app_config, &target).await.expect("empty Xtream");
        assert!(empty_xtream.live.is_empty());
        assert!(empty_xtream.vod.is_empty());
        assert!(empty_xtream.series.is_empty());
        let target_storage = {
            let config = app_config.config.load();
            get_target_storage_path(&config, &target.name).expect("target storage")
        };
        let xtream_storage = {
            let config = app_config.config.load();
            xtream_get_storage_path(&config, &target.name).expect("Xtream storage path")
        };
        assert_eq!(
            std::fs::read_to_string(get_vod_cat_collection_path(&xtream_storage)).expect("empty VOD categories"),
            "[]"
        );
        assert_eq!(
            std::fs::read_to_string(get_series_cat_collection_path(&xtream_storage)).expect("empty series categories"),
            "[]"
        );
        let strm_index = strm_get_file_paths(&hash_string_as_hex(&normalize_string_path("strm")), &target_storage);
        assert!(std::fs::read_to_string(strm_index).expect("empty STRM index").is_empty());
        {
            let cache = playlist_state.data.read().await;
            let cached = cache.get(&target.name).expect("empty target cache");
            assert!(cached.m3u.as_ref().expect("empty M3U cache").is_empty());
            assert!(cached.xtream.as_ref().expect("empty Xtream cache").vod.is_empty());
        }
        assert!(strm_files_below(&strm_root).is_empty(), "trusted empty refresh must clean stale STRM files");
    }

    #[tokio::test]
    async fn intentional_empty_strm_cleanup_false_removes_indexed_files_but_keeps_unmanaged_files() {
        let directory = tempdir().expect("tempdir");
        let app_config = target_test_app_config(directory.path());
        let target = ConfigTarget::from(&ConfigTargetDto {
            name: "curated-strm-retention-test".to_string(),
            output: vec![TargetOutputDto::Strm(StrmTargetOutputDto {
                directory: "strm-retained".to_string(),
                flat: true,
                cleanup: false,
                ..StrmTargetOutputDto::default()
            })],
            ..ConfigTargetDto::default()
        });
        let mut seeded = vec![target_video_group(
            "Retained movie",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000029"),
        )];
        let initial =
            persist_playlist(&app_config, &mut seeded, None, None, &target, None, PlaylistPublicationPlan::Ordinary)
                .await;
        assert!(initial.is_ok(), "initial STRM persist failed: {initial:?}");
        let strm_root = directory.path().join("strm-retained");
        let managed_files = strm_files_below(&strm_root);
        assert_eq!(managed_files.len(), 1);
        let managed_file = managed_files[0].clone();
        let unmanaged_file = strm_root.join("unmanaged.strm");
        std::fs::write(&unmanaged_file, "unmanaged").expect("unmanaged STRM fixture");

        let mut empty = Vec::new();
        let published = persist_playlist(
            &app_config,
            &mut empty,
            None,
            None,
            &target,
            None,
            PlaylistPublicationPlan::complete_curation(true, false),
        )
        .await;
        assert!(published.is_ok(), "empty STRM persist failed: {published:?}");

        assert!(!managed_file.exists(), "cleanup=false removes files tracked by the managed index");
        assert!(unmanaged_file.exists(), "cleanup=false does not scan and remove unmanaged files");
        let target_storage = {
            let config = app_config.config.load();
            get_target_storage_path(&config, &target.name).expect("target storage")
        };
        let index = strm_get_file_paths(&hash_string_as_hex(&normalize_string_path("strm-retained")), &target_storage);
        assert!(std::fs::read_to_string(index).expect("empty STRM index").is_empty());
    }

    #[tokio::test]
    async fn intentional_empty_curation_never_authorizes_empty_live_cluster_replacement() {
        let directory = tempdir().expect("tempdir");
        let app_config = target_test_app_config(directory.path());
        let target = ConfigTarget::from(&ConfigTargetDto {
            name: "curated-live-retention-test".to_string(),
            output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto::default())],
            ..ConfigTargetDto::default()
        });
        let mut live = vec![
            PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![PlaylistItem {
                    header: PlaylistItemHeader {
                        id: "1".intern(),
                        name: "Live channel".intern(),
                        title: "Live channel".intern(),
                        group: "Live".intern(),
                        url: "http://example.invalid/live".intern(),
                        uuid: UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000031"),
                        item_type: PlaylistItemType::Live,
                        xtream_cluster: XtreamCluster::Live,
                        ..PlaylistItemHeader::default()
                    },
                }],
                xtream_cluster: XtreamCluster::Live,
            },
            target_video_group(
                "Previously published movie",
                UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000032"),
            ),
        ];

        let initial =
            persist_playlist(&app_config, &mut live, None, None, &target, None, PlaylistPublicationPlan::Ordinary)
                .await;
        assert!(initial.is_ok(), "initial Live persist failed: {initial:?}");

        let mut ordinary_live_only = vec![live[0].clone()];
        let ordinary = persist_playlist(
            &app_config,
            &mut ordinary_live_only,
            None,
            None,
            &target,
            None,
            PlaylistPublicationPlan::Ordinary,
        )
        .await;
        assert!(ordinary.is_ok(), "ordinary Live-only persist failed: {ordinary:?}");
        let ordinary_storage = load_xtream_target_storage(&app_config, &target).await.expect("ordinary storage");
        assert_eq!(ordinary_storage.live.len(), 1);
        assert_eq!(ordinary_storage.vod.len(), 1, "ordinary refresh retains an absent cluster");

        let mut empty_standard = Vec::new();
        let mut empty_xtream = Vec::new();
        let curated_empty = persist_playlist(
            &app_config,
            &mut empty_standard,
            Some(&mut empty_xtream),
            None,
            &target,
            None,
            PlaylistPublicationPlan::complete_curation(true, false),
        )
        .await;
        assert!(curated_empty.is_ok(), "curated empty persist failed: {curated_empty:?}");

        let storage = load_xtream_target_storage(&app_config, &target).await.expect("Xtream storage");
        assert_eq!(storage.live.len(), 1);
        assert!(storage.vod.is_empty());
        assert!(storage.series.is_empty());
    }

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
