use crate::api::model::create_http_client;
use crate::library::metadata::{EpisodeMetadata, MediaMetadata, MetadataCacheEntry};
use crate::library::metadata_resolver::MetadataResolver;
use crate::library::metadata_storage::MetadataStorage;
use crate::library::scanner::LibraryScanner;
use crate::library::{MediaGroup, MediaGrouper, thumbnail::{self, ThumbnailExtractor}};
use crate::model::{AppConfig, LibraryConfig, MetadataUpdateConfig};
use crate::utils::ffmpeg::FfmpegExecutor;
use log::{debug, error, info, warn};
use path_clean::PathClean;
use shared::model::{LibraryMetadataFormat, LibraryScanResult};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// Action taken when processing a file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessAction {
    Added,
    Updated,
    Unchanged,
}

// VOD processor that orchestrates scanning, classification, metadata resolution, and storage
pub struct LibraryProcessor {
    config: LibraryConfig,
    scanner: LibraryScanner,
    resolver: MetadataResolver,
    storage: MetadataStorage,
    thumbnail_extractor: Option<ThumbnailExtractor>,
    app_config: Option<Arc<AppConfig>>, // Need access to global config for FFprobe settings
}

pub fn resolve_metadata_storage_path(
    metadata_update_config: Option<&MetadataUpdateConfig>,
    storage_dir: &str,
) -> PathBuf {
    let configured_path = metadata_update_config.map_or_else(
        || PathBuf::from(shared::utils::default_metadata_path()),
        |c| {
            if c.cache_path.is_empty() {
                PathBuf::from(shared::utils::default_metadata_path())
            } else {
                PathBuf::from(c.cache_path.clone())
            }
        },
    );
    if configured_path.is_absolute() {
        configured_path.clean()
    } else {
        PathBuf::from(storage_dir).join(configured_path).clean()
    }
}

impl LibraryProcessor {
    // Creates a new Library processor from application config
    pub fn from_app_config(app_config: &AppConfig) -> Option<Self> {
        let Ok(client) = create_http_client(app_config) else {
            error!("Failed to create HTTP client for LibraryProcessor, skipping library scan. Please check your configuration.");
            return None;
        };
        let config = app_config.config.load();
        config
            .library
            .as_ref()
            .map(|lib_cfg| {
                let mut processor =
                    Self::new(lib_cfg.clone(), config.metadata_update.as_ref(), client, &config.storage_dir);
                processor.app_config = Some(Arc::new(app_config.clone()));
                processor
            })
    }

    // Creates a new Library processor with the given configuration
    pub fn new(
        config: LibraryConfig,
        metadata_update_config: Option<&MetadataUpdateConfig>,
        client: reqwest::Client,
        storage_dir: &str,
    ) -> Self {
        let storage_path = resolve_metadata_storage_path(metadata_update_config, storage_dir);
        let scanner = LibraryScanner::new(config.clone());
        let storage = MetadataStorage::new(storage_path);
        let resolver = MetadataResolver::from_config(Some(&config), metadata_update_config, client, Some(storage.clone()));

        let thumbnail_extractor = if config.thumbnails.enabled {
            Some(ThumbnailExtractor::new(config.thumbnails.clone()))
        } else {
            None
        };

        Self {
            config,
            scanner,
            resolver,
            storage,
            thumbnail_extractor,
            app_config: None,
        }
    }

