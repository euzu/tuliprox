//! Loads persisted playlists into the in-memory cache.
//!
//! These two functions read a target's stored playlist and write it into
//! `PlaylistStorageState`. They used to take `AppState` and reach it through
//! `AppState::cache_playlist`, which is what kept them out of this crate; taking
//! the cache directly puts them beside both halves of what they do.

use crate::{
    load_m3u_target_storage, load_xtream_target_storage,
    playlist_mem_cache::{PlaylistStorage, PlaylistStorageState},
};
use log::info;
use shared::error::TuliproxError;
use std::sync::Arc;
use tuliprox_core::model::{AppConfig, ConfigTarget, TargetOutput};

pub async fn load_playlists_into_memory_cache(
    app_config: &AppConfig,
    playlists: &PlaylistStorageState,
) -> Result<(), TuliproxError> {
    for sources in &app_config.sources.load().sources {
        for target in &sources.targets {
            load_target_into_memory_cache(app_config, playlists, target).await;
        }
    }
    Ok(())
}

pub async fn load_target_into_memory_cache(
    app_config: &AppConfig,
    playlists: &PlaylistStorageState,
    target: &Arc<ConfigTarget>,
) {
    if target.use_memory_cache {
        info!("Loading target {} into memory cache", target.name);
        for output in &target.output {
            match output {
                TargetOutput::Xtream(_) => {
                    if let Ok(storage) = load_xtream_target_storage(app_config, target).await {
                        playlists
                            .cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage)))
                            .await;
                    }
                }
                TargetOutput::M3u(_) => {
                    if let Ok(storage) = load_m3u_target_storage(app_config, target).await {
                        playlists.cache_playlist(&target.name, PlaylistStorage::M3uPlaylist(Box::new(storage))).await;
                    }
                }
                _ => {}
            }
        }
    }
}
