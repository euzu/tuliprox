use crate::repository::provider_source::ProviderPlaylistSource;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget, ConfigInput};
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
use shared::info_err;

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
                    && pli.get_provider_id().is_some_and(|id| id > 0)
                    && !pli.has_details()).count()
        }
        ProviderPlaylistSource::XtreamDisk { vod, .. } => {
            let mut count = 0;
            if let Some(query) = vod.as_mut() {
                for (_, item) in query.iter() {
                    if item.item_type == PlaylistItemType::Video && item.provider_id > 0 && !item.has_details() {
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
            let mut updates = Vec::new();
            for plg in groups.iter_mut() {
                for pli in &mut plg.channels {
                    if pli.header.xtream_cluster != XtreamCluster::Video
                        || pli.header.item_type != PlaylistItemType::Video
                        || pli.has_details() {
                        continue;
                    }
                    if let Some(props) = download_and_parse_vod_info(app_config, client, input, &storage_path, pli, errors, resolve_delay).await {
                         processed_vod_info_count += 1;
                         if let Some(provider_id) = pli.get_provider_id() {
                            updates.push((provider_id, props));
                         }
                    }

                    if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
                        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
                        last_log_time = Instant::now();
                    }
                }
            }
            // Batch persist VOD info updates
            for (provider_id, props) in updates {
                let _ = persist_input_vod_info(app_config, &storage_path, XtreamCluster::Video, &input.name, provider_id, &props).await;
            }
        }
        ProviderPlaylistSource::XtreamDisk { vod, .. } => {
            if let Some(query) = vod.as_mut() {
                let mut updates = Vec::new();
                for (_, item) in query.iter() {
                    if item.item_type != PlaylistItemType::Video || item.has_details() {
                        continue;
                    }
                    let provider_id = item.provider_id;
                    if provider_id == 0 { continue; }

                    let mut pli = PlaylistItem::from(&item);
                    if let Some(props) = download_and_parse_vod_info(app_config, client, input, &storage_path, &mut pli, errors, resolve_delay).await {
                        processed_vod_info_count += 1;
                        updates.push((provider_id, props));
                    }

                    if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
                        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
                        last_log_time = Instant::now();
                    }
                }
                // Batch persist VOD info updates
                for (provider_id, props) in updates {
                    let _ = persist_input_vod_info(app_config, &storage_path, XtreamCluster::Video, &input.name, provider_id, &props).await;
                }
            }
        }
        ProviderPlaylistSource::M3uDisk { .. } => {}
    }
    if vod_info_count > 0 {
        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
    }
}

async fn download_and_parse_vod_info(
    _app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    _storage_path: &std::path::Path,
    pli: &mut PlaylistItem,
    errors: &mut Vec<TuliproxError>,
    resolve_delay: u16,
) -> Option<VideoStreamProperties> {
    if let Some(provider_id) = pli.get_provider_id() {
        if provider_id == 0 { return None; }
        if let Some(content) = playlist_resolve_download_playlist_item(client, pli, input, errors, resolve_delay, XtreamCluster::Video).await {
            if !content.is_empty() {
                match serde_json::from_str::<XtreamVideoInfo>(&content) {
                    Ok(info) => {
                        let video_stream_props = VideoStreamProperties::from_info(&info, pli);
                        pli.header.additional_properties = Some(StreamProperties::Video(Box::new(video_stream_props.clone())));
                        return Some(video_stream_props);
                    }
                    Err(err) => {
                        error!("Failed to parse video info for provider_id {provider_id}: {err}");
                        errors.push(info_err!(format!("{err}")));
                    }
                }
            }
        }
    }
    None
}