    // Performs a full Library scan
    pub async fn scan(&self, force_rescan: bool) -> Result<LibraryScanResult, std::io::Error> {
        info!("Starting Library scan (force_rescan: {force_rescan})");

        // Initialize storage
        self.storage.initialize().await?;

        // Load existing metadata cache
        let existing_entries = self.storage.load_all().await;
        let existing_map: HashMap<_, _> = existing_entries
            .iter()
            .map(|e| (e.file_path.clone(), e.clone()))
            .collect();

        // Scan for video files
        let scanned_files = self.scanner.scan_all().await?;
        let scanned_files_count = scanned_files.len();
        info!("Scanned {scanned_files_count} video files");
        let media_groups = MediaGrouper::group(scanned_files);
        info!("Scanned {} file groups", media_groups.len());

        let mut result = LibraryScanResult {
            files_scanned: scanned_files_count,
            groups_scanned: media_groups.len(),
            files_added: 0,
            files_updated: 0,
            files_removed: 0,
            errors: 0,
        };

        // Check global ffprobe config
        let ffprobe_enabled = if let Some(app_cfg) = &self.app_config {
            app_cfg.is_ffprobe_enabled().await
        } else {
            false
        };

        let ffmpeg_available = if self.thumbnail_extractor.is_some() {
            if let Some(app_cfg) = &self.app_config {
                app_cfg.is_ffmpeg_available().await
            } else {
                FfmpegExecutor::new().check_ffmpeg_availability().await
            }
        } else {
            false
        };

        if self.thumbnail_extractor.is_some() && !ffmpeg_available {
            warn!("Thumbnail extraction disabled because ffmpeg is unavailable");
        }

        // Process each scanned file
        for group in &media_groups {
            match self.process_group(group, &existing_map, force_rescan, ffprobe_enabled, ffmpeg_available).await {
                Ok(action) => match action {
                    ProcessAction::Added => result.files_added += 1,
                    ProcessAction::Updated => result.files_updated += 1,
                    ProcessAction::Unchanged => {}
                },
                Err(e) => {
                    error!("Error processing {group}: {e}");
                    result.errors += 1;
                }
            }
        }

        // Cleanup orphaned entries (files that no longer exist)
        let scanned_paths: std::collections::HashSet<_> = media_groups
            .iter()
            .flat_map(|group| match group {
                MediaGroup::Movie { file, .. } => vec![file.file_path.as_str()],
                MediaGroup::Series { episodes, .. } => episodes.iter().map(|ep| ep.file.file_path.as_str()).collect(),
            })
            .collect();

        for entry in existing_entries {
            if !scanned_paths.contains(entry.file_path.as_str()) {
                debug!("Removing orphaned entry for: {}", entry.file_path);
                if let Err(e) = self.storage.delete_by_uuid(&entry.uuid).await {
                    error!("Failed to delete orphaned entry: {e}");
                } else {
                    result.files_removed += 1;
                }
            }
        }

        if ffmpeg_available {
            self.storage.cleanup_orphaned_thumbnails().await;
        }

        info!("Library scan completed: {result:?}");
        Ok(result)
    }

    async fn process_group(
        &self,
        group: &MediaGroup,
        existing_map: &HashMap<String, MetadataCacheEntry>,
        force_rescan: bool,
        can_probe: bool,
        can_extract_thumbnails: bool,
    ) -> Result<ProcessAction, String> {
        match group {
            MediaGroup::Movie { file: _, .. } => {
                self.process_movie(group, existing_map, force_rescan, can_probe, can_extract_thumbnails).await
            }
            MediaGroup::Series { show_key: _, episodes: _ } => {
                self.process_series_group(group, existing_map, force_rescan, can_probe, can_extract_thumbnails).await
            }
        }
    }

    // TODO: Implement enrich_metadata_with_ffprobe to add technical info (resolution, codecs) from local files
    //fn enrich_metadata_with_ffprobe(&self, _metadata: &mut MediaMetadata, _file_path: &str, _can_probe: bool) {
        //if !can_probe { return; }

        // let _url = format!("file://{file_path}"); // Simple file URL for ffmpeg
        //
        // // TODO: Logic for series episodes iteration
        //  match metadata {
        //      MediaMetadata::Movie(_movie) => {
        //          // Currently we don't have fields in MovieMetadata to store tech info
        //          // But we could add them. For now, let's just log.
        //          // In the future this should update the metadata struct.
        //          debug!("Probe logic for local movie {} not yet fully integrated into Metadata struct", file_path);
        //      }
        //      MediaMetadata::Series(_) => {
        //          // Series handle episodes separately
        //      }
        //  }
    //}

