use crate::{
    model::{AppConfig, ConfigInput},
    repository::{load_input_media_server_playlist_item_by_provider_id, persist_input_media_server_vod_info_batch},
};
use shared::{
    error::TuliproxError,
    model::{PlaylistItemType, StreamProperties, VideoStreamProperties, XtreamPlaylistItem},
};
use std::{io, path::Path, sync::Arc};

pub async fn load_media_server_vod_item_and_properties(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    input: &ConfigInput,
    provider_id: &str,
) -> Result<Option<(XtreamPlaylistItem, VideoStreamProperties)>, TuliproxError> {
    if !input.input_type.is_media_server() {
        return Ok(None);
    }

    let Some(item) = load_input_media_server_playlist_item_by_provider_id(
        app_config,
        storage_path,
        &input.name,
        provider_id,
        PlaylistItemType::Video,
    )
    .await?
    else {
        return Ok(None);
    };

    let properties = item.additional_properties.as_ref().and_then(|properties| match properties {
        StreamProperties::Video(video) => Some(video.as_ref().clone()),
        StreamProperties::Live(_) | StreamProperties::Series(_) | StreamProperties::Episode(_) => None,
    });

    Ok(properties.map(|properties| (item, properties)))
}

pub async fn persist_media_server_vod_info_batch_for_input(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    input: &ConfigInput,
    updates: Vec<(Arc<str>, VideoStreamProperties)>,
) -> Result<(), io::Error> {
    if !input.input_type.is_media_server() || updates.is_empty() {
        return Ok(());
    }

    persist_input_media_server_vod_info_batch(app_config, storage_path, &input.name, updates).await
}
