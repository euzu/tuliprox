use shared::model::InputType;
use shared::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ConfigTarget};
use crate::model::{FetchedPlaylist};
use shared::model::{PlaylistItem, PlaylistItemType, XtreamCluster};
use crate::processing::processor::xtream::{create_resolve_info_wal_files, playlist_resolve_download_playlist_item, read_processed_info_ids, should_update_info};
use crate::repository::xtream_repository::{write_vod_info_to_wal_file, xtream_update_input_info_file, xtream_update_input_vod_record_from_wal_file, InputVodInfoRecord};
use shared::error::{notify_err};
use crate::processing::processor::{handle_error, handle_error_and_return, create_resolve_options_function_for_xtream_target};
use shared::utils::{get_u32_from_serde_value, get_u64_from_serde_value, get_string_from_serde_value};
use crate::repository::xtream_repository::xtream_get_input_info;
use serde_json::{from_str, Map, Value};
use std::collections::HashMap;
use std::io::{Write};
use std::sync::Arc;
use std::time::Instant;
use log::{info, log_enabled, Level};
use crate::utils;
use crate::processing::processor::xtream::normalize_json_content;

create_resolve_options_function_for_xtream_target!(vod);

async fn read_processed_vod_info_ids(cfg: &AppConfig, errors: &mut Vec<TuliproxError>, fpl: &FetchedPlaylist<'_>) -> HashMap<u32, u64> {
    read_processed_info_ids(cfg, errors, fpl, PlaylistItemType::Video, |record: &InputVodInfoRecord| record.ts).await
}

fn extract_info_record_from_vod_info(content: &str) -> Option<(u32, InputVodInfoRecord)> {
    let doc = serde_json::from_str::<Map<String, Value>>(content).ok()?;

    let movie_data = doc.get(crate::model::XC_TAG_VOD_INFO_MOVIE_DATA)?.as_object()?;
    let provider_id = get_u32_from_serde_value(
        movie_data.get(crate::model::XC_TAG_VOD_INFO_STREAM_ID)?,
    )?;

    let added = movie_data
        .get(crate::model::XC_TAG_VOD_INFO_ADDED)
        .and_then(get_u64_from_serde_value)
        .unwrap_or(0);

    let info_section = doc.get(crate::model::XC_TAG_VOD_INFO_INFO)?.as_object()?;

    let tmdb_id = info_section
        .get(crate::model::XC_TAG_VOD_INFO_TMDB_ID)
        .and_then(get_u32_from_serde_value)
        .unwrap_or(0);

    let release_date = info_section
        .get(crate::model::XC_TAG_VOD_INFO_RELEASEDATE)
        .and_then(get_string_from_serde_value);

    Some((provider_id, InputVodInfoRecord {
        tmdb_id,
        ts: added,
        release_date,
    }))
}

fn should_update_vod_info(pli: &mut PlaylistItem, processed_provider_ids: &HashMap<u32, u64>) -> (bool, u32, u64) {
    should_update_info(pli, processed_provider_ids, crate::model::XC_TAG_VOD_INFO_ADDED)
}