    // Processes a single video file
    async fn process_movie(
        &self,
        group: &MediaGroup,
        existing_map: &HashMap<String, MetadataCacheEntry>,
        force_rescan: bool,
        _can_probe: bool,
        can_extract_thumbnails: bool,
    ) -> Result<ProcessAction, String> {
        let MediaGroup::Movie { file, .. } = group else { return Err(format!("Expected movie to resolve but got {group}")) };
        // Check if file already exists in cache
        let (mut cache_entry, status) = if let Some(existing_entry) = existing_map.get(&file.file_path) {
            // Check if file has been modified
            if !force_rescan && !existing_entry.is_file_modified(file, 0, 0) {
                debug!("File unchanged, skipping: {}", file.file_path);
                return Ok(ProcessAction::Unchanged);
            }

            debug!("File modified, updating metadata: {}", file.file_path);
            // Reuse existing UUID
            let metadata = self.resolve_metadata(group).await?;
            //self.enrich_metadata_with_ffprobe(&mut metadata, &file.file_path, can_probe);

            let entry = MetadataCacheEntry {
                uuid: existing_entry.uuid.clone(),
                file_path: file.file_path.clone(),
                file_size: file.size_bytes,
                file_modified: file.modified_timestamp,
                metadata,
                thumbnail_hash: existing_entry.thumbnail_hash.clone(),
                thumbnail_mtime: existing_entry.thumbnail_mtime,
            };

            (entry, ProcessAction::Updated)
        } else {
            debug!("New file, resolving metadata: {}", file.file_path);
            let metadata = self.resolve_metadata(group).await?;
            //self.enrich_metadata_with_ffprobe(&mut metadata, &file.file_path, can_probe);

            let entry = MetadataCacheEntry::new(
                file.file_path.clone(),
                file.size_bytes,
                file.modified_timestamp,
                metadata,
            );

            (entry, ProcessAction::Added)
        };

        self.extract_thumbnail_if_needed(
            &mut cache_entry,
            &file.file_path,
            file.modified_timestamp,
            can_extract_thumbnails,
        ).await;
        self.storage.store(&cache_entry).await.map_err(|e| e.to_string())?;
        self.write_metadata_files(&cache_entry).await.map_err(|e| e.to_string())?;
        Ok(status)
    }


