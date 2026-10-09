use super::playlist_mem_cache::{
    PlaylistM3uStorage, PlaylistStorageState, PlaylistXtreamStorage, TargetPlaylistStorage,
};
use std::sync::Arc;

struct LocalEpisodeKey {
    path: Arc<str>,
    virtual_id: u32,
}

#[cfg(test)]
mod tests;

mod episode;
mod input;
mod persist;
mod publication;
mod target;
#[cfg(test)]
use episode::{
    assign_local_series_info_episode_key, assign_media_server_series_info_episode,
    materialize_media_server_series_info_episodes, normalize_target_playlist_epg_ids,
    rewrite_local_series_info_episode_virtual_id, rewrite_series_episode_parent_virtual_ids,
    rewrite_series_info_episode_virtual_id,
};
pub use episode::{rewrite_provider_series_info_episode_virtual_id, ProviderEpisodeKey};
#[cfg(test)]
use input::skipped_clusters;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use input::{
    get_input_local_library_playlist_file_path, get_input_m3u_playlist_file_path,
    get_input_media_server_playlist_file_path, load_input_playlist, load_input_stalker_playlist,
    persist_input_playlist, persist_input_playlist_with_options,
};
#[cfg(test)]
use persist::persist_playlist_with_mode;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use persist::{
    load_input_media_server_playlist, persist_input_media_server_playlist, persist_playlist, persist_playlist_views,
};
#[cfg(test)]
use publication::TargetCacheReloadStage;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use publication::{
    InputPlaylistPersistOptions, LibraryEmptyPublication, PlaylistPublicationPlan, TargetPlaylistPersistOptions,
};
pub use target::{get_target_id_mapping, load_m3u_target_storage, load_xtream_target_storage};
