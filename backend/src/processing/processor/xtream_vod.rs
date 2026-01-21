use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::get_input_storage_path;
use crate::repository::persist_input_vod_info_batch;
use log::{error, info, log_enabled, Level};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, StreamProperties, VideoStreamProperties, XtreamVideoInfo};
use shared::model::{PlaylistItemType, XtreamCluster};
use std::sync::Arc;
use std::time::Instant;


create_resolve_options_function_for_xtream_target!(vod);

const BATCH_SIZE: usize = 100;

pub async fn playlist_resolve_vod(app_config: &Arc<AppConfig>,
                                  client: &reqwest::Client,
                                  target: &ConfigTarget,
                                  errors: &mut Vec<TuliproxError>,
                                  provider_fpl: &mut FetchedPlaylist<'_>,
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

    let vod_info_count = fpl.get_missing_vod_info_count();
    info!("Found missing {vod_info_count} vod info to resolve");

    let mut last_log_time = Instant::now();
    let mut processed_vod_info_count = 0;
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let default_user_agent = app_config.config.load().default_user_agent.clone();

    provider_fpl.source.release_resources(XtreamCluster::Video);

    let input = fpl.input;
    for pli in fpl.items_mut() {
        if pli.header.xtream_cluster != XtreamCluster::Video
            || pli.header.item_type != PlaylistItemType::Video
            || pli.has_details() {
            continue;
        }
        let Some(provider_id) = pli.get_provider_id() else { continue; };
        processed_vod_info_count += 1;
        if provider_id != 0 {
            if let Some(content) = playlist_resolve_download_playlist_item(
                client,
                pli,
                input,
                errors,
                resolve_delay,
                XtreamCluster::Video,
                default_user_agent.as_deref(),
            )
                .await
            {
                if content.is_empty() { continue; }
                //tokio::fs::write(&storage_path.join(format!("{provider_id}_vod_info.json")), &content).await.ok();
                match serde_json::from_str::<XtreamVideoInfo>(&content) {
                    Ok(info) => {
                        let video_stream_props = VideoStreamProperties::from_info(&info, pli);

                        batch.push((provider_id, video_stream_props.clone()));
                        if batch.len() >= BATCH_SIZE {
                            if let Err(err) = persist_input_vod_info_batch(app_config, &storage_path, XtreamCluster::Video, &input.name, std::mem::take(&mut batch)).await {
                                error!("Failed to persist batch VOD info: {err}");
                            }
                        }

                        // This makes the data available for subsequent processing steps like STRM export.
                        pli.header.additional_properties = Some(StreamProperties::Video(Box::new(video_stream_props)));
                    }
                    Err(err) => {
                        error!("Failed to parse video info for provider {} stream_id {provider_id}: {err} {content}", input.name);
                    }
                }
            }
        }
        if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
            info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
            last_log_time = Instant::now();
        }
    }

    if !batch.is_empty() {
        if let Err(err) = persist_input_vod_info_batch(app_config, &storage_path, XtreamCluster::Video, &input.name, batch).await {
            error!("Failed to persist final batch VOD info: {err}");
        }
    }

    provider_fpl.source.obtain_resources().await;
    info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
}
