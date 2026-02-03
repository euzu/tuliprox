use crate::api::model::create_http_client;
use crate::library::metadata::{MediaMetadata, MetadataCacheEntry};
use crate::library::metadata_resolver::MetadataResolver;
use crate::library::metadata_storage::MetadataStorage;
use crate::library::scanner::LibraryScanner;
use crate::library::{MediaGroup, MediaGrouper};
use crate::model::{AppConfig, LibraryConfig};
use log::{debug, error, info, warn};
use shared::model::{LibraryMetadataFormat, LibraryScanResult};
use std::collections::HashMap;
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
    app_config: Option<Arc<AppConfig>>, // Need access to global config for FFprobe settings
}

impl LibraryProcessor {
    // Creates a new Library processor from application config
    pub fn from_app_config(app_config: &AppConfig) -> Option<Self> {
        let Ok(client) = create_http_client(app_config) else {
            error!("Failed to create HTTP client for LibraryProcessor, skipping library scan. Please check your configuration.");
            return None;
        };
        app_config.config.load().library.as_ref().map(|lib_cfg| Self::new(lib_cfg.clone(), client))
    }

    // Creates a new Library processor with the given configuration
    pub fn new(config: LibraryConfig, client: reqwest::Client) -> Self {
        let storage_path = std::path::PathBuf::from(&config.metadata.path);
        let scanner = LibraryScanner::new(config.clone());
        let storage = MetadataStorage::new(storage_path);
        let resolver = MetadataResolver::from_config(&config, client, storage.clone());

        Self {
            config,
            scanner,
            resolver,
            storage,
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

        // Process each scanned file
        for group in &media_groups {
            match self.process_group(group, &existing_map, force_rescan, ffprobe_enabled).await {
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

        info!("Library scan completed: {result:?}");
        Ok(result)
    }

    async fn process_group(&self, group: &MediaGroup, existing_map: &HashMap<String, MetadataCacheEntry>, force_rescan: bool, can_probe: bool) -> Result<ProcessAction, String> {
        match group {
            MediaGroup::Movie { file: _, .. } => {
                self.process_movie(group, existing_map, force_rescan, can_probe).await
            }
            MediaGroup::Series { show_key: _, episodes: _ } => {
                self.process_series_group(group, existing_map, force_rescan, can_probe).await
            }
        }
    }

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
    async fn process_movie(&self, group: &MediaGroup, existing_map: &HashMap<String, MetadataCacheEntry>, force_rescan: bool, _can_probe: bool) -> Result<ProcessAction, String> {
        let MediaGroup::Movie { file, .. } = group else { return Err(format!("Expected movie to resolve but got {group}")) };
        // Check if file already exists in cache
        let (cache_entry, status) = if let Some(existing_entry) = existing_map.get(&file.file_path) {
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

        self.storage.store(&cache_entry).await.map_err(|e| e.to_string())?;
        self.write_metadata_files(&cache_entry).await.map_err(|e| e.to_string())?;
        Ok(status)
    }

    async fn process_series_group(
        &self,
        group: &MediaGroup,
        existing_map: &HashMap<String, MetadataCacheEntry>,
        force_rescan: bool, _can_probe: bool
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
            // Reuse existing UUID
            let metadata = self.resolve_metadata(group).await?;

            let entry = MetadataCacheEntry {
                uuid: existing_entry.uuid.clone(),
                file_path: series_file_path,
                file_size: 0,
                file_modified: 0,
                metadata,
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
            if let Some(series_episodes) = series_metadata.episodes.as_mut() {
                // maybe we have the same episode as 2 different files
                let mut double_episodes = vec![];
                for episode in episodes {
                    for series_episode in &mut *series_episodes {
                        if episode.episode == series_episode.episode && episode.season == series_episode.season {
                            if series_episode.file_path.is_empty() {
                                let mut new_episode = series_episode.clone();
                                new_episode.file_path.clone_from(&episode.file.file_path);
                                new_episode.file_modified = episode.file.modified_timestamp;
                                new_episode.file_size = episode.file.size_bytes;
                                double_episodes.push(new_episode);
                            } else {
                                series_episode.file_path.clone_from(&episode.file.file_path);
                                series_episode.file_modified = episode.file.modified_timestamp;
                                series_episode.file_size = episode.file.size_bytes;
                            }
                        }
                    }
                }
                if !double_episodes.is_empty() {
                    series_episodes.append(&mut double_episodes);
                    series_episodes.sort_by_key(|episode| (episode.season, episode.episode));
                }
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
}
