use std::sync::Arc;
use shared::error::{TuliproxError};
use crate::model::{AppConfig, ConfigInput};
use shared::model::{LiveStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use crate::repository::{get_input_storage_path, persist_input_live_info, BPlusTreeQuery, xtream_get_file_path};
use crate::utils::{debug_if_enabled};
use log::{debug, warn, info};
use crate::processing::parser::xtream::create_xtream_url;

// Imports for playlist resolution logic
use crate::model::{ConfigTarget, FetchedPlaylist};
use shared::model::{InputType};
use crate::api::model::{MetadataUpdateManager, ResolveReason, ResolveReasonSet, UpdateTask, ProviderIdType};
use crate::processing::processor::{create_resolve_options_function_for_xtream_target, ResolveOptionsFlags};
use crate::processing::processor::playlist::PlaylistProcessingContext;

/// Updates metadata for a single Live stream (primarily probing)
pub async fn update_live_stream_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    id: ProviderIdType,
    save: bool,
    db_query: Option<&mut BPlusTreeQuery<u32, XtreamPlaylistItem>>,
) -> Result<Option<LiveStreamProperties>, TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    // Try to load existing info first to preserve data
    let mut props: Option<LiveStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;
    
    let stream_id_opt = if let ProviderIdType::Id(vid) = id { Some(vid) } else { None };

    if let Some(stream_id) = stream_id_opt {
        // Use provided query or open new one
        if let Some(query) = db_query {
            if let Ok(Some(item)) = query.query_zero_copy(&stream_id) {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Live(p)) = item.additional_properties.as_ref() {
                     props = Some(*p.clone());
                }
            }
        } else {
            let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
            if xtream_path.exists() {
                let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    if let Ok(Some(item)) = query.query_zero_copy(&stream_id) {
                        existing_item = Some(item.clone());
                        if let Some(StreamProperties::Live(p)) = item.additional_properties.as_ref() {
                             props = Some(*p.clone());
                        }
                    }
                }
            }
        }
    }

    // Initialize props if missing
    let mut properties = if let Some(p) = props {
        p
    } else {
         LiveStreamProperties {
            stream_id: stream_id_opt.unwrap_or(0),
            // If item exists but no props, try to recover name
            name: existing_item.as_ref().map_or_else(|| "".into(), |i| i.name.clone()), 
            ..LiveStreamProperties::default()
        }
    };

    // 1. Create dummy properties to generate URL for probing
    // We construct a temporary property object just for URL generation to ensure we match the stream config
    // (prefix/extension) correctly, even if we modify `properties` later.
    let temp_stream_prop = StreamProperties::Live(Box::new(properties.clone()));

    let input_url = input.url.as_str();
    let username = input.username.as_deref().unwrap_or("");
    let password = input.password.as_deref().unwrap_or("");
    let opts = input.options.as_ref();
    let use_prefix = opts.is_none_or(|o| o.xtream_live_stream_use_prefix);
    let no_ext = opts.is_some_and(|o| o.xtream_live_stream_without_extension);

    // We generate the URL to probe directly on the provider
    let stream_url = create_xtream_url(
        XtreamCluster::Live,
        input_url, username, password,
        &temp_stream_prop,
        use_prefix, no_ext
    );

    // 2. Configure FFProbe
    let _ = client; 
    
    let ffprobe_timeout = app_config.config.load().video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
    let user_agent = app_config.config.load().default_user_agent.clone();
    let analyze_duration = 5_000_000;
    let probe_size = 5_000_000;

    let display_id = stream_id_opt.map_or_else(|| "StringID".to_string(), |v| v.to_string());
    debug!("Probing Live Stream ID {} for input {}", display_id, input.name);

    // Update last_probed_timestamp BEFORE probing to ensure we record the attempt even if it crashes/panics (unlikely but safe)
    // Actually, update it before persisting.
    let now = chrono::Utc::now().timestamp();
    properties.last_probed_timestamp = Some(now);

    let mut success = false;
    if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
        &stream_url,
        user_agent.as_deref(),
        analyze_duration,
        probe_size,
        ffprobe_timeout,
    ).await {
        // 3. Update properties on success
        if let Some(v) = raw_video {
            properties.video = Some(v.to_string().into());
        }
        if let Some(a) = raw_audio {
            properties.audio = Some(a.to_string().into());
        }
        properties.last_success_timestamp = Some(now);
        success = true;
        
        debug_if_enabled!("Successfully probed Live Stream ID {}", display_id);
    } else {
        warn!("Probe failed for Live Stream ID {} (Input: {})", display_id, input.name);
        // We still persist the updated last_probed_timestamp so we don't retry immediately
    }

    // 4. Persist
    if save {
        if let Some(stream_id) = stream_id_opt {
            persist_input_live_info(app_config, &storage_path, XtreamCluster::Live, &input.name, stream_id, &properties)
                .await
                .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;
        }
    }
    
    if !success {
        // Return error to propagate failure up to task manager/logs
        return Err(shared::error::info_err!("Probe failed for stream {display_id}"));
    }
    
    Ok(Some(properties))
}

