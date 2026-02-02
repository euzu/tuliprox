use crate::api::model::{ActiveProviderManager, ProviderHandle};
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::repository::{get_input_storage_path};
use crate::repository::persist_input_vod_info_batch;
use log::{error, info, log_enabled, warn, Level, debug};
use shared::error::TuliproxError;
use shared::model::{
    InputType, PlaylistEntry, PlaylistItemType, StreamProperties, VideoStreamDetailProperties,
    VideoStreamProperties, XtreamCluster, XtreamVideoInfo, UpdateOutputStrategy,
};
use std::sync::Arc;
use std::time::Instant;
use crate::model::{ConfigInput, MediaQuality};
use serde_json::Value;
use crate::ptt::ptt_parse_title;
use crate::api::model::metadata_update_manager::{MetadataUpdateManager, UpdateTask};
use crate::repository::persist_input_vod_info;
use crate::utils::{debug_if_enabled, xtream};
use crate::model::InputSource;
use shared::model::XtreamPlaylistItem;
use crate::repository::{BPlusTreeQuery, xtream_get_file_path};

create_resolve_options_function_for_xtream_target!(vod);

const BATCH_SIZE: usize = 100;

/// Updates metadata for a single VOD item (Info + Probe).
///
/// # Arguments
/// * `save` - If true, persists changes to the input database immediately (Instant strategy).
///            If false, returns the properties so the caller can batch persist them (Bundled strategy).
/// * `fetch_info` - If true, fetches details from Provider API. If false, uses existing/dummy data.
#[allow(clippy::too_many_arguments)]
pub async fn update_vod_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    stream_id: u32,
    active_handle: Option<&ProviderHandle>,
    active_provider: &Arc<ActiveProviderManager>,
    playlist_title: Option<&str>,
    save: bool,
    fetch_info: bool,
) -> Result<Option<VideoStreamProperties>, TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    // Check if we should skip based on input options
    let opts = input.options.as_ref();
    if opts.is_some_and(|o| o.xtream_skip_vod) {
        return Ok(None);
    }

    // Try to load existing info first to check if we have title/o_name
    let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Video);
    let mut props: Option<VideoStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;

    if xtream_path.exists() {
        let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
        if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
            if let Ok(Some(item)) = query.query_zero_copy(&stream_id) {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Video(p)) = item.additional_properties.as_ref() {
                     props = Some(*p.clone());
                }
            }
        }
    }

    let mut fetched_new = false;
    let mut properties_updated = false;

    // Determine the title to use for logging and fallback. 
    // Cloning here to satisfy borrow checker when 'props' is mutated later.
    let display_title = playlist_title
        .or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()))
        .or_else(|| props.as_ref().map(|p| p.name.as_ref()))
        .unwrap_or("Unknown")
        .to_string();

    // 1. Fetch Info from Provider (ONLY if fetch_info is true)
    // Force fetch if requested, even if we have some data, to ensure freshness/completeness
    if fetch_info {
        let info_url = xtream::get_xtream_player_api_info_url(input, XtreamCluster::Video, stream_id)
            .ok_or_else(|| shared::error::info_err!("Failed to build info URL"))?;

        let input_source = InputSource::from(input).with_url(info_url);
        
        match xtream::get_xtream_stream_info_content(app_config, client, &input_source, false).await {
            Ok(content) => {
                if !content.is_empty() {
                    if let Ok(mut json_value) = serde_json::from_str::<Value>(&content) {
                        if let Some(info) = json_value.get_mut("info").and_then(|v| v.as_object_mut()) {
                             crate::model::normalize_release_date(info);
                        }

                        if let Ok(info) = serde_json::from_value::<XtreamVideoInfo>(json_value) {
                             let temp_item = if let Some(existing) = &existing_item {
                                 existing.clone()
                             } else {
                                  XtreamPlaylistItem {
                                    virtual_id: 0,
                                    provider_id: stream_id,
                                    name: info.info.name.clone(),
                                    logo: "".into(), logo_small: "".into(), group: "".into(), title: "".into(), parent_code: "".into(), rec: "".into(), url: "".into(), epg_channel_id: None, xtream_cluster: XtreamCluster::Video, additional_properties: None, item_type: PlaylistItemType::Video, category_id: 0, input_name: input.name.clone(), channel_no: 0, source_ordinal: 0
                                }
                             };
                             
                             props = Some(VideoStreamProperties::from_info(&info, &temp_item));
                             fetched_new = true;
                             properties_updated = true;
                        }
                    }
                }
            },
            Err(e) => {
                 debug!("Failed to fetch VOD info for {display_title} ({stream_id}): {e}");
            }
        }
    }
    
    // If no props yet, create dummy ones if we have enough info (at least a name/title)
    if props.is_none() {
        if let Some(title) = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref())) {
             let mut new_props = VideoStreamProperties {
                 name: title.into(),
                 stream_id,
                 container_extension: "".into(), // Will be filled later or by probe
                 ..Default::default()
             };
             // Ensure details struct exists
             new_props.details = Some(VideoStreamDetailProperties::default());
             props = Some(new_props);
        } else {
             // We can't proceed without at least a name
             return Err(shared::error::info_err!("No VOD properties available and no title found for {stream_id}"));
        }
    }

    // Now it's safe to unwrap because we handled the None case above
    let mut properties = props.unwrap();

    // 2. Resolve TMDB/Date if missing
    let missing_tmdb = properties.tmdb.is_none();
    let missing_date = properties.details.as_ref().and_then(|d| d.release_date.as_ref()).is_none();

    if missing_tmdb || missing_date {
        // Try local parsing first
        if missing_date && !properties.name.is_empty() {
             let meta_parse = ptt_parse_title(&properties.name);
             if let Some(year) = meta_parse.year {
                  if properties.details.is_none() {
                      properties.details = Some(VideoStreamDetailProperties::default());
                  }
                  if let Some(details) = properties.details.as_mut() {
                      details.release_date = Some(format!("{year}-01-01").into());
                      properties_updated = true;
                      debug_if_enabled!("Parsed local year for '{}': {}", properties.name, year);
                  }
             }
        }

        // Re-check missing date after local parse
        let still_missing_date = properties.details.as_ref().and_then(|d| d.release_date.as_ref()).is_none();

        if missing_tmdb || still_missing_date {
            let library_config = app_config.config.load().library.clone().unwrap_or_default();
            let meta_resolver = MetadataResolver::new(library_config, client.clone());

            let mut meta = None;
            let mut tried_title = false;

            // 1. & 2. Playlist Title
            let title_candidate = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()));
            if let Some(title) = title_candidate {
                if !title.is_empty() {
                    debug!("Resolving TMDB for VOD using Playlist Title '{}' (ID: {})...", title, stream_id);
                    meta = meta_resolver.resolve_from_title(title, properties.tmdb, true).await;
                    tried_title = true;
                }
            }

            // 3. API Name (fallback)
            if meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none())) {
                 if !properties.name.is_empty() {
                     let title_already_tried = if let Some(t) = title_candidate { t == properties.name.as_ref() } else { false };
                     
                     if !tried_title || !title_already_tried {
                         debug!("Fallback to API Name '{}'...", properties.name);
                         meta = meta_resolver.resolve_from_title(&properties.name, properties.tmdb, true).await;
                     }
                 }
            }

            // 4. API Original Name (fallback)
            if meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none())) {
                 if let Some(o_name) = properties.details.as_ref().and_then(|d| d.o_name.as_deref()) {
                     if !o_name.is_empty() && o_name != properties.name.as_ref() {
                         debug!("Fallback to API Original Name '{}'...", o_name);
                         meta = meta_resolver.resolve_from_title(o_name, properties.tmdb, true).await;
                     }
                 }
            }
            
            if let Some(m) = meta {
                 if properties.tmdb.is_none() {
                     properties.tmdb = m.tmdb_id();
                     properties_updated = true;
                 }
                 if let Some(details) = properties.details.as_mut() {
                     if details.release_date.is_none() {
                         details.release_date = m.year().map(|y| format!("{y}-01-01").into());
                         properties_updated = true;
                     }
                 }
                 if properties_updated {
                    let id_display = properties.tmdb.map_or("None".to_string(), |id| id.to_string());
                    debug_if_enabled!("Resolved TMDB for '{}' (ID: {}): {}", display_title, stream_id, id_display);
                 }
            }
        }
    }

    // 3. Probe (if enabled globally in config)
    let ffprobe_enabled = app_config.config.load().video.as_ref().is_some_and(|v| v.ffprobe_enabled);
    if ffprobe_enabled {
        if crate::utils::ffmpeg::check_ffprobe_availability().await {
             // Ensure details struct exists before probing
             if properties.details.is_none() {
                 properties.details = Some(VideoStreamDetailProperties::default());
             }
             
             let details = properties.details.as_ref().unwrap();
             let missing_video = !MediaQuality::is_valid_media_info(details.video.as_deref());
             let missing_audio = !MediaQuality::is_valid_media_info(details.audio.as_deref());

             if missing_video || missing_audio {
                 let input_url = input.url.as_str();
                 let username = input.username.as_deref().unwrap_or("");
                 let password = input.password.as_deref().unwrap_or("");
                 let opts = input.options.as_ref();
                 let use_prefix = opts.map_or(true, |o| o.xtream_live_stream_use_prefix);
                 let no_ext = opts.map_or(false, |o| o.xtream_live_stream_without_extension);

                 let stream_url = crate::processing::parser::xtream::create_xtream_url(
                     XtreamCluster::Video, input_url, username, password,
                     &StreamProperties::Video(Box::new(properties.clone())),
                     use_prefix, no_ext
                 );

                 let ffprobe_timeout = app_config.config.load().video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
                 let user_agent = app_config.config.load().default_user_agent.clone();
                 let analyze_duration = 10_000_000;
                 let probe_size = 10_000_000;
                 let dummy_addr = "127.0.0.1:0".parse().unwrap();
                 
                 // Acquire Connection logic
                 let temp_handle = if active_handle.is_some() {
                     None // No new handle needed
                 } else {
                     active_provider.acquire_connection_with_grace_override(&input.name, &dummy_addr, false, 0).await
                 };

                 if active_handle.is_some() || temp_handle.is_some() {
                     debug_if_enabled!("Probing VOD '{}' (ID: {})", display_title, stream_id);
                     if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                        &stream_url,
                        user_agent.as_deref(),
                        analyze_duration,
                        probe_size,
                        ffprobe_timeout,
                     ).await {
                          if let Some(details) = properties.details.as_mut() {
                              if let Some(v) = raw_video {
                                  details.video = Some(v.to_string().into());
                                  properties_updated = true;
                              }
                              if let Some(a) = raw_audio {
                                  details.audio = Some(a.to_string().into());
                                  properties_updated = true;
                              }
                          }
                     }
                     if let Some(h) = temp_handle {
                         active_provider.release_handle(&h).await;
                     }
                 } else {
                     warn!("Skipping probe for VOD {} due to connection limits", stream_id);
                 }
             }
        }
    }

    // 4. Persist if updated
    if properties_updated || fetched_new {
        if save {
            persist_input_vod_info(app_config, &storage_path, XtreamCluster::Video, &input.name, stream_id, &properties)
                .await
                .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;

            debug_if_enabled!("Successfully updated VOD metadata for '{}' (ID: {})", display_title, stream_id);
        }
        return Ok(Some(properties));
    }
    
    Ok(None)
}


