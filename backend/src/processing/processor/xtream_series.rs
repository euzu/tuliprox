use shared::model::InputType;
use shared::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ConfigTarget};
use crate::model::{FetchedPlaylist};
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemType, XtreamCluster};
use crate::processing::processor::playlist::ProcessingPipe;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::xtream::{create_resolve_episode_wal_files, create_resolve_info_wal_files, playlist_resolve_download_playlist_item, read_processed_info_ids, should_update_info};
use crate::repository::storage::get_input_storage_path;
use crate::repository::xtream_repository::{write_series_info_to_wal_file, xtream_get_info_file_paths, xtream_update_input_info_file, xtream_update_input_series_episodes_record_from_wal_file, xtream_update_input_series_record_from_wal_file};
use crate::repository::IndexedDocumentReader;
use shared::error::{notify_err, info_err};
use crate::processing::processor::{handle_error, handle_error_and_return, create_resolve_options_function_for_xtream_target};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Instant;
use log::{info, log_enabled, Level};
use crate::model::{XtreamSeriesEpisode, XtreamSeriesInfoEpisode};
use crate::utils;
use crate::utils::bincode_serialize;
use crate::processing::processor::xtream::normalize_json_content;

create_resolve_options_function_for_xtream_target!(series);

async fn read_processed_series_info_ids(cfg: &AppConfig, errors: &mut Vec<TuliproxError>, fpl: &FetchedPlaylist<'_>) -> HashMap<u32, u64> {
    read_processed_info_ids(cfg, errors, fpl, PlaylistItemType::SeriesInfo, |ts: &u64| *ts).await
}

fn write_series_episode_record_to_wal_file(
    writer: &mut BufWriter<&File>,
    provider_id: u32,
    episode: &XtreamSeriesInfoEpisode,
) -> std::io::Result<()> {
    let series_episode = XtreamSeriesEpisode::from(episode);
    if let Ok(content_bytes) = bincode_serialize(&series_episode) {
        writer.write_all(&provider_id.to_le_bytes())?;
        let len = u32::try_from(content_bytes.len()).unwrap();
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&content_bytes)?;
    }
    Ok(())
}

fn should_update_series_info(pli: &mut PlaylistItem, processed_provider_ids: &HashMap<u32, u64>) -> (bool, u32, u64) {
    should_update_info(pli, processed_provider_ids, crate::model::XC_TAG_SERIES_INFO_LAST_MODIFIED)
}

