use crate::api::model::ActiveProviderManager;
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget, TargetOutput};
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
use std::sync::Arc;
use std::time::Instant;

create_resolve_options_function_for_xtream_target!(vod);

const BATCH_SIZE: usize = 100;

fn should_probe(target: &ConfigTarget) -> bool {
    target.output.iter().any(|o| match o {
        TargetOutput::Strm(s) => s.probe_missing_quality,
        _ => false,
    })
}

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

    // Check if we need to probe based on target configuration
    let probe_requested = should_probe(target);

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

    if !resolve_movies && !do_probe {
        return;
    }

    let input = fpl.input;
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

    let input = fpl.input;
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
            processed_vod_info_count += 1;
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
                            let mut video_stream_props = VideoStreamProperties::from_info(&info, pli);

                            // Fallback Metadata Resolution if TMDB/Year missing
                            if video_stream_props.tmdb.is_none()
                                || video_stream_props
                                    .details
                                    .as_ref()
                                    .and_then(|d| d.release_date.as_ref())
                                    .is_none()
                            {
                                if let Some(meta) = meta_resolver
                                    .resolve_from_title(&video_stream_props.name, true)
                                    .await
                                {
                                    if video_stream_props.tmdb.is_none() {
                                        video_stream_props.tmdb = meta.tmdb_id();
                                    }
                                    if let Some(details) = video_stream_props.details.as_mut() {
                                        if details.release_date.is_none() {
                                            details.release_date =
                                                meta.year().map(|y| format!("{y}-01-01").into());
                                        }
                                    }
                                }
                            }

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

        // 2. FFprobe Analysis if enabled and needed
        // Determine if we need to probe (if details exist but video/audio info is missing)
        let needs_probe = do_probe
            && match pli.header.additional_properties.as_ref() {
                Some(StreamProperties::Video(props)) => props
                    .details
                    .as_ref()
                    .map_or(true, |d| d.video.is_none() || d.audio.is_none()),
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

                    if let Some(quality) = crate::utils::ffmpeg::probe_url(
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
                            // Try metadata fallback for bare items
                            if let Some(meta) =
                                meta_resolver.resolve_from_title(&props.name, true).await
                            {
                                props.tmdb = meta.tmdb_id();
                            }
                            pli.header.additional_properties =
                                Some(StreamProperties::Video(Box::new(props)));
                        }

                        if let Some(StreamProperties::Video(props)) =
                            pli.header.additional_properties.as_mut()
                        {
                            if props.details.is_none() {
                                props.details = Some(VideoStreamDetailProperties {
                                     kinopoisk_url: None, o_name: None, cover_big: None, movie_image: None,
                                     release_date: None, episode_run_time: None, youtube_trailer: None, director: None,
                                     actors: None, cast: None, description: None, plot: None, age: None, mpaa_rating: None,
                                     rating_count_kinopoisk: 0, country: None, genre: None, backdrop_path: None, duration_secs: None,
                                     duration: None, video: None, audio: None, bitrate: 0, runtime: None, status: None
                                 });
                            }

                            if let Some(details) = props.details.as_mut() {
                                // Store technical details in the expected JSON format for StreamProperties
                                let video_json = serde_json::json!({
                                     "width": match quality.resolution {
                                         crate::model::VideoResolution::P4320 => 4320,
                                         crate::model::VideoResolution::P2160 => 2160,
                                         crate::model::VideoResolution::P1440 => 1440,
                                         crate::model::VideoResolution::P1080 => 1080,
                                         crate::model::VideoResolution::P720 => 720,
                                         crate::model::VideoResolution::SD => 480,
                                         _ => 0
                                     },
                                     "codec_name": match quality.video_codec {
                                         crate::model::VideoCodec::H264 => "h264",
                                         crate::model::VideoCodec::H265 => "hevc",
                                         crate::model::VideoCodec::MPEG4 => "mpeg4",
                                         crate::model::VideoCodec::VC1 => "vc1",
                                         crate::model::VideoCodec::AV1 => "av1",
                                         _ => "unknown"
                                     },
                                     "pix_fmt": if quality.bit_depth == crate::model::VideoBitDepth::Ten { "yuv420p10le" } else { "yuv420p" },
                                     "color_transfer": match quality.dynamic_range {
                                         crate::model::VideoDynamicRange::HDR | crate::model::VideoDynamicRange::HDR10 => "smpte2084",
                                         crate::model::VideoDynamicRange::HLG => "arib-std-b67",
                                         crate::model::VideoDynamicRange::DV => "dovi",
                                         _ => "bt709"
                                     },
                                     "codec_tag_string": if quality.dynamic_range == crate::model::VideoDynamicRange::DV { "dovi" } else { "" }
                                 }).to_string();

                                let audio_json = serde_json::json!({
                                     "codec_name": match quality.audio_codec {
                                         crate::model::AudioCodec::AAC => "aac",
                                         crate::model::AudioCodec::AC3 => "ac3",
                                         crate::model::AudioCodec::EAC3 => "eac3",
                                         crate::model::AudioCodec::DTS => "dts",
                                         crate::model::AudioCodec::TrueHD => "truehd",
                                         crate::model::AudioCodec::FLAC => "flac",
                                         _ => "unknown"
                                     },
                                     "channels": match quality.audio_channels {
                                         crate::model::AudioChannels::Surround71 => 8,
                                         crate::model::AudioChannels::Surround51 => 6,
                                         crate::model::AudioChannels::Stereo => 2,
                                         crate::model::AudioChannels::Mono => 1,
                                         _ => 0
                                     }
                                 }).to_string();

                                details.video = Some(video_json.into());
                                details.audio = Some(audio_json.into());
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