use crate::api::model::ActiveProviderManager;
use crate::model::ConfigInput;
use crate::model::{AppConfig};
use crate::processing::processor::{select_cancel_token, ProbeHandleGuard};
use crate::repository::{get_input_m3u_playlist_file_path, get_input_storage_path, get_input_local_library_playlist_file_path, xtream_get_file_path, BPlusTreeUpdate};
use crate::utils::debug_if_enabled;
use crate::utils::ffmpeg::{FfmpegExecutor, ProbeFailureKind, ProbeUrlOutcome};
use log::{info, warn};
use shared::error::TuliproxError;
use shared::model::{EpisodeStreamProperties, InputType, PlaylistItemType, StreamProperties, VideoStreamDetailProperties, VideoStreamProperties, LiveStreamProperties, M3uPlaylistItem, XtreamCluster, XtreamPlaylistItem};
use std::sync::Arc;
use shared::model::UUIDType;

enum ProbeStorageKind {
    M3u,
    Library,
    Xtream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericProbeOutcome {
    Updated,
    Noop,
    ProbeFailed,
}

fn requires_provider_connection_for_generic_probe(input_type: InputType) -> bool {
    !matches!(input_type, InputType::Library)
}

/// Updates metadata (Probing) for a stream URL (M3U, Xtream, Library) and persists it.
/// - `unique_id`: For M3U this is the `provider_id` (String). For Library this is the `UUID` string.
///   For Xtream this is the numeric provider id as string.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn update_generic_stream_metadata(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    unique_id: &str,
    stream_url: &str,
    item_type: PlaylistItemType,
    active_provider: &Arc<ActiveProviderManager>,
    active_handle: Option<&crate::api::model::ProviderHandle>,
    probe_priority: i8,
) -> Result<GenericProbeOutcome, TuliproxError> {
    let storage_dir = &app_config.config.load().storage_dir;

    // Check if probing is enabled globally
    let ffprobe_enabled = app_config.is_ffprobe_enabled().await;
    if !ffprobe_enabled {
        return Ok(GenericProbeOutcome::Noop);
    }

    // Determine storage file path based on input type
    let storage_path = get_input_storage_path(&input.name, storage_dir).await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    let (db_path, storage_kind) = match input.input_type {
        InputType::M3u | InputType::M3uBatch => (
            get_input_m3u_playlist_file_path(&storage_path, &input.name),
            ProbeStorageKind::M3u,
        ),
        InputType::Library => (
            get_input_local_library_playlist_file_path(&storage_path, &input.name),
            ProbeStorageKind::Library,
        ),
        InputType::Xtream | InputType::XtreamBatch => {
            let cluster = if item_type.is_live() {
                XtreamCluster::Live
            } else if matches!(item_type, PlaylistItemType::Video | PlaylistItemType::LocalVideo) {
                XtreamCluster::Video
            } else if matches!(item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries) {
                XtreamCluster::Series
            } else {
                // Generic probing currently supports live/video/series payload shapes.
                return Ok(GenericProbeOutcome::Noop);
            };
            (
                xtream_get_file_path(&storage_path, cluster),
                ProbeStorageKind::Xtream,
            )
        }
    };

    if !db_path.exists() {
        return Err(shared::error::info_err!("Playlist DB file not found for input {}: {}", input.name, db_path.display()));
    }

    let needs_provider_connection = requires_provider_connection_for_generic_probe(input.input_type);

    let acquired_handle = if !needs_provider_connection || active_handle.is_some() {
        None
    } else {
        active_provider
            .acquire_connection_for_probe(&input.name, probe_priority)
            .await
            .map(|handle| ProbeHandleGuard::new(active_provider, handle))
    };

    if needs_provider_connection && active_handle.is_none() && acquired_handle.is_none() {
        warn!("Skipping probe for generic stream {unique_id} due to connection limits");
        return Err(shared::error::info_err!("No connection available"));
    }

    let probe_url = stream_url.to_string();
    let config = app_config.config.load();
    let metadata_update = config.metadata_update.clone().unwrap_or_default();
    let ffprobe_timeout = metadata_update.ffprobe.timeout.unwrap_or(60);
    let user_agent = config.default_user_agent.clone();
    let (analyze_duration, probe_size) = if item_type.is_live() {
        (
            metadata_update.ffprobe.live_analyze_duration_micros,
            metadata_update.ffprobe.live_probe_size_bytes,
        )
    } else {
        (
            metadata_update.ffprobe.analyze_duration_micros,
            metadata_update.ffprobe.probe_size_bytes,
        )
    };

    debug_if_enabled!("Probing Generic Stream '{unique_id}'");

    let cancel_token = select_cancel_token(
        acquired_handle.as_ref().and_then(ProbeHandleGuard::handle),
        active_handle,
    );
    let probe_data = FfmpegExecutor::new().probe_url_with_cancel(
        &probe_url,
        user_agent.as_deref(),
        analyze_duration,
        probe_size,
        ffprobe_timeout,
        config.proxy.as_ref(),
        cancel_token,
    )
    .await;

    if let Some(handle) = acquired_handle {
        handle.release().await;
    }

    let (raw_video, raw_audio) = match probe_data {
        ProbeUrlOutcome::Success(_quality, raw_video, raw_audio) => (raw_video, raw_audio),
        ProbeUrlOutcome::Failed(ProbeFailureKind::NotFound) => {
            warn!("Probe target not found (404) for generic stream: {unique_id}");
            return Err(shared::error::info_err!("Probe target returned 404 Not Found for stream {unique_id}"));
        }
        ProbeUrlOutcome::Failed(ProbeFailureKind::Other) => {
            warn!("Probe failed or timed out for generic stream: {unique_id}");
            return Ok(GenericProbeOutcome::ProbeFailed);
        }
        ProbeUrlOutcome::Failed(ProbeFailureKind::Cancelled) => {
            warn!("Probe cancelled for generic stream: {unique_id}");
            return Ok(GenericProbeOutcome::ProbeFailed);
        }
    };

    // Hold the async file lock while the blocking DB update runs in a blocking thread.
    let file_lock = app_config.file_locks.write_lock(&db_path).await;
    let db_path_for_update = db_path.clone();
    let unique_id_for_update = unique_id.to_string();
    let updated = tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let mut updated = false;
        match storage_kind {
            ProbeStorageKind::M3u => {
                let key: Arc<str> = Arc::from(unique_id_for_update.as_str());
                let mut tree_update = BPlusTreeUpdate::<Arc<str>, M3uPlaylistItem>::try_new(&db_path_for_update)
                    .map_err(|e| format!("Failed to open M3U tree update: {e}"))?;

                if let Some(mut item) = tree_update.query(&key).map_err(|e| format!("Tree query error: {e}"))? {
                    update_properties(
                        &mut item.additional_properties,
                        item_type,
                        &item.name,
                        item.virtual_id,
                        raw_video,
                        raw_audio,
                    );
                    tree_update
                        .update(&key, item)
                        .map_err(|e| format!("Tree update error: {e}"))?;
                    info!("Successfully updated M3U metadata for: {unique_id_for_update}");
                    updated = true;
                } else {
                    warn!("Item not found in M3U DB: {unique_id_for_update}");
                }
            }
            ProbeStorageKind::Library => {
                let mut tree_update = BPlusTreeUpdate::<UUIDType, XtreamPlaylistItem>::try_new(&db_path_for_update)
                    .map_err(|e| format!("Failed to open Library tree update: {e}"))?;
                let uuid = UUIDType::from_valid_uuid(&unique_id_for_update);

                if let Some(mut item) = tree_update.query(&uuid).map_err(|e| format!("Tree query error: {e}"))? {
                    update_properties(
                        &mut item.additional_properties,
                        item_type,
                        &item.name,
                        item.virtual_id,
                        raw_video,
                        raw_audio,
                    );
                    tree_update
                        .update(&uuid, item)
                        .map_err(|e| format!("Tree update error: {e}"))?;
                    info!("Successfully updated Library metadata for: {unique_id_for_update}");
                    updated = true;
                } else {
                    warn!("Item not found in Library DB: {unique_id_for_update}");
                }
            }
            ProbeStorageKind::Xtream => {
                let Ok(provider_id) = unique_id_for_update.parse::<u32>() else {
                    warn!("Skipping xtream generic probe update with non-numeric id: {unique_id_for_update}");
                    return Ok(false);
                };

                let mut tree_update = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new(&db_path_for_update)
                    .map_err(|e| format!("Failed to open Xtream tree update: {e}"))?;

                if let Some(mut item) = tree_update
                    .query(&provider_id)
                    .map_err(|e| format!("Tree query error: {e}"))?
                {
                    update_properties(
                        &mut item.additional_properties,
                        item_type,
                        &item.name,
                        item.virtual_id,
                        raw_video,
                        raw_audio,
                    );
                    tree_update
                        .update(&provider_id, item)
                        .map_err(|e| format!("Tree update error: {e}"))?;
                    info!("Successfully updated Xtream metadata for: {unique_id_for_update}");
                    updated = true;
                } else {
                    warn!("Item not found in Xtream DB: {unique_id_for_update}");
                }
            }
        }

        Ok(updated)
    })
    .await
    .map_err(|e| shared::error::info_err!("Failed to join generic probe DB update task: {e}"))?
    .map_err(|e| shared::error::info_err!("{e}"))?;

    drop(file_lock);
    if updated {
        Ok(GenericProbeOutcome::Updated)
    } else {
        Ok(GenericProbeOutcome::Noop)
    }
}

