use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget, ConfigInput};
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::playlist::ProcessingPipe;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::provider_source::ProviderPlaylistSource;
use crate::repository::storage::get_input_storage_path;
use crate::repository::xtream_repository::persists_input_series_info;
use log::{error, info, log_enabled, Level};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, SeriesStreamProperties, StreamProperties, XtreamSeriesInfo};
use std::sync::Arc;
use std::time::Instant;
use shared::utils::StringInterner;
use indexmap::IndexMap;
use shared::model::{PlaylistGroup, PlaylistItemType, XtreamCluster, PlaylistItem};

create_resolve_options_function_for_xtream_target!(series);

#[allow(clippy::too_many_lines)]
async fn playlist_resolve_series_info(app_config: &Arc<AppConfig>, client: &reqwest::Client,
                                      errors: &mut Vec<TuliproxError>,
                                      fpl: &mut FetchedPlaylist<'_>,
                                      resolve_series: bool,
                                      resolve_delay: u16) -> Vec<PlaylistGroup> {
    let input = fpl.input;
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir) {
        Ok(storage_path) => storage_path,
        Err(err) => {
            error!("Can't resolve vod, input storage directory for input '{}' failed: {err}", input.name);
            return vec![];
        }
    };

    let series_info_count = if resolve_series {
        match &mut fpl.source {
            ProviderPlaylistSource::Memory(groups) => {
                groups.iter()
                    .flat_map(|plg| &plg.channels)
                    .filter(|&pli| pli.header.xtream_cluster == XtreamCluster::Series
                        && pli.header.item_type == PlaylistItemType::SeriesInfo
                        && pli.get_provider_id().is_some_and(|id| id > 0)
                        && !pli.has_details()).count()
            }
            ProviderPlaylistSource::XtreamDisk { series, .. } => {
                let mut count = 0;
                if let Some(query) = series.as_mut() {
                    for (_, item) in query.iter() {
                        if item.item_type == PlaylistItemType::SeriesInfo && item.provider_id > 0 && !item.has_details() {
                            count += 1;
                        }
                    }
                }
                count
            }
            ProviderPlaylistSource::M3uDisk { .. } => 0,
        }
    } else {
        0
    };

    if series_info_count > 0 {
        info!("Found {series_info_count} series info to resolve");
    }

    let mut last_log_time = Instant::now();
    let mut processed_series_info_count = 0;
    let mut result: Vec<PlaylistGroup> = vec![];
    let mut interner = StringInterner::new();

    match &mut fpl.source {
        ProviderPlaylistSource::Memory(groups) => {
            for plg in groups.iter_mut() {
                let mut group_series = vec![];
                for pli in &mut plg.channels {
                    if let Some(episodes) = process_xtream_series_item(
                        app_config, client, input, &storage_path, pli, errors, &mut interner,
                        resolve_series, resolve_delay, None,
                        &mut processed_series_info_count, series_info_count, &mut last_log_time
                    ).await {
                        group_series.extend(episodes);
                    }
                }
                if !group_series.is_empty() {
                    result.push(PlaylistGroup {
                        id: plg.id,
                        title: plg.title.clone(),
                        channels: group_series,
                        xtream_cluster: XtreamCluster::Series,
                    });
                }
            }
        }
        ProviderPlaylistSource::XtreamDisk { series, .. } => {
            if let Some(query) = series.as_mut() {
                let mut group_episodes: IndexMap<Arc<str>, Vec<PlaylistItem>> = IndexMap::new();
                let mut updates = Vec::new();

                for (_, item) in query.iter() {
                    let mut pli = PlaylistItem::from(&item);
                    if let Some(episodes) = process_xtream_series_item(
                        app_config, client, input, &storage_path, &mut pli, errors, &mut interner,
                        resolve_series, resolve_delay, Some(&mut updates),
                        &mut processed_series_info_count, series_info_count, &mut last_log_time
                    ).await {
                       group_episodes.entry(Arc::clone(&pli.header.group)).or_default().extend(episodes);
                    }
                }

                // Batch persist series info updates
                for (provider_id, props) in updates {
                    let _ = persists_input_series_info(app_config, &storage_path, XtreamCluster::Series, &input.name, provider_id, &props).await;
                }

                for (group_id, (title, channels)) in group_episodes.into_iter().enumerate() {
                    result.push(PlaylistGroup {
                        id: u32::try_from(group_id).unwrap_or(0) + 1,
                        title: title.to_string(),
                        channels,
                        xtream_cluster: XtreamCluster::Series,
                    });
                }
            }
        }
        ProviderPlaylistSource::M3uDisk { .. } => {}
    }

    if resolve_series {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn process_xtream_series_item(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    storage_path: &std::path::Path,
    pli: &mut PlaylistItem,
    errors: &mut Vec<TuliproxError>,
    interner: &mut StringInterner,
    resolve_series: bool,
    resolve_delay: u16,
    updates: Option<&mut Vec<(u32, SeriesStreamProperties)>>,
    processed_series_info_count: &mut usize,
    series_info_count: usize,
    last_log_time: &mut Instant,
) -> Option<Vec<PlaylistItem>> {
    if pli.header.xtream_cluster != XtreamCluster::Series
        || pli.header.item_type != PlaylistItemType::SeriesInfo {
        return None;
    }

    let mut props_opt = None;
    if resolve_series && !pli.has_details() {
        if let Some(props) = download_and_parse_series_info(app_config, client, input, storage_path, pli, errors, resolve_delay).await {
            *processed_series_info_count += 1;
            props_opt = Some(props);
            
            if updates.is_none() {
                 if let Some(provider_id) = pli.get_provider_id() {
                    let _ = persists_input_series_info(app_config, storage_path, XtreamCluster::Series, &input.name, provider_id, props_opt.as_ref().unwrap()).await;
                 }
            }
        }
    }

    let (group, series_name) = {
        let header = &pli.header;
        (header.group.clone(), if header.name.is_empty() { header.title.clone() } else { header.name.clone() })
    };

    let parsed_episodes = if let Some(ref props) = props_opt {
        parse_xtream_series_info(&pli.get_uuid(), props, &group, &series_name, input, interner)
    } else if let Some(StreamProperties::Series(properties)) = pli.header.additional_properties.as_ref() {
        parse_xtream_series_info(&pli.get_uuid(), properties, &group, &series_name, input, interner)
    } else {
        None
    };

    if let Some(props) = props_opt {
        if let Some(updates) = updates {
             if let Some(provider_id) = pli.get_provider_id() {
                updates.push((provider_id, props));
             }
        } else {
            pli.header.additional_properties = Some(StreamProperties::Series(Box::new(props)));
        }
    }

    if resolve_series && log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
        *last_log_time = Instant::now();
    }

    parsed_episodes
}