create_resolve_options_function_for_xtream_target!(live);

fn get_resolve_livetv_options(target: &ConfigTarget, fpl: &FetchedPlaylist) -> (bool, u16, u32, bool) {

    let resolve_options = get_resolve_live_options(target, fpl);
    if resolve_options.flags.contains(ResolveOptionsFlags::Resolve) {
        let interval = match target.get_xtream_output() {
            Some(xtream_output) => xtream_output.resolve_live_interval_hours,
            None => 0
        };
        return (true, resolve_options.resolve_delay, interval, resolve_options.flags.contains(ResolveOptionsFlags::Probe));
    }
    (false, 0, 0, false)

}

pub async fn playlist_resolve_livetv(
    ctx: &PlaylistProcessingContext,
    target: &ConfigTarget,
    _errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
) {

    let (resolve_livetv, resolve_delay, interval_hours, probe_requested) = get_resolve_livetv_options(target, fpl);

    let app_config: &Arc<AppConfig> = &ctx.config;
    // let client: &reqwest::Client = &ctx.client;
    // let provider_manager: Option<&Arc<ActiveProviderManager>> = ctx.provider_manager.as_ref();
    let metadata_manager: Option<&Arc<MetadataUpdateManager>> = ctx.metadata_manager.as_ref();

    // Check if ffprobe is enabled globally
    let do_probe = probe_requested && app_config.is_ffprobe_enabled().await;
    // We only proceed if both the target requests it AND global ffprobe is enabled
    if !resolve_livetv || !do_probe {
        return;
    }

    // Determine cutoff timestamp
    let interval_secs = u64::from(interval_hours) * 3600;
    let now = chrono::Utc::now().timestamp();
    let cutoff_ts = now.saturating_sub(interval_secs.cast_signed());

    let input_name_arc = fpl.input.name.clone();
    let mut queued_count = 0;

    for pli in fpl.items_mut() {
        // Only interested in actual Live streams (not VOD/Series)
        if pli.header.xtream_cluster != XtreamCluster::Live {
            continue;
        }
        // Exclude generic/unknown types if necessary, though XtreamCluster::Live usually covers PlaylistItemType::Live
        if !pli.header.item_type.is_live() {
            continue;
        }

        let provider_id = if let Ok(uid) = pli.header.id.parse::<u32>() {
            if uid == 0 { continue; }
            ProviderIdType::Id(uid)
        } else {
            ProviderIdType::from(&*pli.header.id)
        };

        let mut needs_probe = false;

        // Check existing properties
        if let Some(StreamProperties::Live(props)) = pli.header.additional_properties.as_ref() {
            if let Some(last_ts) = props.last_probed_timestamp {
                if last_ts < cutoff_ts {
                    // Expired
                    needs_probe = true;
                }
            } else {
                // Never probed
                needs_probe = true;
            }
        } else {
            // No properties at all
            needs_probe = true;
        }

        if needs_probe {
            if let Some(mgr) = metadata_manager {
                let reason = ResolveReasonSet::from_variants(&[ResolveReason::Probe]);
                let task = UpdateTask::ProbeLive {
                    id: provider_id,
                    reason,
                    delay: resolve_delay,
                    interval: interval_secs,
                };
                let input_name = input_name_arc.clone();
                mgr.queue_task_background(input_name, task);
                queued_count += 1;
            }
        }
    }

    if queued_count > 0 {
        info!("Queued {queued_count} Live TV streams for probing (Input: {input_name_arc})");
    }
}