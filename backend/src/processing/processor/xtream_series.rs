use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::playlist::ProcessingPipe;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::storage::get_input_storage_path;
use crate::repository::xtream_repository::persist_input_series_info_batch;
use crate::repository::{MemoryPlaylistSource, PlaylistSource};
use log::{error, info, log_enabled, Level};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, SeriesStreamProperties, StreamProperties, XtreamSeriesInfo};
use shared::model::{PlaylistGroup, PlaylistItemType, XtreamCluster};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

create_resolve_options_function_for_xtream_target!(series);

const BATCH_SIZE: usize = 100;

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
            error!("Can't resolve series info, input storage directory for input '{}' failed: {err}", input.name);
            return vec![];
        }
    };

    let series_info_count = if resolve_series {
        let series_info_count = fpl.get_missing_series_info_count();
        if series_info_count > 0 {
            info!("Found {series_info_count} series info to resolve");
        }
        series_info_count
    } else {
        0
    };

    let mut last_log_time = Instant::now();
    let mut processed_series_info_count = 0;
    let mut group_series: HashMap<u32, PlaylistGroup> = HashMap::new();
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    let input = fpl.input;
    for pli in fpl.items_mut() {
        if pli.header.xtream_cluster != XtreamCluster::Series
            || pli.header.item_type != PlaylistItemType::SeriesInfo {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else { continue; };
        if provider_id == 0 {
            continue;
        }

        let should_download = resolve_series && !pli.has_details();
        if should_download {
            processed_series_info_count += 1;
            if let Some(content) = playlist_resolve_download_playlist_item(client, pli, input, errors, resolve_delay, XtreamCluster::Series).await {
                if !content.is_empty() {
                    match serde_json::from_str::<XtreamSeriesInfo>(&content) {
                        Ok(info) => {
                            let series_stream_props = SeriesStreamProperties::from_info(&info, pli);

                            batch.push((provider_id, series_stream_props.clone()));
                            if batch.len() >= BATCH_SIZE {
                                if let Err(err) = persist_input_series_info_batch(app_config, &storage_path, XtreamCluster::Series, &input.name, std::mem::take(&mut batch)).await {
                                    error!("Failed to persist batch series info: {err}");
                                }
                            }

                            // Update in-memory playlist items with the newly fetched vod info.
                            // This makes the data available for later processing steps like STRM export.
                            pli.header.additional_properties = Some(StreamProperties::Series(Box::new(series_stream_props)));
                        }
                        Err(err) => {
                            error!("Failed to parse series info for provider_id {provider_id}: {err}");
                        }
                    }
                }
            }
        }

        // extract episodes from info
        if let Some(StreamProperties::Series(properties)) = pli.header.additional_properties.as_ref() {
            let (group, series_name) = {
                let header = &pli.header;
                (header.group.clone(), if header.name.is_empty() { header.title.clone() } else { header.name.clone() })
            };
            if let Some(episodes) = parse_xtream_series_info(&pli.get_uuid(), properties, &group, &series_name, input) {
                let group = group_series.entry(pli.header.category_id)
                    .or_insert_with(|| {
                        PlaylistGroup {
                            id: pli.header.category_id,
                            title: pli.header.group.clone(),
                            channels: Vec::new(),
                            xtream_cluster: XtreamCluster::Series,
                        }
                    });
                group.channels.extend(episodes.into_iter());
            }
        }

        if resolve_series && log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
            info!("resolved {processed_series_info_count}/{series_info_count} series info");
            last_log_time = Instant::now();
        }
    }

    if !batch.is_empty() {
        if let Err(err) = persist_input_series_info_batch(app_config, &storage_path, XtreamCluster::Series, &input.name, batch).await {
            error!("Failed to persist final batch series info: {err}");
        }
    }

    if resolve_series {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
    }
    group_series.into_values().collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn playlist_resolve_series(cfg: &Arc<AppConfig>,
                                     client: &reqwest::Client,
                                     target: &ConfigTarget,
                                     errors: &mut Vec<TuliproxError>,
                                     pipe: &ProcessingPipe,
                                     provider_fpl: &mut FetchedPlaylist<'_>,
                                     processed_fpl: &mut FetchedPlaylist<'_>,
) {
    let (resolve_series, resolve_delay) = get_resolve_series_options(target, processed_fpl);

    provider_fpl.source.release_resources(XtreamCluster::Series);
    let series_playlist = playlist_resolve_series_info(cfg, client, errors, processed_fpl, resolve_series, resolve_delay).await;
    provider_fpl.source.obtain_resources().await;
    if series_playlist.is_empty() { return; }

    if provider_fpl.is_memory() {
        // original content saved into original list
        for plg in &series_playlist {
            provider_fpl.update_playlist(plg).await;
        }
    }
    // run the processing pipe over new items
    let mut new_playlist = series_playlist;
    for f in pipe {
        let mut source = MemoryPlaylistSource::new(new_playlist);
        if let Some(v) = f(&mut source, target) {
            new_playlist = v;
        } else {
            new_playlist = source.take_groups();
        }
    }

    // assign new items to the new playlist
    for plg in &new_playlist {
        processed_fpl.update_playlist(plg).await;
    }
}