pub async fn playlist_resolve_vod(app_config: &AppConfig, client: Arc<reqwest::Client>, target: &ConfigTarget, errors: &mut Vec<TuliproxError>, fpl: &mut FetchedPlaylist<'_>) {
    let (resolve_movies, resolve_delay) = get_resolve_vod_options(target, fpl);
    if !resolve_movies { return; }

    // we cant write to the indexed-document directly because of the write lock and time-consuming operation.
    // All readers would be waiting for the lock and the app would be unresponsive.
    // We collect the content into a wal file and write it once we collected everything.
    let config = app_config.config.load();
    let Some((wal_content_file, wal_record_file, wal_content_path, wal_record_path)) = create_resolve_info_wal_files(&config, fpl.input, XtreamCluster::Video)
    else { return; };

    let mut processed_info_ids = read_processed_vod_info_ids(app_config, errors, fpl).await;
    let mut content_writer = utils::file_writer(&wal_content_file);
    let mut record_writer = utils::file_writer(&wal_record_file);
    let mut content_updated = false;

    // TODO merge both filters to one
    let vod_info_count = fpl.playlistgroups.iter()
        .flat_map(|plg| &plg.channels)
        .filter(|&pli| pli.header.xtream_cluster == XtreamCluster::Video).count();

    let vod_info_iter = fpl.playlistgroups.iter_mut()
        .flat_map(|plg| plg.channels.iter_mut())
        .filter(|pli| pli.header.xtream_cluster == XtreamCluster::Video);

    info!("Found {vod_info_count} vod info to resolve");
    let start_time = Instant::now();
    let mut processed_vod_info_count = 0;
    let mut last_processed_vod_info_count = 0;

    for pli in  vod_info_iter {
        let (should_update, _provider_id, _ts) = should_update_vod_info(pli, &processed_info_ids);
        if should_update {
            if let Some(content) = playlist_resolve_download_playlist_item(Arc::clone(&client), pli, fpl.input, errors, resolve_delay, XtreamCluster::Video).await {
                let normalized_content = normalize_json_content(content);
                if let Some((provider_id, info_record)) = extract_info_record_from_vod_info(&normalized_content) {
                    let ts = info_record.ts;
                    handle_error_and_return!(write_vod_info_to_wal_file(provider_id, &normalized_content, &info_record, &mut content_writer, &mut record_writer),
                        |err| errors.push(notify_err!(format!("Failed to resolve vod, could not write to wal file {err}"))));
                    processed_info_ids.insert(provider_id, ts);
                    content_updated = true;
                }
            }
        }
        if log_enabled!(Level::Info) {
            processed_vod_info_count += 1;
            let elapsed = start_time.elapsed().as_secs();
            if elapsed > 0 && ((processed_vod_info_count - last_processed_vod_info_count) > 50) && (elapsed % 30 == 0) {
                info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
                last_processed_vod_info_count = processed_vod_info_count;
            }
        }
    }
    if last_processed_vod_info_count != processed_vod_info_count {
        info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
    }
    if content_updated {
        // TODO better approach for transactional updates is multiplexed WAL file.

        handle_error!(content_writer.flush(),
            |err| errors.push(notify_err!(format!("Failed to resolve vod, could not write to wal file {err}"))));
        handle_error!(record_writer.flush(),
            |err| errors.push(notify_err!(format!("Failed to resolve vod tmdb, could not write to wal file {err}"))));
        handle_error!(content_writer.get_ref().sync_all(), |err| errors.push(notify_err!(format!("Failed to sync vod info to wal file {err}"))));
        handle_error!(record_writer.get_ref().sync_all(), |err| errors.push(notify_err!(format!("Failed to sync vod info record to wal file {err}"))));
        drop(content_writer);
        drop(record_writer);
        drop(wal_content_file);
        drop(wal_record_file);
        handle_error!(xtream_update_input_info_file(app_config, fpl.input, &wal_content_path, XtreamCluster::Video).await,
            |err| errors.push(err));
        handle_error!(xtream_update_input_vod_record_from_wal_file(app_config, fpl.input, &wal_record_path).await,
            |err| errors.push(err));
    }
    
    // Update in-memory playlist items with the newly fetched vod info.
    // This makes the data available for subsequent processing steps like STRM export.
    let vod_info_iter = fpl.playlistgroups.iter_mut()
        .flat_map(|plg| &mut plg.channels)
        .filter(|pli| pli.header.xtream_cluster == XtreamCluster::Video);

    for pli in vod_info_iter {
        if let Some(provider_id) = pli.header.get_provider_id() {
            if let Some(content) = xtream_get_input_info(app_config, fpl.input, provider_id, XtreamCluster::Video) {
                pli.header.additional_properties = from_str::<Map<String, Value>>(&content).ok().and_then(|info_doc| info_doc.get("info").cloned());
            }
        }
    }
}