async fn playlist_resolve_series_info(cfg: &AppConfig, client: Arc<reqwest::Client>, errors: &mut Vec<TuliproxError>,
                                      fpl: &mut FetchedPlaylist<'_>, resolve_delay: u16) -> bool {
    let mut processed_info_ids = read_processed_series_info_ids(cfg, errors, fpl).await;
    // we cant write to the indexed-document directly because of the write lock and time-consuming operation.
    // All readers would be waiting for the lock and the app would be unresponsive.
    // We collect the content into a wal file and write it once we collected everything.
    let Some((wal_content_file, wal_record_file, wal_content_path, wal_record_path)) = create_resolve_info_wal_files(&cfg.config.load(), fpl.input, XtreamCluster::Series)
    else { return !processed_info_ids.is_empty(); };

    let mut content_writer = utils::file_writer(&wal_content_file);
    let mut record_writer = utils::file_writer(&wal_record_file);
    let mut content_updated = false;

    // TODO merge both filters to one
    let series_info_count = fpl.playlistgroups.iter()
        .filter(|&plg| plg.xtream_cluster == XtreamCluster::Series)
        .flat_map(|plg| &plg.channels)
        .filter(|&pli| pli.header.item_type == PlaylistItemType::SeriesInfo).count();

    let series_info_iter = fpl.playlistgroups.iter_mut()
        .filter(|plg| plg.xtream_cluster == XtreamCluster::Series)
        .flat_map(|plg| &mut plg.channels)
        .filter(|pli| pli.header.item_type == PlaylistItemType::SeriesInfo);


    info!("Found {series_info_count} series info to resolve");
    let start_time = Instant::now();
    let mut processed_series_info_count = 0;
    let mut last_processed_series_info_count = 0;
    for pli in series_info_iter {
        let (should_update, provider_id, ts) = should_update_series_info(pli, &processed_info_ids);
        if should_update {
            if let Some(content) = playlist_resolve_download_playlist_item(Arc::clone(&client), pli, fpl.input, errors, resolve_delay, XtreamCluster::Series).await {
                let normalized_content = normalize_json_content(content);
                handle_error_and_return!(write_series_info_to_wal_file(provider_id, ts, &normalized_content, &mut content_writer, &mut record_writer),
                        |err| errors.push(notify_err!(format!("Failed to resolve series, could not write to wal file {err}"))));
                processed_info_ids.insert(provider_id, ts);
                content_updated = true;
            }
        }
        if log_enabled!(Level::Info) {
            processed_series_info_count += 1;
            let elapsed = start_time.elapsed().as_secs();
            if elapsed > 0 &&  ((processed_series_info_count - last_processed_series_info_count) > 50) && (elapsed % 30 == 0) {
                info!("resolved {processed_series_info_count}/{series_info_count} series info");
                last_processed_series_info_count = processed_series_info_count;
            }
        }
    }
    if last_processed_series_info_count != processed_series_info_count {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
    }
    // content_wal contains the provider_id and series_info with episode listing
    // record_wal contains provider_id and timestamp
    if content_updated {
        handle_error!(content_writer.flush(),
            |err| errors.push(notify_err!(format!("Failed to resolve vod, could not write to wal file {err}"))));
        handle_error!(record_writer.flush(),
            |err| errors.push(notify_err!(format!("Failed to resolve vod tmdb, could not write to wal file {err}"))));
        handle_error!(content_writer.get_ref().sync_all(), |err| errors.push(notify_err!(format!("Failed to sync series info to wal file {err}"))));
        handle_error!(record_writer.get_ref().sync_all(), |err| errors.push(notify_err!(format!("Failed to sync series info record to wal file {err}"))));
        drop(content_writer);
        drop(wal_content_file);
        drop(record_writer);
        drop(wal_record_file);
        handle_error!(xtream_update_input_info_file(cfg, fpl.input, &wal_content_path, XtreamCluster::Series).await,
            |err| errors.push(err));
        handle_error!(xtream_update_input_series_record_from_wal_file(cfg, fpl.input, &wal_record_path).await,
            |err| errors.push(err));
    }

    // TODO better approach for transactional updates is multiplexed WAL file.
    // we updated now
    // - series_info.db  which contains the original series_info json
    // - series_record.db which contains the series_info provider_id and timestamp
    !processed_info_ids.is_empty()
}
async fn process_series_info(
    app_config: &AppConfig,
    fpl: &mut FetchedPlaylist<'_>,
    errors: &mut Vec<TuliproxError>,
) -> Vec<PlaylistGroup> {
    let mut result: Vec<PlaylistGroup> = vec![];
    let input = fpl.input;
    let config = app_config.config.load();
    let Ok(Some((info_path, idx_path))) = get_input_storage_path(&input.name, &config.working_dir)
        .map(|storage_path| xtream_get_info_file_paths(&storage_path, XtreamCluster::Series))
    else {
        errors.push(notify_err!("Failed to open input info file for series".to_string()));
        return result;
    };

    let _file_lock = app_config.file_locks.read_lock(&info_path);

    // Contains the Series Info with episode listing
    let Ok(mut info_reader) = IndexedDocumentReader::<u32, String>::new(&info_path, &idx_path) else { return result; };

    let Some((wal_file, wal_path)) = create_resolve_episode_wal_files(&config, input) else {
        errors.push(notify_err!("Could not create wal file for series episodes record".to_string()));
        return result;
    };
    let mut wal_writer = utils::file_writer(&wal_file);

    for plg in fpl
        .playlistgroups
        .iter_mut()
        .filter(|plg| plg.xtream_cluster == XtreamCluster::Series)
    {
        let mut group_series = vec![];

        for pli in plg
            .channels
            .iter_mut()
            .filter(|pli| pli.header.item_type == PlaylistItemType::SeriesInfo)
        {
            let Some(provider_id) = pli.header.get_provider_id() else { continue; };
            let Ok(content) = info_reader.get(&provider_id)  else { continue; };
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(series_content) => {
                    let (group, series_name) = {
                        let header = &pli.header;
                        (header.group.clone(), if header.name.is_empty() {header.title.clone()} else { header.name.clone()})
                    };
                    match parse_xtream_series_info(&series_content, &group, &series_name, input) {
                        Ok(Some(mut series)) => {
                            for (episode, pli_episode) in &mut series {
                                let Some(provider_id) = &pli_episode.header.get_provider_id() else { continue; };
                                handle_error!(write_series_episode_record_to_wal_file(&mut wal_writer, *provider_id, episode),
                                |err| errors.push(info_err!(format!("Failed to write to series episode wal file: {err}"))));
                            }
                            group_series.extend(series.into_iter().map(|(_, pli)| pli));
                        }
                        Ok(None) => {}
                        Err(err) => {
                            errors.push(err);
                        }
                    }
                }
                Err(err) => errors.push(info_err!(format!("Failed to parse JSON: {err}"))),
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

    handle_error!(wal_writer.flush(),
            |err| errors.push(notify_err!(format!("Failed to resolve series episodes, could not write to wal file {err}"))));
    drop(wal_writer);
    drop(wal_file);
    handle_error!(xtream_update_input_series_episodes_record_from_wal_file(app_config, input, &wal_path).await,
            |err| errors.push(err));
    result
}


pub async fn playlist_resolve_series(cfg: &AppConfig,
                                     client: Arc<reqwest::Client>,
                                     target: &ConfigTarget,
                                     errors: &mut Vec<TuliproxError>,
                                     pipe: &ProcessingPipe,
                                     provider_fpl: &mut FetchedPlaylist<'_>,
                                     processed_fpl: &mut FetchedPlaylist<'_>,
) {
    let (resolve_series, resolve_delay) = get_resolve_series_options(target, processed_fpl);
    if !resolve_series { return; }

    if !playlist_resolve_series_info(cfg, client, errors, processed_fpl, resolve_delay).await { return; }
    let series_playlist = process_series_info(cfg, provider_fpl, errors).await;
    if series_playlist.is_empty() { return; }
    // original content saved into original list
    for plg in &series_playlist {
        provider_fpl.update_playlist(plg);
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
