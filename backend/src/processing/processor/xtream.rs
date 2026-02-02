use std::sync::Arc;
use shared::error::{TuliproxError};
use crate::model::{AppConfig, ConfigInput, InputSource};
use shared::model::{LiveStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use crate::repository::{get_input_storage_path, persist_input_live_info, BPlusTreeQuery, xtream_get_file_path};
use crate::utils::{debug_if_enabled, xtream};
use log::{debug, warn, info};
use crate::processing::parser::xtream::create_xtream_url;

// Imports for playlist resolution logic
use crate::model::{ConfigTarget, FetchedPlaylist};
use shared::model::{InputType, PlaylistEntry};
use crate::api::model::{ActiveProviderManager, metadata_update_manager::{MetadataUpdateManager, UpdateTask}};

#[allow(dead_code)]
pub async fn get_xtream_stream_info_content(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &InputSource, trace_log: bool) -> Result<String, std::io::Error> {
    xtream::get_xtream_stream_info_content(app_config, client, input, trace_log).await
}

/// Updates metadata for a single Live stream (primarily probing)
pub async fn update_live_stream_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    stream_id: u32,
) -> Result<(), TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    // Try to load existing info first to preserve data
    let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
    let mut props: Option<LiveStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;

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

    // Initialize props if missing
    let mut properties = if let Some(p) = props {
        p
    } else {
         LiveStreamProperties {
            stream_id,
            // If item exists but no props, try to recover name
            name: existing_item.as_ref().map(|i| i.name.clone()).unwrap_or_else(|| "".into()), 
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
    let use_prefix = opts.map_or(true, |o| o.xtream_live_stream_use_prefix);
    let no_ext = opts.map_or(false, |o| o.xtream_live_stream_without_extension);

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

    debug!("Probing Live Stream ID {} for input {}", stream_id, input.name);

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
        
        debug_if_enabled!("Successfully probed Live Stream ID {}", stream_id);
    } else {
        warn!("Probe failed for Live Stream ID {} (Input: {})", stream_id, input.name);
        // We still persist the updated last_probed_timestamp so we don't retry immediately
    }

    // 4. Persist
    persist_input_live_info(app_config, &storage_path, XtreamCluster::Live, &input.name, stream_id, &properties)
        .await
        .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;
    
    if !success {
        // Return error to propagate failure up to task manager/logs
        return Err(shared::error::info_err!("Probe failed for stream {stream_id}"));
    }
    
    Ok(())
}

fn get_resolve_livetv_options(target: &ConfigTarget, fpl: &FetchedPlaylist) -> (bool, u32) {
    match target.get_xtream_output() {
        Some(xtream_output) => (
            xtream_output.resolve_livetv && fpl.input.input_type == InputType::Xtream,
            xtream_output.resolve_livetv_interval_hours
        ),
        None => (false, 0)
    }
}

pub async fn playlist_resolve_livetv(
    app_config: &Arc<AppConfig>,
    _client: &reqwest::Client,
    target: &ConfigTarget,
    _errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    _provider_manager: Option<&Arc<ActiveProviderManager>>,
    metadata_manager: Option<&Arc<MetadataUpdateManager>>,
) {
    let (resolve_livetv, interval_hours) = get_resolve_livetv_options(target, fpl);

    // Check if ffprobe is enabled globally
    let ffprobe_enabled = app_config.config.load().video.as_ref().is_some_and(|v| v.ffprobe_enabled);
    
    // We only proceed if both the target requests it AND global ffprobe is enabled
    if !resolve_livetv || !ffprobe_enabled {
        return;
    }

    // Double check system availability
    if !crate::utils::ffmpeg::check_ffprobe_availability().await {
        warn!("LiveTV probe requested but ffprobe is missing/not executable.");
        return;
    }

    // Determine cutoff timestamp
    let interval_secs = u64::from(interval_hours) * 3600;
    let now = chrono::Utc::now().timestamp();
    let cutoff_ts = now.saturating_sub(interval_secs as i64);

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

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

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
                let task = UpdateTask::ProbeLive { 
                    id: provider_id, 
                    reason: "interval".to_string() 
                };
                let input_name = input_name_arc.clone();
                let mgr_arc = Arc::clone(mgr);
                tokio::spawn(async move {
                    mgr_arc.queue_task(input_name, task).await;
                });
                queued_count += 1;
            }
        }
    }

    if queued_count > 0 {
        info!("Queued {queued_count} Live TV streams for probing (Input: {})", input_name_arc);
    }
}