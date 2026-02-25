use parking_lot::Mutex;
use std::sync::Arc;
use shared::error::{TuliproxError};
use crate::model::{AppConfig, ConfigInput, ConfigInputFlags};
use shared::model::{LiveStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use crate::repository::{get_input_storage_path, persist_input_live_info, BPlusTreeQuery, xtream_get_file_path};
use crate::utils::{debug_if_enabled};
use log::{debug, warn};
use crate::processing::parser::xtream::create_xtream_url;
use crate::api::model::{ActiveProviderManager, ProviderHandle, ProviderIdType};

/// Updates metadata for a single Live stream (primarily probing)
#[allow(clippy::too_many_lines)]
pub async fn update_live_stream_metadata(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    id: ProviderIdType,
    save: bool,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
    _active_handle: Option<&ProviderHandle>,
    _active_provider: &Arc<ActiveProviderManager>,
) -> Result<Option<LiveStreamProperties>, TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    // Try to load existing info first to preserve data
    let mut props: Option<LiveStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;
    
    let stream_id_opt = if let ProviderIdType::Id(vid) = id { Some(vid) } else { None };

    if let Some(stream_id) = stream_id_opt {
        if let Some(query) = db_query {
            let query = Arc::clone(&query);
            let item = match tokio::task::spawn_blocking(move || {
                let mut guard = query.lock();
                guard.query_zero_copy(&stream_id).ok().flatten()
            })
            .await
            {
                Ok(item) => item,
                Err(err) => {
                    warn!("Failed to query Live metadata from disk for {stream_id}: {err}");
                    None
                }
            };

            if let Some(item) = item {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Live(p)) = item.additional_properties.as_ref() {
                    props = Some(*p.clone());
                }
            }
        } else {
            let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Live);
            if xtream_path.exists() {
                let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
                let xtream_path = xtream_path.clone();
                let item = match tokio::task::spawn_blocking(move || {
                    let _guard = file_lock;
                    let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
                    query.query_zero_copy(&stream_id)
                })
                .await
                {
                    Ok(Ok(item)) => item,
                    Ok(Err(err)) => {
                        warn!("Failed to query Live metadata from disk for {stream_id}: {err}");
                        None
                    }
                    Err(err) => {
                        warn!("Failed to query Live metadata from disk for {stream_id}: {err}");
                        None
                    }
                };

                if let Some(item) = item {
                    existing_item = Some(item.clone());
                    if let Some(StreamProperties::Live(p)) = item.additional_properties.as_ref() {
                        props = Some(*p.clone());
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
    let use_prefix = input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix);
    let no_ext = input.has_flag(ConfigInputFlags::XtreamLiveStreamWithoutExtension);

    // We generate the URL to probe directly on the provider
    let stream_url = create_xtream_url(
        XtreamCluster::Live,
        input_url, username, password,
        &temp_stream_prop,
        use_prefix, no_ext
    );
    let config = app_config.config.load();
    let metadata_update = config.metadata_update.clone().unwrap_or_default();
    let ffprobe_timeout = metadata_update.ffprobe_timeout.unwrap_or(60);
    let user_agent = config.default_user_agent.clone();
    let analyze_duration = metadata_update.ffprobe_live_analyze_duration_micros;
    let probe_size = metadata_update.ffprobe_live_probe_size_bytes;

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
        config.proxy.as_ref(),
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
