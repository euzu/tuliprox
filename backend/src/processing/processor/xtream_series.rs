use crate::api::model::{ActiveProviderManager, ProviderHandle};
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::parser::xtream::create_xtream_series_episode_url;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::playlist::ProcessingPipe;
use crate::repository::{
    get_input_storage_path, persist_input_series_info_batch, MemoryPlaylistSource, PlaylistSource,
};
use log::{error, info, log_enabled, warn, Level, debug};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, SeriesStreamProperties, StreamProperties, XtreamSeriesInfo, UpdateOutputStrategy};
use shared::model::{PlaylistGroup, PlaylistItemType, XtreamCluster};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use crate::model::{ConfigInput, MediaQuality, InputSource};
use serde_json::Value;
use crate::ptt::ptt_parse_title;
use crate::api::model::metadata_update_manager::{MetadataUpdateManager, UpdateTask};
use crate::utils::{debug_if_enabled, xtream};
use crate::repository::persists_input_series_info;
use shared::model::XtreamPlaylistItem;
use crate::repository::{BPlusTreeQuery, xtream_get_file_path};

create_resolve_options_function_for_xtream_target!(series);

const BATCH_SIZE: usize = 100;

// Returns: (PlaylistGroups, SeriesID->Properties Map, EpisodeID->SeriesID Map)
type SeriesResolveResult = (
    Vec<PlaylistGroup>,
    HashMap<u32, SeriesStreamProperties>,
    HashMap<u32, u32>
);