pub async fn playlist_resolve_series(cfg: &Arc<AppConfig>,
                                     client: &reqwest::Client,
                                     target: &ConfigTarget,
                                     errors: &mut Vec<TuliproxError>,
                                     pipe: &ProcessingPipe,
                                     provider_fpl: &mut FetchedPlaylist<'_>,
                                     processed_fpl: &mut FetchedPlaylist<'_>,
) {
    let (resolve_series, resolve_delay) = get_resolve_series_options(target, processed_fpl);

    let series_playlist = playlist_resolve_series_info(cfg, client, errors, processed_fpl, resolve_series, resolve_delay).await;
    if series_playlist.is_empty() { return; }

    // original content saved into original list
    if provider_fpl.source.is_memory() {
        for plg in &series_playlist {
            provider_fpl.update_playlist(plg);
        }
    }
    // run processing pipe over new items
    let mut new_playlist = series_playlist;
    for f in pipe {
        if let Some(v) = f(&mut new_playlist, target) {
            new_playlist = v;
        }
    }
    // assign new items to the new playlist
    for plg in &new_playlist {
        processed_fpl.update_playlist(plg);
    }
}

async fn download_and_parse_series_info(
    _app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    _storage_path: &std::path::Path,
    pli: &mut PlaylistItem,
    errors: &mut Vec<TuliproxError>,
    resolve_delay: u16,
) -> Option<SeriesStreamProperties> {
    if let Some(content) = playlist_resolve_download_playlist_item(client, pli, input, errors, resolve_delay, XtreamCluster::Series).await {
        if !content.is_empty() {
            match serde_json::from_str::<XtreamSeriesInfo>(&content) {
                Ok(info) => {
                    return Some(SeriesStreamProperties::from_info(&info, pli));
                }
                Err(err) => {
                    let provider_id = pli.get_provider_id().unwrap_or(0);
                    error!("Failed to parse series info for provider_id {provider_id}: {err}");
                }
            }
        }
    }
    None
}