    #[allow(clippy::too_many_lines)]
    async fn process_series_group(
        &self,
        group: &MediaGroup,
        existing_map: &HashMap<String, MetadataCacheEntry>,
        force_rescan: bool,
        _can_probe: bool,
        can_extract_thumbnails: bool,
    ) -> Result<ProcessAction, String> {
        let MediaGroup::Series { show_key, episodes } = group else { return Err(format!("Expected series to resolve but got {group}")) };
        let series_file_path = episodes
            .iter()
            .find_map(|episode| {
                if episode.file.file_path.is_empty() {
                    None
                } else {
                    Some(episode.file.file_path.clone())
                }
            })
            .unwrap_or_else(|| show_key.to_string());

        // Build a map of existing per-episode thumbnail state so it can be
        // carried forward when the series metadata is rebuilt from scratch.
        let mut existing_ep_thumbs: HashMap<(u32, u32), (Option<String>, i64)> = HashMap::new();

        // Check if file already exists in cache
        let (mut chache_entry, status) = if let Some(existing_entry) = existing_map.get(&series_file_path) {
            if !force_rescan {
                // Check if file has been modified
                if !episodes.iter().any(|episode| existing_entry.is_file_modified(&episode.file, episode.season, episode.episode)) {
                    debug!("File unchanged, skipping: {show_key}");
                    return Ok(ProcessAction::Unchanged);
                }
            }

            debug!("File modified, updating metadata: {show_key}");
            // Preserve existing per-episode thumbnail state before rebuilding
            if let MediaMetadata::Series(ref existing_series) = existing_entry.metadata {
                if let Some(ref eps) = existing_series.episodes {
                    for ep in eps {
                        existing_ep_thumbs.insert(
                            (ep.season, ep.episode),
                            (ep.thumbnail_id.clone(), ep.file_modified),
                        );
                    }
                }
            }
            // Reuse existing UUID
            let metadata = self.resolve_metadata(group).await?;

            let entry = MetadataCacheEntry {
                uuid: existing_entry.uuid.clone(),
                file_path: series_file_path,
                file_size: 0,
                file_modified: 0,
                metadata,
                thumbnail_hash: existing_entry.thumbnail_hash.clone(),
                thumbnail_mtime: existing_entry.thumbnail_mtime,
            };
            (entry, ProcessAction::Updated)
        } else {
            debug!("New series, resolving metadata: {show_key}");
            let metadata = self.resolve_metadata(group).await?;

            let entry = MetadataCacheEntry::new(
                series_file_path,
                0,
                0,
                metadata,
            );

            (entry, ProcessAction::Added)
        };

        if let (MediaMetadata::Series(ref mut series_metadata), MediaGroup::Series { episodes, .. }) = (&mut chache_entry.metadata, group) {
            let series_episodes = series_metadata.episodes.get_or_insert_with(|| {
                // No episode list from TMDB/NFO — synthesize stubs from scanned files
                // so that file paths, sizes and timestamps get populated below.
                episodes.iter().map(|ep| EpisodeMetadata {
                    title: ep.metadata.title.clone(),
                    season: ep.season,
                    episode: ep.episode,
                    file_path: String::new(),
                    file_size: 0,
                    file_modified: 0,
                    ..EpisodeMetadata::default()
                }).collect()
            });

            // maybe we have the same episode as 2 different files
            let mut double_episodes = vec![];
            for episode in episodes {
                for series_episode in &mut *series_episodes {
                    if episode.episode == series_episode.episode && episode.season == series_episode.season {
                        let previous_file_modified = series_episode.file_modified;
                        if series_episode.file_path.is_empty() {
                            // Carry forward existing thumbnail state so we don't
                            // re-extract thumbnails that are already cached.
                            let prev_mtime = if let Some((ref existing_thumb_id, existing_mtime)) =
                                existing_ep_thumbs.get(&(episode.season, episode.episode))
                            {
                                series_episode.thumbnail_id.clone_from(existing_thumb_id);
                                Some(*existing_mtime)
                            } else {
                                None
                            };
                            series_episode.file_path.clone_from(&episode.file.file_path);
                            series_episode.file_modified = episode.file.modified_timestamp;
                            series_episode.file_size = episode.file.size_bytes;
                            self.update_episode_thumbnail(
                                series_episode,
                                &episode.file.file_path,
                                episode.file.modified_timestamp,
                                prev_mtime,
                                can_extract_thumbnails,
                            ).await;
                        } else {
                            let mut new_episode = series_episode.clone();
                            new_episode.file_path.clone_from(&episode.file.file_path);
                            new_episode.file_modified = episode.file.modified_timestamp;
                            new_episode.file_size = episode.file.size_bytes;
                            self.update_episode_thumbnail(
                                &mut new_episode,
                                &episode.file.file_path,
                                episode.file.modified_timestamp,
                                Some(previous_file_modified),
                                can_extract_thumbnails,
                            ).await;
                            double_episodes.push(new_episode);
                        }
                    }
                }
            }
            if !double_episodes.is_empty() {
                series_episodes.append(&mut double_episodes);
                series_episodes.sort_by_key(|episode| (episode.season, episode.episode));
            }

            series_metadata.number_of_episodes = u32::try_from(series_episodes.len()).unwrap_or(0);
            if series_metadata.number_of_seasons == 0 {
                series_metadata.number_of_seasons = unique_season_count(series_episodes);
            }
        }

        if let MediaGroup::Series { episodes, .. } = group {
            if let Some(first_ep) = episodes.first() {
                self.extract_thumbnail_if_needed(
                    &mut chache_entry,
                    &first_ep.file.file_path,
                    first_ep.file.modified_timestamp,
                    can_extract_thumbnails,
                ).await;
            }
        }

        self.storage.store(&chache_entry).await.map_err(|e| e.to_string())?;
        self.write_metadata_files(&chache_entry).await.map_err(|e| e.to_string())?;
        Ok(status)
    }

    // Resolves metadata for a video file
    async fn resolve_metadata(&self, file: &MediaGroup) -> Result<MediaMetadata, String> {
        self.resolver.resolve(file).await.ok_or_else(|| format!("Could not resolve metadata for {file}"))
    }

    /// Extracts and caches a thumbnail if no TMDB poster is available.
    /// Uses mtime-based cache invalidation: re-extracts if source file
    /// has been modified since last extraction.
    async fn extract_thumbnail_if_needed(
        &self,
        cache_entry: &mut MetadataCacheEntry,
        file_path: &str,
        file_mtime: i64,
        can_extract_thumbnails: bool,
    ) {
        if !can_extract_thumbnails {
            return;
        }

        // Skip if already has a poster from TMDB/NFO
        if cache_entry.metadata.poster().is_some() {
            // Clear stale generated-thumbnail references so they can be reclaimed
            cache_entry.thumbnail_hash = None;
            cache_entry.thumbnail_mtime = None;
            return;
        }

        let Some(ref extractor) = self.thumbnail_extractor else { return };

        let hash = thumbnail::file_hash(file_path);

        // Check if we already have a valid cached thumbnail
        if self.storage.has_thumbnail(&hash).await {
            // Re-extract if source file was modified since last extraction
            if cache_entry.thumbnail_mtime == Some(file_mtime) {
                cache_entry.thumbnail_hash = Some(hash);
                return;
            }
            debug!("Source file modified, re-extracting thumbnail: {file_path}");
        }

        match extractor.extract_from_file(file_path).await {
            Ok(data) => {
                if let Err(err) = self.storage.store_thumbnail(&hash, &data).await {
                    error!("Failed to store thumbnail for {file_path}: {err}");
                    return;
                }
                debug!("Extracted thumbnail for: {file_path}");
                cache_entry.thumbnail_hash = Some(hash);
                cache_entry.thumbnail_mtime = Some(file_mtime);
            }
            Err(err) => {
                warn!("Thumbnail extraction failed for {file_path}: {err}");
            }
        }
    }

