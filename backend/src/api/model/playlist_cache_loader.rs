//! Loads persisted playlists into the in-memory cache.
//!
//! These two functions read through the repository and then write into
//! `AppState`'s playlist cache. That second half is server state, so they belong
//! here rather than in `repository`, where they forced the storage layer to name
//! `AppState`.

use crate::{
    api::model::{AppState, PlaylistStorage},
    model::{ConfigTarget, TargetOutput},
    repository::{load_m3u_target_storage, load_xtream_target_storage},
};
use log::info;
use shared::error::TuliproxError;
use std::sync::Arc;

pub async fn load_playlists_into_memory_cache(app_state: &AppState) -> Result<(), TuliproxError> {
    for sources in &app_state.app_config.sources.load().sources {
        for target in &sources.targets {
            load_target_into_memory_cache(app_state, target).await;
        }
    }
    Ok(())
}

pub async fn load_target_into_memory_cache(app_state: &AppState, target: &Arc<ConfigTarget>) {
    if target.use_memory_cache {
        info!("Loading target {} into memory cache", target.name);
        for output in &target.output {
            match output {
                TargetOutput::Xtream(_) => {
                    if let Ok(storage) = load_xtream_target_storage(&app_state.app_config, target).await {
                        app_state.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;
                    }
                }
                TargetOutput::M3u(_) => {
                    if let Ok(storage) = load_m3u_target_storage(&app_state.app_config, target).await {
                        app_state.cache_playlist(&target.name, PlaylistStorage::M3uPlaylist(Box::new(storage))).await;
                    }
                }
                _ => {}
            }
        }
    }
}
