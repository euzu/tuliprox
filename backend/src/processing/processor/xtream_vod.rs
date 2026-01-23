use crate::api::model::ActiveProviderManager;
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::get_input_storage_path;
use crate::repository::persist_input_vod_info_batch;
use log::{error, info, log_enabled, warn, Level};
use shared::error::TuliproxError;
use shared::model::{
    InputType, PlaylistEntry, PlaylistItemType, StreamProperties, VideoStreamDetailProperties,
    VideoStreamProperties, XtreamCluster, XtreamVideoInfo,
};
use shared::utils::Internable;
use std::sync::Arc;
use std::time::Instant;
use crate::model::MediaQuality;

create_resolve_options_function_for_xtream_target!(vod);

const BATCH_SIZE: usize = 100;

pub async fn playlist_resolve_vod(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    provider_fpl: &mut FetchedPlaylist<'_>,
    fpl: &mut FetchedPlaylist<'_>,
    provider_manager: Option<&Arc<ActiveProviderManager>>,
) {
    let (resolve_movies, resolve_delay) = get_resolve_vod_options(target, fpl);

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

    // Determine effective probing: requested AND possible
    let do_probe = probe_requested && can_probe;

    // Determine if we need to do anything
    // We proceed if we need to download from provider OR probe OR resolve tmdb
    if !resolve_movies && !do_probe && !resolve_tmdb_missing {
        return;
    }

    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir) {
        Ok(storage_path) => storage_path,
        Err(err) => {
            error!(
                "Can't resolve vod, input storage directory for input '{}' failed: {err}",
                input.name
            );
            return;
        }
    };

    let vod_info_count = fpl.get_missing_vod_info_count();
    // Also count items that need probing if enabled (this logic is simplified, we iterate all anyway)
    info!("Found missing {vod_info_count} vod info to resolve");

    let mut last_log_time = Instant::now();
    let mut processed_vod_info_count = 0;
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    // Setup Metadata resolver for fallback
    let library_config = app_config
        .config
        .load()
        .library
        .clone()
        .unwrap_or_default();
    let meta_resolver = MetadataResolver::new(library_config, client.clone());

    provider_fpl.source.release_resources(XtreamCluster::Video);

    let input_name_arc = input.name.clone();

    for pli in fpl.items_mut() {
        if pli.header.xtream_cluster != XtreamCluster::Video
            || pli.header.item_type != PlaylistItemType::Video
        {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };
        let mut properties_updated = false;

        // 1. Resolve Info from API if missing or forced
        if provider_id != 0 && (!pli.has_details() && resolve_movies) {
            if let Some(content) = playlist_resolve_download_playlist_item(
                app_config,
                client,
                pli,
                input,
                errors,
                resolve_delay,
                XtreamCluster::Video,
            )
            .await
            {
                if !content.is_empty() {
                    match serde_json::from_str::<XtreamVideoInfo>(&content) {
                        Ok(info) => {
                            let video_stream_props = VideoStreamProperties::from_info(&info, pli);
                            // Update item
                            pli.header.additional_properties =
                                Some(StreamProperties::Video(Box::new(video_stream_props)));
                            properties_updated = true;
                        }
                        Err(err) => {
                            error!("Failed to parse video info for provider {} stream_id {provider_id}: {err} {content}", input.name);
                        }
                    }
                }
            }
        }
        
        // 1.5 TMDB / Metadata Fallback
        // Check if we have properties now (either newly fetched or existing)
        if resolve_tmdb_missing {
             if let Some(StreamProperties::Video(video_stream_props)) = pli.header.additional_properties.as_mut() {
                  if video_stream_props.tmdb.is_none() || 
                     video_stream_props.details.as_ref().and_then(|d| d.release_date.as_ref()).is_none() 
                  {
                      if let Some(meta) = meta_resolver
                        .resolve_from_title(&video_stream_props.name, true)
                        .await
                        {
                             let mut changed = false;
                             if video_stream_props.tmdb.is_none() {
                                 video_stream_props.tmdb = meta.tmdb_id();
                                 changed = true;
                             }
                             if let Some(details) = video_stream_props.details.as_mut() {
                                 if details.release_date.is_none() {
                                     details.release_date = meta.year().map(|y| format!("{y}-01-01").into());
                                     changed = true;
                                 }
                             }
                             if changed {
                                 properties_updated = true;
                             }
                        }
                  }
             } else if !pli.has_details() {
                  // Case where we don't have details yet and didn't fetch them (resolve_movies=false)
                  // but we want to resolve tmdb based on playlist title
                  if let Some(meta) = meta_resolver.resolve_from_title(&pli.header.name, true).await {
                      // Create partial properties
                      let mut props = VideoStreamProperties {
                          name: pli.header.name.clone(),
                          category_id: pli.header.category_id,
                          stream_id: pli.header.virtual_id, 
                          stream_icon: pli.header.logo.clone(),
                          direct_source: "".into(), custom_sid: None, added: "".into(), container_extension: "".into(),
                          rating: None, rating_5based: None, stream_type: Some("movie".intern()), trailer: None, tmdb: None, is_adult: 0, details: None
                      };
                      props.tmdb = meta.tmdb_id();
                       if let Some(year) = meta.year() {
                           props.details = Some(VideoStreamDetailProperties {
                                release_date: Some(format!("{year}-01-01").into()),
                                ..VideoStreamDetailProperties::default() // You might need a Default impl for VideoStreamDetailProperties or fill all fields
                           });
                       }
                      
                      pli.header.additional_properties = Some(StreamProperties::Video(Box::new(props)));
                      properties_updated = true;
                  }
             }
        }


        // 2. FFprobe Analysis if enabled and needed
        // Determine if we need to probe (if details exist but video/audio info is missing)
        let needs_probe = do_probe
            && match pli.header.additional_properties.as_ref() {
                Some(StreamProperties::Video(props)) => props
                    .details
                    .as_ref()
                    .map_or(true, |d| !MediaQuality::is_valid_media_info(d.video.as_deref()) || !MediaQuality::is_valid_media_info(d.audio.as_deref())),
                None => true, // If we didn't fetch API details above (e.g. resolve_movies=false), we might still want to probe
                _ => false,
            };

        if needs_probe {
            if let Some(provider_mgr) = provider_manager {
                let ffprobe_timeout = app_config
                    .config
                    .load()
                    .video
                    .as_ref()
                    .and_then(|v| v.ffprobe_timeout)
                    .unwrap_or(60);

                // Try acquire connection
                // Uses 0.0.0.0 as we are internal
                let dummy_addr = "127.0.0.1:0".parse().unwrap();

                // CRITICAL: Check connection limit before probing
                if let Some(handle) = provider_mgr
                    .acquire_connection_with_grace_override(&input_name_arc, &dummy_addr, false)
                    .await
                {
                    let url = pli.header.url.as_ref();
                    let provider_url = if let Some(_alloc) = handle.allocation.get_provider_config()
                    {
                        url.to_string()
                    } else {
                        url.to_string()
                    };

                    crate::utils::debug_if_enabled!(
                        "Probing stream quality for {}",
                        shared::utils::sanitize_sensitive_info(&provider_url)
                    );

                    // Probe with ffmpeg
                    let analyze_duration = 10_000_000; // 10s default
                    let probe_size = 10_000_000; // 10MB default
                    let user_agent = app_config.config.load().default_user_agent.clone();

                    if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                        &provider_url,
                        user_agent.as_deref(),
                        analyze_duration,
                        probe_size,
                        ffprobe_timeout,
                    )
                    .await
                    {
                        // Update PlaylistItem with quality info
                        if pli.header.additional_properties.is_none() {
                            // Create basic properties if missing
                            let mut props = VideoStreamProperties {
                                  name: pli.header.name.clone(),
                                  category_id: pli.header.category_id,
                                  stream_id: pli.header.virtual_id, 
                                  stream_icon: pli.header.logo.clone(),
                                  direct_source: "".into(), custom_sid: None, added: "".into(), container_extension: "".into(),
                                  rating: None, rating_5based: None, stream_type: None, trailer: None, tmdb: None, is_adult: 0, details: None
                              };
                            // Try metadata fallback for bare items if not already done
                            if resolve_tmdb_missing {
                                if let Some(meta) = meta_resolver.resolve_from_title(&props.name, true).await
                                {
                                    props.tmdb = meta.tmdb_id();
                                }
                            }
                            pli.header.additional_properties =
                                Some(StreamProperties::Video(Box::new(props)));
                        }

                        if let Some(StreamProperties::Video(props)) =
                            pli.header.additional_properties.as_mut()
                        {
                            if props.details.is_none() {
                                // Default implementation required for VideoStreamDetailProperties
                                // Assuming we fill minimal defaults
                                props.details = Some(VideoStreamDetailProperties::default());
                            }

                            if let Some(details) = props.details.as_mut() {
                                // OVERWRITE existing fields with FFprobe data
                                if let Some(v) = raw_video {
                                    details.video = Some(v.to_string().into());
                                }
                                if let Some(a) = raw_audio {
                                    details.audio = Some(a.to_string().into());
                                }
                                properties_updated = true;
                            }
                        }
                    }

                    provider_mgr.release_handle(&handle).await;
                } else {
                    warn!("Processing ABORTED for VOD {}: No provider connection available for probe. Will retry next update.", pli.header.name);
                    // CRITICAL Requirement: Abort processing for this item if no connection, so it stays "incomplete"
                    // and is retried next time.
                    continue;
                }
            }
        }

        // Add to batch for persistence ONLY if we have properties and didn't abort
        if properties_updated {
            processed_vod_info_count += 1;
            
            if let Some(StreamProperties::Video(props)) = pli.header.additional_properties.as_ref() {
                batch.push((provider_id, *props.clone()));
                if batch.len() >= BATCH_SIZE {
                    if let Err(err) = persist_input_vod_info_batch(
                        app_config,
                        &storage_path,
                        XtreamCluster::Video,
                        &input.name,
                        std::mem::take(&mut batch),
                    )
                    .await
                    {
                        error!("Failed to persist batch VOD info: {err}");
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
        if let Err(err) = persist_input_vod_info_batch(
            app_config,
            &storage_path,
            XtreamCluster::Video,
            &input.name,
            batch,
        )
        .await
        {
            error!("Failed to persist final batch VOD info: {err}");
        }
    }

    provider_fpl.source.obtain_resources().await;
    info!("resolved {processed_vod_info_count}/{vod_info_count} vod info");
}