/// Updates metadata for a single Series (Info + Episodes Probe) and persists it.
///
/// # Arguments
/// * `save` - If true, persists changes to the input database immediately (Instant strategy).
///            If false, returns the properties so the caller can batch persist them (Bundled strategy).
/// * `fetch_info` - If true, fetches details from Provider API. If false, uses existing/dummy data.
#[allow(clippy::too_many_arguments)]
pub async fn update_series_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    series_id: u32,
    active_provider: &Arc<ActiveProviderManager>,
    active_handle: Option<&ProviderHandle>,
    playlist_title: Option<&str>, 
    save: bool,
    fetch_info: bool,
) -> Result<Option<SeriesStreamProperties>, TuliproxError> {
     let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    let opts = input.options.as_ref();
    if opts.is_some_and(|o| o.xtream_skip_series) {
        return Ok(None);
    }
    
    // Try to load existing info first
    let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Series);
    let mut props: Option<SeriesStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;

    if xtream_path.exists() {
        let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
        if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
            if let Ok(Some(item)) = query.query_zero_copy(&series_id) {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Series(p)) = item.additional_properties.as_ref() {
                     props = Some(*p.clone());
                }
            }
        }
    }

    let mut fetched_new = false;
    let mut properties_updated = false;
    
    // Use playlist title for logging if available, otherwise fallback
    let _display_title = playlist_title
        .or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()))
        .or_else(|| props.as_ref().map(|p| p.name.as_ref()))
        .unwrap_or("Unknown")
        .to_string();

    // 1. Fetch Info from Provider (ONLY if fetch_info is true)
    // Force fetch if requested, to ensuring freshness/completeness
    if fetch_info {
        // Fetch Info from Provider
        let info_url = xtream::get_xtream_player_api_info_url(input, XtreamCluster::Series, series_id)
            .ok_or_else(|| shared::error::info_err!("Failed to build info URL"))?;

        let input_source = InputSource::from(input).with_url(info_url);
        let content = xtream::get_xtream_stream_info_content(app_config, client, &input_source, false)
            .await
            .map_err(|e| shared::error::info_err!("{e}"))?;

        if !content.is_empty() {
             if let Ok(mut json_value) = serde_json::from_str::<Value>(&content) {
                if let Some(info) = json_value.get_mut("info").and_then(|v| v.as_object_mut()) {
                    crate::model::normalize_release_date(info);
                }

                if let Ok(info) = serde_json::from_value::<XtreamSeriesInfo>(json_value) {
                     let temp_item = if let Some(existing) = &existing_item {
                         existing.clone()
                     } else {
                          XtreamPlaylistItem {
                            virtual_id: 0,
                            provider_id: series_id,
                            name: info.info.name.clone(),
                            logo: "".into(), logo_small: "".into(), group: "".into(), title: "".into(), parent_code: "".into(), rec: "".into(), url: "".into(), epg_channel_id: None, xtream_cluster: XtreamCluster::Series, additional_properties: None, item_type: PlaylistItemType::Series, category_id: 0, input_name: input.name.clone(), channel_no: 0, source_ordinal: 0
                        }
                     };
                     
                     // Log details about fetched series
                     let season_count = info.seasons.as_ref().map(|s| s.len()).unwrap_or(0);
                     let episode_count = info.episodes.as_ref().map(|e| e.len()).unwrap_or(0);
                     debug_if_enabled!("Fetched info for Series ID {}: {} seasons, {} episodes", series_id, season_count, episode_count);
                     
                     props = Some(SeriesStreamProperties::from_info(&info, &temp_item));
                     fetched_new = true;
                     properties_updated = true;
                }
             }
        }
    }
    
    // If we don't have props, verify if we can proceed with minimal dummy props
    if props.is_none() {
         if let Some(title) = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref())) {
              let new_props = SeriesStreamProperties {
                  name: title.into(),
                  series_id,
                  ..Default::default()
              };
              // Ensure details struct exists if needed, but for series we often need episodes from API
              // If we didn't fetch API, we likely won't have episodes unless we have them from cache.
              // If cache was empty and fetch failed/skipped, we really have nothing.
              // But for TMDB resolution based on title, we can still proceed.
              props = Some(new_props);
         } else {
             return Err(shared::error::info_err!("No Series properties available and no title found for {series_id}"));
         }
    }
    
    let mut properties = props.unwrap();

    // 2. Resolve TMDB/Date if missing
    if (properties.tmdb.is_none() || properties.release_date.is_none()) && !properties.name.is_empty() {
        let library_config = app_config.config.load().library.clone().unwrap_or_default();
        let meta_resolver = MetadataResolver::new(library_config, client.clone());

        // Strategy Priority:
        // 1. Playlist Title (passed arg)
        // 2. Playlist Title (from existing item on disk)
        // 3. API Name
        
        let mut meta = None;
        let mut tried_title = false;

        // Try extracting year locally first
        if properties.release_date.is_none() {
            let meta_parse = ptt_parse_title(&properties.name);
            if let Some(year) = meta_parse.year {
                properties.release_date = Some(format!("{year}-01-01").into());
                properties_updated = true;
                debug_if_enabled!("Parsed local year for Series '{}': {}", properties.name, year);
            }
        }
        
        // 1. & 2. Playlist Title
        let title_candidate = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()));
        if let Some(title) = title_candidate {
            if !title.is_empty() {
                debug!("Resolving TMDB for Series using Playlist Title '{}' (ID: {})...", title, series_id);
                meta = meta_resolver.resolve_from_title(title, properties.tmdb, false).await;
                tried_title = true;
            }
        }
        
        // 3. API Name (fallback)
        if meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none())) {
             if !properties.name.is_empty() {
                 let title_already_tried = if let Some(t) = title_candidate { t == properties.name.as_ref() } else { false };
                 if !tried_title || !title_already_tried {
                     debug!("Fallback to API Name '{}'...", properties.name);
                     meta = meta_resolver.resolve_from_title(&properties.name, properties.tmdb, false).await;
                 }
             }
        }

        if let Some(m) = meta {
            if properties.tmdb.is_none() {
                properties.tmdb = m.tmdb_id();
                properties_updated = true;
            }
            if properties.release_date.is_none() {
                properties.release_date = m.year().map(|y| format!("{y}-01-01").into());
                properties_updated = true;
            }
            if properties_updated {
                let id_display = properties.tmdb.map_or("None".to_string(), |id| id.to_string());
                debug_if_enabled!("Resolved TMDB for Series ID {}: {}", series_id, id_display);
            }
        }
    }

    // 3. Probe Episodes (if enabled)
    let ffprobe_enabled = app_config.config.load().video.as_ref().is_some_and(|v| v.ffprobe_enabled);
    if ffprobe_enabled {
        if crate::utils::ffmpeg::check_ffprobe_availability().await {
             if let Some(details) = properties.details.as_mut() {
                 if let Some(episodes) = details.episodes.as_mut() {
                     let dummy_addr = "127.0.0.1:0".parse().unwrap();
                     let ffprobe_timeout = app_config.config.load().video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
                     let user_agent = app_config.config.load().default_user_agent.clone();
                     let analyze_duration = 10_000_000;
                     let probe_size = 10_000_000;
                     
                     let input_url = input.url.as_str();
                     let input_username = input.username.as_deref().unwrap_or("");
                     let input_password = input.password.as_deref().unwrap_or("");
                     
                     let mut probed_count = 0;

                     for ep in episodes {
                         let missing_video = !MediaQuality::is_valid_media_info(ep.video.as_deref());
                         let missing_audio = !MediaQuality::is_valid_media_info(ep.audio.as_deref());

                         if missing_video || missing_audio {
                             
                             // Acquire Connection logic
                             let temp_handle = if active_handle.is_some() {
                                 None 
                             } else {
                                 active_provider.acquire_connection_with_grace_override(&input.name, &dummy_addr, false, 0).await
                             };
                             
                             if active_handle.is_some() || temp_handle.is_some() {
                                 let episode_url = create_xtream_series_episode_url(input_url, input_username, input_password, ep);
                                 
                                 // Specific logging for the user to follow
                                 let missing_reason = if missing_video && missing_audio { "video/audio" } else if missing_video { "video" } else { "audio" };
                                 debug!("Probing Series Episode '{}' (S{}E{}) - Missing {}", ep.title, ep.season, ep.episode_num, missing_reason);

                                 if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                                    &episode_url,
                                    user_agent.as_deref(),
                                    analyze_duration,
                                    probe_size,
                                    ffprobe_timeout,
                                 ).await {
                                      if let Some(v) = raw_video {
                                          ep.video = Some(v.to_string().into());
                                          properties_updated = true;
                                      }
                                      if let Some(a) = raw_audio {
                                          ep.audio = Some(a.to_string().into());
                                          properties_updated = true;
                                      }
                                      probed_count += 1;
                                 }
                                 
                                 if let Some(h) = temp_handle {
                                     active_provider.release_handle(&h).await;
                                 }
                             } else {
                                 warn!("Skipping probe for series episode {} due to connection limits", ep.title);
                             }
                         }
                     }
                     if probed_count > 0 {
                         info!("Probed {} episodes for Series ID {}", probed_count, series_id);
                     }
                 }
             }
        }
    }

    // 4. Persist
    if properties_updated || fetched_new {
        if save {
            persists_input_series_info(app_config, &storage_path, XtreamCluster::Series, &input.name, series_id, &properties)
                .await
                .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;

            debug_if_enabled!("Successfully updated Series metadata for ID {}", series_id);
        }
        return Ok(Some(properties));
    }

    Ok(None)
}


