use super::LocalEpisodeKey;
use crate::TargetIdMapping;
use shared::{
    model::{
        ConfigTargetOptions, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
        SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties, StreamProperties, UUIDType, VirtualId,
    },
    utils::{is_dash_url, is_hls_url, Internable},
};
use std::{collections::HashMap, sync::Arc};
use tuliprox_core::{model::ConfigTarget, utils::fold_epg_id_arc};

pub struct ProviderEpisodeKey {
    pub provider_id: u32,
    pub virtual_id: u32,
}

pub(super) fn normalize_target_playlist_epg_ids(
    playlist: &mut [PlaylistGroup],
    target_options: Option<&ConfigTargetOptions>,
) {
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

pub(super) fn prepare_target_playlist_for_persistence(
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

pub(super) fn assign_local_series_info_episode_key(
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

pub(super) fn assign_media_server_series_info_episode(
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
pub(super) fn materialize_media_server_series_info_episodes(
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
pub(super) fn rewrite_local_series_info_episode_virtual_id(
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

pub(super) fn rewrite_series_info_episode_virtual_id(
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

pub(super) fn rewrite_series_episode_parent_virtual_ids(
    playlist: &mut [PlaylistGroup],
    target_id_mapping: &mut TargetIdMapping,
) {
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
