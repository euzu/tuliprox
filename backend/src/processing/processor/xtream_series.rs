use crate::api::model::ActiveProviderManager;
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::parser::xtream::create_xtream_series_episode_url;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::playlist::ProcessingPipe;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::{
    get_input_storage_path, persist_input_series_info_batch, MemoryPlaylistSource, PlaylistSource,
};
use log::{error, info, log_enabled, warn, Level, debug};
use shared::error::TuliproxError;
use shared::model::{InputType, PlaylistEntry, SeriesStreamProperties, StreamProperties, XtreamSeriesInfo};
use shared::model::{PlaylistGroup, PlaylistItemType, XtreamCluster};
use indexmap::IndexMap;
use std::sync::Arc;
use std::time::Instant;
use crate::model::MediaQuality;
use serde_json::Value;
use crate::ptt::ptt_parse_title;

create_resolve_options_function_for_xtream_target!(series);

const BATCH_SIZE: usize = 100;

// Returns: (PlaylistGroups, SeriesID->Properties Map, EpisodeID->SeriesID Map)
type SeriesResolveResult = (
    Vec<PlaylistGroup>,
    HashMap<u32, SeriesStreamProperties>,
    HashMap<u32, u32>
);

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn playlist_resolve_series_info(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    resolve_series: bool,
    resolve_delay: u16,
    provider_manager: Option<&Arc<ActiveProviderManager>>,
    do_probe: bool,
) -> SeriesResolveResult {
    let input = fpl.input;
    
    // Get input options
    let input_options = input.options.as_ref();
    let resolve_tmdb_missing = input_options.is_some_and(|o| o.resolve_tmdb);

    let series_info_count = if resolve_series {
        fpl.get_missing_series_info_count()
    } else {
        0
    };

    if series_info_count == 0 && !resolve_tmdb_missing && !do_probe {
        return (vec![], HashMap::new(), HashMap::new());
    }

    if resolve_series && series_info_count > 0 {
         info!("Found {series_info_count} series info to resolve");
    }

    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir).await {
        Ok(storage_path) => storage_path,
        Err(err) => {
            error!(
                "Can't resolve series info, input storage directory for input '{}' failed: {err}",
                input.name
            );
            return (vec![], HashMap::new(), HashMap::new());
        }
    };

    let mut last_log_time = Instant::now();
    let mut processed_series_info_count = 0;
    let mut group_series: IndexMap<u32, PlaylistGroup> = IndexMap::new();
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    // Track Series Properties and Episode-to-Series mapping for updates during probing
    let mut series_props_map: HashMap<u32, SeriesStreamProperties> = HashMap::new();
    let mut episode_to_series_map: HashMap<u32, u32> = HashMap::new();

    // Setup Metadata resolver for fallback
    let library_config = app_config
        .config
        .load()
        .library
        .clone()
        .unwrap_or_default();
    let meta_resolver = MetadataResolver::new(library_config, client.clone());

    // FFProbe config
    let ffprobe_timeout = app_config.config.load().video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
    let user_agent = app_config.config.load().default_user_agent.clone();
    let analyze_duration = 10_000_000;
    let probe_size = 10_000_000;

    let input = fpl.input;
    let input_name_arc = input.name.clone();
    let input_url = input.url.as_str();
    let input_username = input.username.as_ref().map_or("", |v| v);
    let input_password = input.password.as_ref().map_or("", |v| v);

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

        let should_download = resolve_series && !pli.has_details();
        let mut properties_updated = false;

        if should_download {
            if let Some(content) = playlist_resolve_download_playlist_item(
                app_config,
                client,
                pli,
                input,
                errors,
                resolve_delay,
                XtreamCluster::Series,
            )
            .await
            {
                if !content.is_empty() {
                    match serde_json::from_str::<Value>(&content) {
                         Ok(mut json_value) => {
                             // Normalize JSON to handle duplicate keys
                             if let Some(info) = json_value.get_mut("info").and_then(|v| v.as_object_mut()) {
                                 crate::model::normalize_release_date(info);
                             }

                            match serde_json::from_value::<XtreamSeriesInfo>(json_value) {
                                Ok(info) => {
                                    let series_stream_props =
                                        SeriesStreamProperties::from_info(&info, pli);

                                    // Update in-memory playlist items
                                    pli.header.additional_properties =
                                        Some(StreamProperties::Series(Box::new(series_stream_props)));
                                    properties_updated = true;
                                }
                                Err(err) => {
                                    error!(
                                        "Failed to parse series info for provider_id {provider_id}: {err}"
                                    );
                                }
                            }
                        }
                         Err(err) => {
                            error!(
                                "Failed to parse series info JSON for provider_id {provider_id}: {err}"
                            );
                        }
                    }
                }
            }
        }
        
        // Metadata Fallback for Series if TMDB or Release Date is missing
        if resolve_tmdb_missing {
             if let Some(StreamProperties::Series(series_stream_props)) = pli.header.additional_properties.as_mut() {
                // 1. Try to extract year from title if date is missing
                if series_stream_props.release_date.is_none() && !series_stream_props.name.is_empty() {
                      let meta = ptt_parse_title(&series_stream_props.name);
                      if let Some(year) = meta.year {
                          series_stream_props.release_date = Some(format!("{year}-01-01").into());
                          properties_updated = true;
                      }
                }

                let has_tmdb = series_stream_props.tmdb.is_some();
                let has_date = series_stream_props.release_date.is_some();

                if !series_stream_props.name.is_empty() && (!has_tmdb || !has_date)
                {
                    let reason = if !has_tmdb && !has_date { "missing_tmdb_and_date" } 
                                 else if !has_tmdb { "missing_tmdb" } 
                                 else { "missing_date" };
                    debug!("Resolving TMDB for Series '{}' (ID: {}). Reason: {}", series_stream_props.name, provider_id, reason);

                    // Use existing TMDB ID if available
                    if let Some(meta) = meta_resolver
                        .resolve_from_title(&series_stream_props.name, series_stream_props.tmdb, false)
                        .await
                    {
                        let mut changed = false;
                        if series_stream_props.tmdb.is_none() {
                            series_stream_props.tmdb = meta.tmdb_id();
                            changed = true;
                        }
                        if series_stream_props.release_date.is_none() {
                            series_stream_props.release_date =
                                meta.year().map(|y| format!("{y}-01-01").into());
                            changed = true;
                        }
                        if changed {
                            properties_updated = true;
                        }
                    }
                }
             }
        }

        // FFprobe Analysis (Integrated directly here to persist results!)
        if do_probe {
             if let Some(StreamProperties::Series(series_stream_props)) = pli.header.additional_properties.as_mut() {
                 if let Some(details) = series_stream_props.details.as_mut() {
                     if let Some(episodes) = details.episodes.as_mut() {
                         if let Some(provider_mgr) = provider_manager {
                             let dummy_addr = "127.0.0.1:0".parse().unwrap();
                             
                             // Iterate episodes and check if we need to probe
                             for ep in episodes {
                                 let mut missing_info = Vec::new();
                                 let missing_video = !MediaQuality::is_valid_media_info(ep.video.as_deref());
                                 let missing_audio = !MediaQuality::is_valid_media_info(ep.audio.as_deref());
                                 
                                 if missing_video { missing_info.push("video"); }
                                 if missing_audio { missing_info.push("audio"); }
                                 
                                 let needs_probe = !missing_info.is_empty();

                                 if needs_probe {
                                     // Acquire connection
                                     if let Some(handle) = provider_mgr.acquire_connection_with_grace_override(&input_name_arc, &dummy_addr, false).await {
                                         let episode_url = create_xtream_series_episode_url(input_url, input_username, input_password, ep);
                                         
                                         crate::utils::debug_if_enabled!(
                                            "Probing episode '{}' (ID: {}). Missing: [{}]",
                                            ep.title, ep.id, missing_info.join(", ")
                                         );

                                         if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                                            &episode_url,
                                            user_agent.as_deref(),
                                            analyze_duration,
                                            probe_size,
                                            ffprobe_timeout,
                                         ).await {
                                              if let Some(v) = raw_video {
                                                  ep.video = Some(v.to_string().into());
                                              }
                                              if let Some(a) = raw_audio {
                                                  ep.audio = Some(a.to_string().into());
                                              }
                                              properties_updated = true;
                                         }
                                         
                                         provider_mgr.release_handle(&handle).await;
                                     } else {
                                          warn!("Skipping probe for episode {} due to connection limits", ep.title);
                                     }
                                 }
                             }
                         }
                     }
                 }
             }
        }


        // Add to batch for persistence if properties were updated
        if properties_updated {
            processed_series_info_count += 1;
            
            if let Some(StreamProperties::Series(props)) =
                pli.header.additional_properties.as_ref()
            {
                batch.push((provider_id, *props.clone()));
                if batch.len() >= BATCH_SIZE {
                    if let Err(err) = persist_input_series_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Series,
                        &input.name,
                        std::mem::take(&mut batch),
                    )
                    .await
                    {
                        error!("Failed to persist batch series info: {err}");
                    }
                }
            }
        }

        // Capture global release date from the basic playlist item (get_series) BEFORE updating with detailed info
        // This is needed because detailed info might have different dates/metadata structure,
        // and we want to ensure the Series folder name generated later uses the year from the main list.
        let global_release_date = pli.header.additional_properties.as_ref()
            .and_then(|p| p.get_release_date());

        // Extract episodes from info and build groups
        if let Some(StreamProperties::Series(properties)) =
            pli.header.additional_properties.as_ref()
        {
            // Store properties for later lookup
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
                // Build reverse mapping: Episode ID -> Parent Series ID
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

    if !batch.is_empty() {
        if let Err(err) = persist_input_series_info_batch(
            app_config,
            &storage_path,
            XtreamCluster::Series,
            &input.name,
            batch,
        )
        .await
        {
            error!("Failed to persist final batch series info: {err}");
        }
    }

    if resolve_series && series_info_count > 0 {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
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
) {
    let (resolve_series, resolve_delay) = get_resolve_series_options(target, processed_fpl);

    // Get input options
    let input = processed_fpl.input;
    let input_options = input.options.as_ref();
    let probe_requested = input_options.is_some_and(|o| o.analyze_stream);

    // FFprobe check
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

    provider_fpl.source.release_resources(XtreamCluster::Series);

    // 1. Resolve Series Info, Generate Episodes, AND Probe if needed (Integrated)
    // Results are persisted within this call
    let (series_playlist, mut series_props_map, _episode_map) = playlist_resolve_series_info(
        cfg,
        client,
        errors,
        processed_fpl,
        resolve_series,
        resolve_delay,
        provider_manager,
        do_probe
    )
    .await;

    provider_fpl.source.obtain_resources().await;
    if series_playlist.is_empty() {
        return;
    }

    // Update memory cache if provider_fpl is memory based - this ensures subsequent targets see the new info
    if provider_fpl.is_memory() {
        // Collect SeriesStreamProperties updates to sync back to source
        let mut updated_source_items: Vec<(u32, SeriesStreamProperties)> = Vec::new();
        
        // We iterate the series map instead of the playlist because the map contains the Full properties
        for (provider_id, props) in series_props_map.drain() {
            updated_source_items.push((provider_id, props));
        }

        // Apply updates to the source playlist items
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