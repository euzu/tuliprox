use crate::api::model::ActiveProviderManager;
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget};
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::create_resolve_options_function_for_xtream_target;
use crate::processing::processor::playlist::ProcessingPipe;
use crate::processing::processor::xtream::playlist_resolve_download_playlist_item;
use crate::repository::{
    get_input_storage_path, persist_input_series_info_batch, MemoryPlaylistSource, PlaylistSource,
};
use log::{error, info, log_enabled, warn, Level};
use shared::error::TuliproxError;
use shared::model::{
    InputType, PlaylistEntry, PlaylistItemType, PlaylistGroup, SeriesStreamProperties,
    StreamProperties, XtreamCluster, XtreamSeriesInfo,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use crate::model::MediaQuality;

create_resolve_options_function_for_xtream_target!(series);

const BATCH_SIZE: usize = 100;

#[allow(clippy::too_many_lines)]
async fn playlist_resolve_series_info(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    resolve_series: bool,
    resolve_delay: u16,
) -> Vec<PlaylistGroup> {
    let input = fpl.input;
    
    // Get input options
    let input_options = input.options.as_ref();
    let resolve_tmdb_missing = input_options.is_some_and(|o| o.resolve_tmdb);
    // Note: Probing logic handled in calling function `playlist_resolve_series` for episodes

    let working_dir = &app_config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir) {
        Ok(storage_path) => storage_path,
        Err(err) => {
            error!(
                "Can't resolve series info, input storage directory for input '{}' failed: {err}",
                input.name
            );
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

    // Setup Metadata resolver for fallback
    let library_config = app_config
        .config
        .load()
        .library
        .clone()
        .unwrap_or_default();
    let meta_resolver = MetadataResolver::new(library_config, client.clone());

    let input = fpl.input;
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
                    match serde_json::from_str::<XtreamSeriesInfo>(&content) {
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
            }
        }
        
        // Metadata Fallback for Series if TMDB or Release Date is missing
        if resolve_tmdb_missing {
             if let Some(StreamProperties::Series(series_stream_props)) = pli.header.additional_properties.as_mut() {
                if series_stream_props.tmdb.is_none() || series_stream_props.release_date.is_none() {
                    if let Some(meta) = meta_resolver
                        .resolve_from_title(&series_stream_props.name, false)
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

        // Extract episodes from info and build groups
        if let Some(StreamProperties::Series(properties)) =
            pli.header.additional_properties.as_ref()
        {
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
                parse_xtream_series_info(&pli.get_uuid(), properties, &group, &series_name, input)
            {
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

    if resolve_series {
        info!("resolved {processed_series_info_count}/{series_info_count} series info");
    }
    group_series.into_values().collect()
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

    // 1. Resolve Series Info and Generate Episodes
    let mut series_playlist = playlist_resolve_series_info(
        cfg,
        client,
        errors,
        processed_fpl,
        resolve_series,
        resolve_delay,
    )
    .await;

    // 2. Probe Episodes if requested
    if do_probe {
        let ffprobe_timeout = cfg
            .config
            .load()
            .video
            .as_ref()
            .and_then(|v| v.ffprobe_timeout)
            .unwrap_or(60);
            
        // Use default analysis params for episodes if not specified
        let analyze_duration = 10_000_000;
        let probe_size = 10_000_000;

        if let Some(provider_mgr) = provider_manager {
            let dummy_addr = "127.0.0.1:0".parse().unwrap();
            let input_name_arc = processed_fpl.input.name.clone();

            // Iterate over generated episodes to probe missing quality
            for group in &mut series_playlist {
                for pli in &mut group.channels {
                    let needs_probe = match pli.header.additional_properties.as_ref() {
                        Some(StreamProperties::Episode(props)) => {
                            !MediaQuality::is_valid_media_info(props.video.as_deref()) || !MediaQuality::is_valid_media_info(props.audio.as_deref())
                        }
                        _ => false,
                    };

                    if needs_probe {
                        // CRITICAL: Check connection limit before probing
                        if let Some(handle) = provider_mgr
                            .acquire_connection_with_grace_override(
                                &input_name_arc,
                                &dummy_addr,
                                false,
                            )
                            .await
                        {
                            let url = pli.header.url.clone();
                            crate::utils::debug_if_enabled!(
                                "Probing episode quality for {}",
                                shared::utils::sanitize_sensitive_info(&url)
                            );

                            if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                                &url,
                                None,
                                analyze_duration,
                                probe_size,
                                ffprobe_timeout,
                            )
                            .await
                            {
                                if let Some(StreamProperties::Episode(e)) =
                                    pli.header.additional_properties.as_mut()
                                {
                                    // OVERWRITE with full JSON
                                    if let Some(v) = raw_video {
                                        e.video = Some(v.to_string().into());
                                    }
                                    if let Some(a) = raw_audio {
                                        e.audio = Some(a.to_string().into());
                                    }
                                }
                            }

                            provider_mgr.release_handle(&handle).await;
                        } else {
                            warn!(
                                "Skipping probe for episode {} due to connection limits",
                                pli.header.name
                            );
                        }
                    }
                }
            }
        }
    }

    provider_fpl.source.obtain_resources().await;
    if series_playlist.is_empty() {
        return;
    }

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