fn update_properties(
    props_opt: &mut Option<StreamProperties>, 
    item_type: PlaylistItemType, 
    name: &str, 
    virtual_id: u32,
    raw_video: Option<serde_json::Value>, 
    raw_audio: Option<serde_json::Value>
) {
    if matches!(item_type, PlaylistItemType::Video | PlaylistItemType::LocalVideo) {
       let mut props = if let Some(StreamProperties::Video(p)) = props_opt {
           *p.clone()
       } else {
           VideoStreamProperties {
               name: name.into(),
               stream_id: virtual_id,
               container_extension: "".into(),
               ..Default::default()
           }
       };

       if props.details.is_none() {
           props.details = Some(VideoStreamDetailProperties::default());
       }
       if let Some(details) = props.details.as_mut() {
           if let Some(v) = raw_video {
               details.video = Some(v.to_string().into());
           }
           if let Some(a) = raw_audio {
               details.audio = Some(a.to_string().into());
           }
       }
       *props_opt = Some(StreamProperties::Video(Box::new(props)));
    }
    else if matches!(item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries) {
       let mut props = if let Some(StreamProperties::Episode(p)) = props_opt {
           *p.clone()
       } else {
           EpisodeStreamProperties {
               episode_id: virtual_id,
               episode: 0,
               season: 0,
               added: None,
               release_date: None,
               series_release_date: None,
               tmdb: None,
               movie_image: "".into(),
               container_extension: "".into(),
               video: None,
               audio: None,
           }
       };

       if let Some(v) = raw_video {
           props.video = Some(v.to_string().into());
       }
       if let Some(a) = raw_audio {
           props.audio = Some(a.to_string().into());
       }
       *props_opt = Some(StreamProperties::Episode(Box::new(props)));
    }
    else if matches!(item_type, PlaylistItemType::Live | PlaylistItemType::LiveHls | PlaylistItemType::LiveDash) {
       let mut props = if let Some(StreamProperties::Live(p)) = props_opt {
           *p.clone()
       } else {
           LiveStreamProperties {
               name: name.into(),
               stream_id: virtual_id,
               ..LiveStreamProperties::default()
           }
       };
       
       if let Some(v) = raw_video {
           props.video = Some(v.to_string().into());
       }
       if let Some(a) = raw_audio {
           props.audio = Some(a.to_string().into());
       }

       let now = chrono::Utc::now().timestamp();
       props.last_probed_timestamp = Some(now);
       props.last_success_timestamp = Some(now);
       
       *props_opt = Some(StreamProperties::Live(Box::new(props)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_probe_does_not_require_provider_connection() {
        assert!(!requires_provider_connection_for_generic_probe(InputType::Library));
    }

    #[test]
    fn m3u_probe_requires_provider_connection() {
        assert!(requires_provider_connection_for_generic_probe(InputType::M3u));
        assert!(requires_provider_connection_for_generic_probe(
            InputType::M3uBatch
        ));
    }

    #[test]
    fn xtream_probe_requires_provider_connection() {
        assert!(requires_provider_connection_for_generic_probe(
            InputType::Xtream
        ));
        assert!(requires_provider_connection_for_generic_probe(
            InputType::XtreamBatch
        ));
    }
}