#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn playlist_resolve_series_info(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    _errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    resolve_series: bool,
    _resolve_delay: u16,
    active_provider: Option<&Arc<ActiveProviderManager>>,
    metadata_manager: Option<&Arc<MetadataUpdateManager>>,
    do_probe: bool,
    update_strategy: UpdateOutputStrategy,
) -> SeriesResolveResult {
    let input = fpl.input;
    
    let input_options = input.options.as_ref();
    let resolve_tmdb_missing = input_options.is_some_and(|o| o.resolve_tmdb);

    let series_info_count = if resolve_series {
        fpl.get_missing_series_info_count()
    } else {
        0
    };
    
    if resolve_series && series_info_count > 0 {
         info!("Found {series_info_count} series info to resolve");
    }

    let mut last_log_time = Instant::now();
    let mut processed_series_info_count = 0;
    let mut group_series: HashMap<u32, PlaylistGroup> = HashMap::new();
    
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir).await {
        Ok(path) => path,
        Err(err) => {
            error!("Can't resolve series, input storage directory for input '{}' failed: {err}", input.name);
            return (vec![], HashMap::new(), HashMap::new());
        }
    };

    let mut series_props_map: HashMap<u32, SeriesStreamProperties> = HashMap::new();
    let mut episode_to_series_map: HashMap<u32, u32> = HashMap::new();

    let input = fpl.input;
    let input_name_arc = input.name.clone();
    
    let mut inline_count = 0;
    let mut queued_count = 0;
    
    let save_immediate = update_strategy == UpdateOutputStrategy::Instant;

    for pli in fpl.items_mut() {
        if pli.header.xtream_cluster != XtreamCluster::Series
            || pli.header.item_type != PlaylistItemType::SeriesInfo
        {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

        let needs_info = resolve_series && !pli.has_details();
        let mut reasons = Vec::new();
        
        if needs_info {
             reasons.push("info");
        }

        if resolve_tmdb_missing {
             if let Some(StreamProperties::Series(series_stream_props)) = pli.header.additional_properties.as_ref() {
                 let has_tmdb = series_stream_props.tmdb.is_some();
                 let has_date = series_stream_props.release_date.is_some();
                 
                 // Check if we have title in properties or playlist item
                 let title_present = !series_stream_props.name.is_empty() || !pli.header.title.is_empty();

                 if title_present && (!has_tmdb || !has_date) {
                      if !has_tmdb { reasons.push("tmdb"); }
                      if !has_date { reasons.push("date"); }
                 }
             } else if needs_info {
                 // Covered by info
             } else {
                 reasons.push("missing_details");
             }
        }
        
        if do_probe {
             if let Some(StreamProperties::Series(series_stream_props)) = pli.header.additional_properties.as_ref() {
                 if let Some(details) = series_stream_props.details.as_ref() {
                     if let Some(episodes) = details.episodes.as_ref() {
                         for ep in episodes {
                             let missing_video = !MediaQuality::is_valid_media_info(ep.video.as_deref());
                             let missing_audio = !MediaQuality::is_valid_media_info(ep.audio.as_deref());
                             if missing_video || missing_audio {
                                 reasons.push("probe");
                                 break; // One episode is enough to trigger task
                             }
                         }
                     }
                 }
             }
        }

        // Check if we need to queue background task
        let force_inline = metadata_manager.is_none();
        if !reasons.is_empty() {
             if let Some(mgr) = metadata_manager {
                 let task = UpdateTask::ResolveSeries { 
                     id: provider_id, 
                     reason: reasons.join(",") 
                 };
                 let input_name = input_name_arc.clone();
                 let mgr_arc = Arc::clone(mgr);
                 tokio::spawn(async move {
                    mgr_arc.queue_task(input_name, task).await; 
                 });
                 queued_count += 1;
            }
        } else {
            // No updates needed
        }

        if !force_inline && !reasons.is_empty() {
             // Queued and not forced -> skip processing expansion for NOW if info is missing.
             // If we just need probe/tmdb update, we can still expand what we have.
             if needs_info {
                 continue;
             }
        }

        // Inline Processing (Fallback or if we have info and just need probe/tmdb inline)
        if force_inline && !reasons.is_empty() {
             if let Some(prov_mgr) = active_provider {
                 let fetch_info = reasons.contains(&"info");

                 let res = update_series_metadata(
                    app_config,
                    client,
                    input,
                    provider_id,
                    prov_mgr, 
                    None, // No active handle
                    Some(&pli.header.title),
                    save_immediate,
                    fetch_info,
                 ).await;

                 match res {
                     Ok(Some(updated_props)) => {
                         pli.header.additional_properties = Some(StreamProperties::Series(Box::new(updated_props.clone())));
                         
                         if !save_immediate {
                             batch.push((provider_id, updated_props));
                             if batch.len() >= BATCH_SIZE {
                                 if let Err(err) = persist_input_series_info_batch(
                                     app_config,
                                     &storage_path,
                                     XtreamCluster::Series,
                                     &input.name,
                                     std::mem::take(&mut batch),
                                 ).await {
                                     error!("Failed to persist batch Series info: {err}");
                                 }
                             }
                        }
                         
                         processed_series_info_count += 1;
                         inline_count += 1;
                     }
                     Ok(None) => {},
                     Err(e) => {
                         error!("Failed to update Series metadata for {}: {e}", pli.header.title);
                     }
                 }
             }
        }

        let global_release_date = pli.header.additional_properties.as_ref()
            .and_then(|p| p.get_release_date());

        // This block is crucial: It expands the SeriesInfo into episodes (PlaylistItems)
        // and adds them to the playlist structure.
        if let Some(StreamProperties::Series(properties)) =
            pli.header.additional_properties.as_ref()
        {
            series_props_map.insert(provider_id, *properties.clone());

            let (group, series_name) = {
                let header = &pli.header;
                (
                    header.group.clone(),
                    if header.name.is_empty() {
                        header.title.clone()
                    } else {
                        header.name.clone()
                    },
                )
            };
            if let Some(episodes) =
                parse_xtream_series_info(&pli.get_uuid(), properties, &group, &series_name, input, global_release_date)
            {
                for ep in &episodes {
                    if let Some(ep_id) = ep.get_provider_id() {
                        episode_to_series_map.insert(ep_id, provider_id);
                    }
                }

                let group = group_series
                    .entry(pli.header.category_id)
                    .or_insert_with(|| PlaylistGroup {
                        id: pli.header.category_id,
                        title: pli.header.group.clone(),
                        channels: Vec::new(),
                        xtream_cluster: XtreamCluster::Series,
                    });
                group.channels.extend(episodes.into_iter());
            }
        }

        if resolve_series
            && log_enabled!(Level::Info)
            && last_log_time.elapsed().as_secs() >= 30
        {
            info!("resolved {processed_series_info_count}/{series_info_count} series info");
            last_log_time = Instant::now();
        }
    }
    
    // Flush remaining batch if bundled strategy
    if !batch.is_empty() {
        if let Err(err) = persist_input_series_info_batch(
             app_config,
             &storage_path,
             XtreamCluster::Series,
             &input.name,
             batch,
         ).await {
             error!("Failed to persist final batch Series info: {err}");
         }
    }

    if resolve_series && series_info_count > 0 {
         info!("processed {processed_series_info_count}/{series_info_count} series info ({queued_count} queued for background, {inline_count} inline)");
    }
    (group_series.into_values().collect(), series_props_map, episode_to_series_map)
}