pub async fn playlist_resolve_vod(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    target: &ConfigTarget,
    _errors: &mut Vec<TuliproxError>,
    provider_fpl: &mut FetchedPlaylist<'_>,
    fpl: &mut FetchedPlaylist<'_>,
    provider_manager: Option<&Arc<ActiveProviderManager>>,
    metadata_manager: Option<&Arc<MetadataUpdateManager>>,
) {
    let (resolve_movies, _resolve_delay) = get_resolve_vod_options(target, fpl);

    // Get input options
    let input = fpl.input;
    let input_options = input.options.as_ref();
    let resolve_tmdb_missing = input_options.is_some_and(|o| o.resolve_tmdb);
    let probe_requested = input_options.is_some_and(|o| o.analyze_stream);

    let ffprobe_enabled = app_config
        .config
        .load()
        .video
        .as_ref()
        .is_some_and(|v| v.ffprobe_enabled);

    // Check ffprobe availability if enabled
    let can_probe = if ffprobe_enabled {
        crate::utils::ffmpeg::check_ffprobe_availability().await
    } else {
        false
    };

    if ffprobe_enabled && !can_probe {
        warn!("ffprobe enabled but not found in system path. Quality analysis will be disabled.");
    }

    let do_probe = probe_requested && can_probe;

    // Determine if we need to do anything
    if !resolve_movies && !do_probe && !resolve_tmdb_missing {
        return;
    }

    let mut vod_info_count = 0;
    // Calculate how many items actually need updates to provide a meaningful progress log
    for pli in fpl.items() {
         if pli.header.xtream_cluster == XtreamCluster::Video
            && pli.header.item_type == PlaylistItemType::Video
            && pli.get_provider_id().is_some()
            && ( (!pli.has_details() && resolve_movies) || resolve_tmdb_missing || do_probe ) {
            vod_info_count += 1;
         }
    }

    if vod_info_count > 0 {
         info!("Found {vod_info_count} vod info candidates for resolution");
    }

    let mut last_log_time = Instant::now();
    let mut processed_vod_info_count = 0;

    provider_fpl.source.release_resources(XtreamCluster::Video);

    let input_name_arc = input.name.clone();

    // To update the source in memory, we collect the updated items
    let mut updated_source_items: Vec<(u32, VideoStreamProperties)> = Vec::new();
    
    // Batch for bundled persistence
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir).await {
        Ok(path) => path,
        Err(err) => {
            error!("Can't resolve vod, input storage directory for input '{}' failed: {err}", input.name);
            return;
        }
    };
    
    let mut queued_count = 0;
    let mut inline_count = 0;

    let update_strategy = target.get_xtream_output()
        .map(|o| o.update_strategy)
        .unwrap_or(UpdateOutputStrategy::Instant);
    
    let save_immediate = update_strategy == UpdateOutputStrategy::Instant;

    for pli in fpl.items_mut() {
        if pli.header.xtream_cluster != XtreamCluster::Video
            || pli.header.item_type != PlaylistItemType::Video
        {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };

        // Check if we need to do anything for this item
        let needs_info = !pli.has_details() && resolve_movies;
        let mut reasons = Vec::new();
        
        if needs_info {
            reasons.push("info");
        }
        
        // TMDB check
        if resolve_tmdb_missing {
             if let Some(StreamProperties::Video(video_stream_props)) = pli.header.additional_properties.as_ref() {
                  let has_tmdb = video_stream_props.tmdb.is_some();
                  let has_date = video_stream_props.details.as_ref().and_then(|d| d.release_date.as_ref()).is_some();
                  
                  if !has_tmdb || !has_date {
                      if !has_tmdb { reasons.push("tmdb"); }
                      if !has_date { reasons.push("date"); }
                  }
             } else if needs_info {
                 // Already covered by needs_info
             } else {
                 // No properties but no download requested? Should be rare
                 reasons.push("missing_details");
             }
        }
        
        // Probe check
        if do_probe {
             let mut missing_info = Vec::new();
             let needs_probe = match pli.header.additional_properties.as_ref() {
                Some(StreamProperties::Video(props)) => {
                    let details = props.details.as_ref();
                    let missing_video = !MediaQuality::is_valid_media_info(details.and_then(|d| d.video.as_deref()));
                    let missing_audio = !MediaQuality::is_valid_media_info(details.and_then(|d| d.audio.as_deref()));

                    if missing_video { missing_info.push("video"); }
                    if missing_audio { missing_info.push("audio"); }

                    !missing_info.is_empty()
                },
                None => {
                    missing_info.push("all_details");
                    true
                },
                _ => false,
            };

            if needs_probe {
                reasons.push("probe");
            }
        }
        
        // If we have reasons to update, check if we should queue background task
        let force_inline = metadata_manager.is_none();
        if !reasons.is_empty() {
             if let Some(mgr) = metadata_manager {
                 let task = UpdateTask::ResolveVod { 
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
            // No updates needed for this item
            continue;
        }

        // Skip inline processing if it was queued (and we are not forced to process inline)
        if !force_inline {
            continue;
        }

        // Inline Processing (Fallback if no manager) or if logic above fell through
        if let Some(prov_mgr) = provider_manager {
            let fetch_info = reasons.contains(&"info");
            
            let res = update_vod_metadata(
                app_config,
                client,
                input,
                provider_id,
                None, // No active handle, function will acquire one if needed
                prov_mgr,
                Some(&pli.header.title),
                save_immediate,
                fetch_info,
            ).await;
            
            match res {
                Ok(Some(updated_props)) => {
                    // Update current item in fpl
                    pli.header.additional_properties = Some(StreamProperties::Video(Box::new(updated_props.clone())));
                    // Schedule update for source memory cache
                    updated_source_items.push((provider_id, updated_props.clone()));
                    
                    if !save_immediate {
                         batch.push((provider_id, updated_props));
                         if batch.len() >= BATCH_SIZE {
                             if let Err(err) = persist_input_vod_info_batch(
                                 app_config,
                                 &storage_path,
                                 XtreamCluster::Video,
                                 &input.name,
                                 std::mem::take(&mut batch),
                             ).await {
                                 error!("Failed to persist batch VOD info: {err}");
                             }
                         }
                    }
                    
                    inline_count += 1;
                    processed_vod_info_count += 1;
                },
                Ok(None) => {}, // No changes
                Err(e) => {
                     error!("Failed to update VOD metadata for {}: {e}", pli.header.title);
                }
            }
        }

        if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
            info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
            last_log_time = Instant::now();
        }
    }
    
    // Flush remaining batch if bundled strategy
    if !batch.is_empty() {
        if let Err(err) = persist_input_vod_info_batch(
             app_config,
             &storage_path,
             XtreamCluster::Video,
             &input.name,
             batch,
         ).await {
             error!("Failed to persist final batch VOD info: {err}");
         }
    }

    // Write-back to source memory cache to avoid reprocessing on next target/iteration
    if !updated_source_items.is_empty() && provider_fpl.is_memory() {
        // Map updates for O(1) lookup
        let updates_map: std::collections::HashMap<u32, VideoStreamProperties> =
            updated_source_items.into_iter().collect();

        // Update items in provider_fpl
        for item in provider_fpl.items_mut() {
            if let Some(provider_id) = item.get_provider_id() {
                if let Some(new_props) = updates_map.get(&provider_id) {
                     item.header.additional_properties = Some(StreamProperties::Video(Box::new(new_props.clone())));
                }
            }
        }
    }

    provider_fpl.source.obtain_resources().await;

    if resolve_movies && vod_info_count > 0 {
        if queued_count > 0 {
             info!("processed {processed_vod_info_count}/{vod_info_count} vod info ({queued_count} queued for background, {inline_count} inline)");
        } else {
             info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
        }
    }
}