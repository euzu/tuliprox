use crate::repository::provider_source::ProviderPlaylistSource;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::storage::get_input_storage_path;
use crate::repository::xtream_repository::persist_input_vod_info;
use log::{error, info, log_enabled, Level};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, StreamProperties, VideoStreamProperties, XtreamVideoInfo};
use shared::model::{PlaylistItemType, XtreamCluster, PlaylistItem};
use std::sync::Arc;
use std::time::Instant;

create_resolve_options_function_for_xtream_target!(vod);

#[allow(clippy::too_many_lines)]
pub async fn playlist_resolve_vod(app_config: &Arc<AppConfig>, client: &reqwest::Client,
                                  target: &ConfigTarget, errors: &mut Vec<TuliproxError>,
                                  fpl: &mut FetchedPlaylist<'_>) {
    let (resolve_movies, resolve_delay) = get_resolve_vod_options(target, fpl);
    if !resolve_movies { return; }

    let input = fpl.input;
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir) {
        Ok(storage_path) => storage_path,
        Err(err) => {
            error!("Can't resolve vod, input storage directory for input '{}' failed: {err}", input.name);
            return;
        }
    };

    // LocalVideo entries are not resolved!
    let vod_info_count = match &mut fpl.source {
        ProviderPlaylistSource::Memory(groups) => {
            groups.iter()
                .flat_map(|plg| &plg.channels)
                .filter(|pli| pli.header.xtream_cluster == XtreamCluster::Video
                    && pli.header.item_type == PlaylistItemType::Video
                    && !pli.has_details()).count()
        }
        ProviderPlaylistSource::XtreamDisk { vod, .. } => {
            let mut count = 0;
            if let Some(query) = vod.as_mut() {
                for (_, item) in query.iter() {
                    if item.item_type == PlaylistItemType::Video && !item.has_details() {
                        count += 1;
                    }
                }
            }
            count
        }
        ProviderPlaylistSource::M3uDisk { .. } => 0,
    };

    if vod_info_count > 0 {
        info!("Found {vod_info_count} vod info to resolve");
    }

    let mut last_log_time = Instant::now();
    let mut processed_vod_info_count = 0;

    match &mut fpl.source {
        ProviderPlaylistSource::Memory(groups) => {
            for plg in groups.iter_mut() {
                for pli in &mut plg.channels {
                    if pli.header.xtream_cluster != XtreamCluster::Video
                        || pli.header.item_type != PlaylistItemType::Video
                        || pli.has_details() {
                        continue;
                    }
                    let Some(provider_id) = pli.get_provider_id() else { continue; };
                    processed_vod_info_count += 1;
                    if provider_id != 0 {
                        if let Some(content) = playlist_resolve_download_playlist_item(client, pli, fpl.input, errors, resolve_delay, XtreamCluster::Video).await {
                            if content.is_empty() { continue; }
                            match serde_json::from_str::<XtreamVideoInfo>(&content) {
                                Ok(info) => {
                                    let video_stream_props = VideoStreamProperties::from_info(&info, pli);
                                    if let Err(err) = persist_input_vod_info(app_config, &storage_path, pli.header.xtream_cluster, &input.name, provider_id, &video_stream_props).await {
                                        error!("Failed to persist VOD info for provider_id {provider_id}: {err}");
                                    }
                                    // This makes the data available for subsequent processing steps like STRM export.
                                    pli.header.additional_properties = Some(StreamProperties::Video(Box::new(video_stream_props)));
                                }
                                Err(err) => {
                                    error!("Failed to parse video info for provider_id {provider_id}: {err}");
                                }
                            }
                        }
                    }
                    if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
                        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
                        last_log_time = Instant::now();
                    }
                }
            }
        }
        ProviderPlaylistSource::XtreamDisk { vod, .. } => {
            if let Some(query) = vod.as_mut() {
                for (_, item) in query.iter() {
                    if item.item_type != PlaylistItemType::Video || item.has_details() {
                        continue;
                    }
                    let pli = PlaylistItem::from(&item);
                    let provider_id = item.provider_id;
                    if provider_id == 0 { continue; }
                    processed_vod_info_count += 1;
                    if let Some(content) = playlist_resolve_download_playlist_item(client, &pli, fpl.input, errors, resolve_delay, XtreamCluster::Video).await {
                        if content.is_empty() { continue; }
                        if let Ok(info) = serde_json::from_str::<XtreamVideoInfo>(&content) {
                            let video_stream_props = VideoStreamProperties::from_info(&info, &pli);
                            if let Err(err) = persist_input_vod_info(app_config, &storage_path, pli.header.xtream_cluster, &input.name, provider_id, &video_stream_props).await {
                                error!("Failed to persist VOD info for provider_id {provider_id}: {err}");
                            }
                        }
                    }
                    if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
                        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
                        last_log_time = Instant::now();
                    }
                }
            }
        }
        ProviderPlaylistSource::M3uDisk { .. } => {}
    }
    if vod_info_count > 0 {
        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
    }
}