    async fn update_episode_thumbnail(
        &self,
        episode: &mut EpisodeMetadata,
        file_path: &str,
        file_mtime: i64,
        previous_file_mtime: Option<i64>,
        can_extract_thumbnails: bool,
    ) {
        if !episode.thumb.as_deref().unwrap_or_default().is_empty()
            && episode.thumbnail_id.as_deref().unwrap_or_default().is_empty()
        {
            return;
        }

        if let Some(thumbnail_id) = self.extract_thumbnail_id_for_file(
            file_path,
            file_mtime,
            previous_file_mtime,
            can_extract_thumbnails,
        ).await {
            episode.thumbnail_id = Some(thumbnail_id);
        }
    }

    async fn extract_thumbnail_id_for_file(
        &self,
        file_path: &str,
        file_mtime: i64,
        previous_file_mtime: Option<i64>,
        can_extract_thumbnails: bool,
    ) -> Option<String> {
        if !can_extract_thumbnails {
            return None;
        }

        let extractor = self.thumbnail_extractor.as_ref()?;
        let hash = thumbnail::file_hash(file_path);

        if self.storage.has_thumbnail(&hash).await && previous_file_mtime == Some(file_mtime) {
            return Some(hash);
        }

        match extractor.extract_from_file(file_path).await {
            Ok(data) => {
                if let Err(err) = self.storage.store_thumbnail(&hash, &data).await {
                    error!("Failed to store episode thumbnail for {file_path}: {err}");
                    return None;
                }
                Some(hash)
            }
            Err(err) => {
                warn!("Episode thumbnail extraction failed for {file_path}: {err}");
                None
            }
        }
    }

    // Writes metadata files (JSON, NFO) based on configuration
    async fn write_metadata_files(&self, entry: &MetadataCacheEntry) -> Result<(), std::io::Error> {
        // JSON is always written by storage.store()

        // TODO enrich nfo with all information, we are currently storing a subset, and rebuilding json from nfo ends in information loss!
        // Write NFO if enabled
        if self.config.metadata.formats.contains(&LibraryMetadataFormat::Nfo) {
            if let Err(e) = self.storage.write_nfo(entry).await {
                warn!("Failed to write NFO for {}: {e}", entry.file_path);
            }
        }

        Ok(())
    }

    // Gets all cached metadata entries
    pub async fn get_all_entries(&self) -> Vec<MetadataCacheEntry> {
        self.storage.load_all().await
    }
}

fn unique_season_count(episodes: &[EpisodeMetadata]) -> u32 {
    let mut seasons: Vec<u32> = episodes.iter().map(|episode| episode.season).collect();
    seasons.sort_unstable();
    seasons.dedup();
    u32::try_from(seasons.len()).unwrap_or(0)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_result_creation() {
        let result = LibraryScanResult {
            files_scanned: 100,
            groups_scanned: 0,
            files_added: 50,
            files_updated: 20,
            files_removed: 5,
            errors: 2,
        };

        assert_eq!(result.files_scanned, 100);
        assert_eq!(result.files_added, 50);
    }

    #[test]
    fn test_unique_season_count_handles_unsorted_duplicates() {
        let episodes = vec![
            EpisodeMetadata { season: 2, ..EpisodeMetadata::default() },
            EpisodeMetadata { season: 1, ..EpisodeMetadata::default() },
            EpisodeMetadata { season: 2, ..EpisodeMetadata::default() },
            EpisodeMetadata { season: 3, ..EpisodeMetadata::default() },
        ];

        assert_eq!(unique_season_count(&episodes), 3);
    }
}
