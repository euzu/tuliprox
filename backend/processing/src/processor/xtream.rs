use log::{debug, warn};
use parking_lot::Mutex;
use shared::{
    error::TuliproxError,
    model::{LiveStreamProperties, StreamProperties, XtreamCluster, XtreamPlaylistItem},
};
use std::sync::Arc;
use tuliprox_core::{
    model::{AppConfig, ConfigInput, ConfigInputFlags, ProviderHandle, ProviderIdType},
    utils::{
        debug_if_enabled,
        ffmpeg::{is_supported_probe_url, FfmpegExecutor, ProbeFailureKind, ProbeStreamStats, ProbeUrlOutcome},
    },
};
use tuliprox_parser::xtream::create_xtream_url;
use tuliprox_repository::{get_input_storage_path, persist_input_live_info, xtream_get_file_path, BPlusTreeQuery};
use tuliprox_session::ActiveProviderManager;

/// Updates metadata for a single Live stream (primarily probing)
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn update_live_stream_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    id: ProviderIdType,
    save: bool,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
    _active_handle: Option<&ProviderHandle>,
    _active_provider: &Arc<ActiveProviderManager>,
) -> Result<Option<LiveStreamProperties>, TuliproxError> {
    let storage_dir = &app_config.config.load().storage_dir;
    let storage_path = get_input_storage_path(&input.name, storage_dir)
        .await
        .map_err(|e| TuliproxError::Io(format!("Storage path error: {e}")))?;

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
    let stream_url =
        create_xtream_url(XtreamCluster::Live, input_url, username, password, &temp_stream_prop, use_prefix, no_ext);
    let probe_url_cow = input.resolve_url(&stream_url)?;
    if !is_supported_probe_url(probe_url_cow.as_ref()) {
        debug!("Skipping unsupported live probe for input {}: {}", input.name, probe_url_cow.as_ref());
        return Ok(None);
    }
    let config = app_config.config.load();
    let metadata_update = config.metadata_update.clone().unwrap_or_default();
    let ffprobe_timeout = metadata_update.ffprobe.timeout.unwrap_or(60);
    let user_agent = config.default_user_agent.clone();
    let analyze_duration = metadata_update.ffprobe.live_analyze_duration_micros;
    let probe_size = metadata_update.ffprobe.live_probe_size_bytes.get();

    let display_id = stream_id_opt.map_or_else(|| "StringID".to_string(), |v| v.to_string());
    debug!("Probing Live Stream ID {} for input {}", display_id, input.name);

    // Update last_probed_timestamp BEFORE probing to ensure we record the attempt even if it crashes/panics (unlikely but safe)
    // Actually, update it before persisting.
    let now = chrono::Utc::now().timestamp();
    properties.last_probed_timestamp = Some(now);

    let mut success = false;
    let mut not_found = false;
    let is_remote_probe =
        reqwest::Url::parse(probe_url_cow.as_ref()).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    let probe_params = tuliprox_core::utils::ffmpeg::ProbeParams {
        url: probe_url_cow.as_ref(),
        user_agent: user_agent.as_deref(),
        analyze_duration,
        probe_size,
        timeout_secs: ffprobe_timeout,
    };
    let probe_result = if is_remote_probe {
        FfmpegExecutor::new().probe_remote_url(client, &probe_params).await
    } else {
        FfmpegExecutor::new().probe_url(&probe_params, config.proxy.as_ref()).await
    };
    match probe_result {
        ProbeUrlOutcome::Success(_quality, raw_video, raw_audio, stats) => {
            apply_live_probe_success(&mut properties, raw_video, raw_audio, stats, now);
            success = true;

            debug_if_enabled!("Successfully probed Live Stream ID {}", display_id);
        }
        ProbeUrlOutcome::Failed(ProbeFailureKind::NotFound) => {
            warn!("Live stream probe target returned 404 for ID {} (Input: {})", display_id, input.name);
            not_found = true;
        }
        ProbeUrlOutcome::Failed(ProbeFailureKind::Other) => {
            warn!("Probe failed for Live Stream ID {} (Input: {})", display_id, input.name);
            // We still persist the updated last_probed_timestamp so we don't retry immediately
        }
        ProbeUrlOutcome::Failed(ProbeFailureKind::Cancelled) => {
            warn!("Probe cancelled for Live Stream ID {} (Input: {})", display_id, input.name);
            // We still persist the updated last_probed_timestamp so we don't retry immediately
        }
    }

    // 4. Persist
    if save {
        if let Some(stream_id) = stream_id_opt {
            persist_input_live_info(
                app_config,
                &storage_path,
                XtreamCluster::Live,
                &input.name,
                stream_id,
                &properties,
            )
            .await
            .map_err(|e| shared::error::TuliproxError::Io(format!("Persist error: {e}")))?;
        }
    }

    if !success {
        if not_found {
            return Err(shared::error::TuliproxError::Probe(format!(
                "Probe failed with 404 Not Found for stream {display_id}"
            )));
        }
        // Return error to propagate failure up to task manager/logs
        return Err(shared::error::TuliproxError::Probe(format!("Probe failed for stream {display_id}")));
    }

    Ok(Some(properties))
}

fn apply_live_probe_success(
    properties: &mut LiveStreamProperties,
    raw_video: Option<serde_json::Value>,
    raw_audio: Option<serde_json::Value>,
    stats: ProbeStreamStats,
    now: i64,
) {
    if let Some(video) = raw_video {
        properties.video = Some(video.to_string().into());
    }
    if let Some(audio) = raw_audio {
        properties.audio = Some(audio.to_string().into());
    }
    if let Some(bitrate) = stats.bitrate.filter(|bitrate| *bitrate > 0) {
        properties.bitrate = properties.bitrate.max(bitrate);
    }
    properties.last_success_timestamp = Some(now);
}

#[cfg(test)]
mod tests {
    use super::apply_live_probe_success;
    use serde_json::json;
    use shared::model::LiveStreamProperties;
    use tuliprox_core::utils::ffmpeg::ProbeStreamStats;

    #[test]
    fn apply_live_probe_success_persists_positive_bitrate() {
        let mut properties = LiveStreamProperties { bitrate: 1_500_000, ..LiveStreamProperties::default() };

        apply_live_probe_success(
            &mut properties,
            Some(json!({ "codec_name": "h264" })),
            Some(json!({ "codec_name": "aac" })),
            ProbeStreamStats { duration_secs: None, bitrate: Some(2_500_000) },
            123,
        );

        assert_eq!(properties.bitrate, 2_500_000);
        assert_eq!(properties.last_success_timestamp, Some(123));
        assert!(properties.video.is_some());
        assert!(properties.audio.is_some());

        apply_live_probe_success(
            &mut properties,
            None,
            None,
            ProbeStreamStats { duration_secs: None, bitrate: Some(2_000_000) },
            124,
        );
        assert_eq!(properties.bitrate, 2_500_000);
        assert_eq!(properties.last_success_timestamp, Some(124));
    }
}
