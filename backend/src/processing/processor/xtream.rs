use std::sync::Arc;
use shared::error::{TuliproxError};
use crate::model::{AppConfig, ConfigInput};
use shared::model::{LiveStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use crate::repository::{get_input_storage_path, persist_input_live_info, BPlusTreeQuery, xtream_get_file_path};
use crate::utils::{debug_if_enabled};
use log::{debug, warn};
use crate::processing::parser::xtream::create_xtream_url;
use crate::api::model::ProviderIdType;

/// Updates metadata for a single Live stream (primarily probing)
pub async fn update_live_stream_metadata(
    app_config: &Arc<AppConfig>,
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
    let config = app_config.config.load();
    let ffprobe_timeout = config.video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
    let user_agent = config.default_user_agent.clone();
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
