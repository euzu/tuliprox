use crate::api::model::ActiveProviderManager;
use crate::model::ConfigInput;
use crate::model::{AppConfig};
use crate::repository::{get_input_m3u_playlist_file_path, get_input_storage_path, get_input_local_library_playlist_file_path, xtream_get_file_path, BPlusTreeUpdate};
use crate::utils::{debug_if_enabled, ffmpeg};
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
) -> Result<(), TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;

    // Check if probing is enabled globally
    let ffprobe_enabled = app_config.is_ffprobe_enabled().await;
    if !ffprobe_enabled {
        return Ok(());
    }

    // Determine storage file path based on input type
    let storage_path = get_input_storage_path(&input.name, working_dir).await
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
                return Ok(());
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

    let acquired_handle = if active_handle.is_some() {
        None
    } else {
        active_provider.acquire_connection_for_probe(&input.name).await
    };

    if active_handle.is_none() && acquired_handle.is_none() {
        warn!("Skipping probe for generic stream {unique_id} due to connection limits");
        return Err(shared::error::info_err!("No connection available"));
    }

    let probe_url = stream_url.to_string();
    let ffprobe_timeout = app_config.config.load().video.as_ref().and_then(|v| v.ffprobe_timeout).unwrap_or(60);
    let user_agent = app_config.config.load().default_user_agent.clone();
    let analyze_duration = 10_000_000;
    let probe_size = 10_000_000;

    debug_if_enabled!("Probing Generic Stream '{unique_id}'");

    let probe_data = ffmpeg::probe_url(
        &probe_url,
        user_agent.as_deref(),
        analyze_duration,
        probe_size,
        ffprobe_timeout,
    ).await;

    if let Some(handle) = acquired_handle {
        active_provider.release_handle(&handle).await;
    }

    let Some((_quality, raw_video, raw_audio)) = probe_data else {
         warn!("Probe failed or timed out for generic stream: {unique_id}");
         return Ok(());
    };

    // Acquire lock and open a tree for update
    let _file_lock = app_config.file_locks.write_lock(&db_path).await;

    // Update the record in BPlusTree
    match storage_kind {
        ProbeStorageKind::M3u => {
            let key: Arc<str> = unique_id.into();
            let mut tree_update = BPlusTreeUpdate::<Arc<str>, M3uPlaylistItem>::try_new(&db_path)
                .map_err(|e| shared::error::info_err!("Failed to open M3U tree update: {e}"))?;

            if let Some(mut item) = tree_update
                .query(&key)
                .map_err(|e| shared::error::info_err!("Tree query error: {e}"))?
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
                    .update(&key, item)
                    .map_err(|e| shared::error::info_err!("Tree update error: {e}"))?;
                info!("Successfully updated M3U metadata for: {unique_id}");
            } else {
                warn!("Item not found in M3U DB: {unique_id}");
            }
        }
        ProbeStorageKind::Library => {
            let mut tree_update = BPlusTreeUpdate::<UUIDType, XtreamPlaylistItem>::try_new(&db_path)
                .map_err(|e| shared::error::info_err!("Failed to open Library tree update: {e}"))?;
            let uuid = UUIDType::from_valid_uuid(unique_id);

            if let Some(mut item) = tree_update
                .query(&uuid)
                .map_err(|e| shared::error::info_err!("Tree query error: {e}"))?
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
                    .update(&uuid, item)
                    .map_err(|e| shared::error::info_err!("Tree update error: {e}"))?;
                info!("Successfully updated Library metadata for: {unique_id}");
            } else {
                warn!("Item not found in Library DB: {unique_id}");
            }
        }
        ProbeStorageKind::Xtream => {
            let Ok(provider_id) = unique_id.parse::<u32>() else {
                warn!("Skipping xtream generic probe update with non-numeric id: {unique_id}");
                return Ok(());
            };

            let mut tree_update = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new(&db_path)
                .map_err(|e| shared::error::info_err!("Failed to open Xtream tree update: {e}"))?;

            if let Some(mut item) = tree_update
                .query(&provider_id)
                .map_err(|e| shared::error::info_err!("Tree query error: {e}"))?
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
                    .map_err(|e| shared::error::info_err!("Tree update error: {e}"))?;
                info!("Successfully updated Xtream metadata for: {unique_id}");
            } else {
                warn!("Item not found in Xtream DB: {unique_id}");
            }
        }
    }

    Ok(())
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