#[allow(clippy::too_many_arguments)]
pub async fn playlist_resolve_series(
    cfg: &Arc<AppConfig>,
    client: &reqwest::Client,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    pipe: &ProcessingPipe,
    provider_fpl: &mut FetchedPlaylist<'_>,
    processed_fpl: &mut FetchedPlaylist<'_>,
    provider_manager: Option<&Arc<ActiveProviderManager>>,
    metadata_manager: Option<&Arc<MetadataUpdateManager>>,
) {
    let (resolve_series, resolve_delay) = get_resolve_series_options(target, processed_fpl);
    
    let update_strategy = target.get_xtream_output()
        .map(|o| o.update_strategy)
        .unwrap_or(UpdateOutputStrategy::Instant);

    let input = processed_fpl.input;
    let input_options = input.options.as_ref();
    let probe_requested = input_options.is_some_and(|o| o.analyze_stream);
    let resolve_tmdb = input_options.is_some_and(|o| o.resolve_tmdb);

    let ffprobe_enabled = cfg
        .config
        .load()
        .video
        .as_ref()
        .is_some_and(|v| v.ffprobe_enabled);
    let can_probe = if ffprobe_enabled {
        crate::utils::ffmpeg::check_ffprobe_availability().await
    } else {
        false
    };

    let do_probe = probe_requested && can_probe;
    
    // Skip if nothing to do
    if !resolve_series && !do_probe && !resolve_tmdb {
        return;
    }

    provider_fpl.source.release_resources(XtreamCluster::Series);

    let (series_playlist, mut series_props_map, _episode_map) = playlist_resolve_series_info(
        cfg,
        client,
        errors,
        processed_fpl,
        resolve_series,
        resolve_delay,
        provider_manager,
        metadata_manager,
        do_probe,
        update_strategy
    )
    .await;

    provider_fpl.source.obtain_resources().await;
    if series_playlist.is_empty() {
        return;
    }

    if provider_fpl.is_memory() {
        let mut updated_source_items: Vec<(u32, SeriesStreamProperties)> = Vec::new();
        
        for (provider_id, props) in series_props_map.drain() {
            updated_source_items.push((provider_id, props));
        }

        if !updated_source_items.is_empty() {
             let updates_map: HashMap<u32, SeriesStreamProperties> = updated_source_items.into_iter().collect();
             for item in provider_fpl.items_mut() {
                 if let Some(pid) = item.get_provider_id() {
                     if let Some(new_props) = updates_map.get(&pid) {
                         item.header.additional_properties = Some(StreamProperties::Series(Box::new(new_props.clone())));
                     }
                 }
             }
        }
    }

    let mut new_playlist = series_playlist;
    for f in pipe {
        let mut source = MemoryPlaylistSource::new(new_playlist);
        if let Some(v) = f(&mut source, target) {
            new_playlist = v;
        } else {
            new_playlist = source.take_groups();
        }
    }

    for plg in &new_playlist {
        processed_fpl.update_playlist(plg).await;
    }
}