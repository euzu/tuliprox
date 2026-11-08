use crate::{
    library::{MediaClassification, MediaClassifier},
    ptt::PttMetadata,
};
use log::{debug, error, info, trace, warn};
use shared::model::LibraryContentType;
use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, Formatter},
    path::{Path, PathBuf},
};
use tokio::{fs, io};
use tuliprox_core::model::{LibraryConfig, LibraryScanDirectory};

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct SeriesKey {
    pub title: String,
    pub year: Option<u32>,
    pub tmdb_id: Option<u32>,
}

impl Display for SeriesKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title)?;
        if let Some(year) = self.year {
            write!(f, "-{year}")?;
        }
        if let Some(tmdb_id) = self.tmdb_id {
            write!(f, "-{tmdb_id}")?;
        }
        Ok(())
    }
}

pub enum MediaGroup {
    Movie { file: ScannedMediaFile, metadata: Box<PttMetadata> },
    Series { show_key: SeriesKey, episodes: Vec<SeriesEpisodeFile> },
}

impl Display for MediaGroup {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaGroup::Movie { file, .. } => write!(f, "{}", file.file_path),
            MediaGroup::Series { show_key, .. } => write!(f, "{}", show_key.title),
        }
    }
}

#[derive(Clone)]
pub struct SeriesEpisodeFile {
    pub file: ScannedMediaFile,
    pub season: u32,
    pub episode: u32,
    pub auto_assigned_episode: bool,
    pub metadata: Box<PttMetadata>,
}

pub struct MediaGrouper;

impl MediaGrouper {
    pub fn group(files: Vec<ScannedMediaFile>) -> Vec<MediaGroup> {
        let mut series_map: HashMap<SeriesKey, Vec<SeriesEpisodeFile>> = HashMap::new();
        let mut movies = Vec::new();

        let mut episode_counters = HashMap::new();
        for file in files {
            let classification = MediaClassifier::classify(&file, &mut episode_counters);
            match classification {
                MediaClassification::Movie { metadata } => {
                    movies.push(MediaGroup::Movie { file, metadata: Box::new(metadata) });
                }
                MediaClassification::Series { key, metadata, season, episode, .. } => {
                    let auto_assigned_episode = metadata.episodes.is_empty() || metadata.seasons.is_empty();
                    series_map.entry(key).or_default().push(SeriesEpisodeFile {
                        file,
                        season,
                        episode,
                        auto_assigned_episode,
                        metadata: Box::new(metadata),
                    });
                }
                // Recordings are routed to a dedicated DVR section
                // by the recording catalog projection. The scanner
                // passes them through
                // unchanged; the frontend reads the dedicated
                // catalog.
                MediaClassification::Recording { .. } => {}
            }
        }

        let mut result = movies;
        result.extend(series_map.into_iter().map(|(key, mut episodes)| {
            assign_fallback_episode_numbers(&mut episodes);
            episodes.sort_by(|a, b| {
                (a.season, a.episode, a.file.modified_timestamp, &a.file.file_path).cmp(&(
                    b.season,
                    b.episode,
                    b.file.modified_timestamp,
                    &b.file.file_path,
                ))
            });
            MediaGroup::Series { show_key: key, episodes }
        }));

        // remove empty groups
        result
            .into_iter()
            .filter(|group| match group {
                MediaGroup::Movie { .. } => true,
                MediaGroup::Series { episodes, .. } => !episodes.is_empty(),
            })
            .collect()
    }
}

fn assign_fallback_episode_numbers(episodes: &mut [SeriesEpisodeFile]) {
    let next_fallback_episode = episodes
        .iter()
        .filter(|episode| !episode.auto_assigned_episode && episode.season == 1)
        .map(|episode| episode.episode)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    let mut fallback_indices: Vec<usize> =
        episodes.iter().enumerate().filter_map(|(idx, episode)| episode.auto_assigned_episode.then_some(idx)).collect();

    fallback_indices.sort_by(|left, right| {
        let left_episode = &episodes[*left];
        let right_episode = &episodes[*right];
        (left_episode.file.modified_timestamp, &left_episode.file.file_path)
            .cmp(&(right_episode.file.modified_timestamp, &right_episode.file.file_path))
    });

    for (offset, idx) in fallback_indices.into_iter().enumerate() {
        episodes[idx].season = 1;
        episodes[idx].episode = next_fallback_episode.saturating_add(u32::try_from(offset).unwrap_or(0));
    }
}

/// Represents a discovered video file with its metadata
#[derive(Debug, Clone)]
pub struct ScannedMediaFile {
    pub path: PathBuf,
    pub file_path: String,
    pub file_name: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_timestamp: i64,
    pub content_type: LibraryContentType,
}

impl ScannedMediaFile {
    /// Creates a new `ScannedMediaFile` from a path and metadata
    pub async fn from_path(path: &Path, content_type: LibraryContentType) -> io::Result<Self> {
        let metadata = fs::metadata(path).await?;
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();

        let modified_timestamp = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0);

        Ok(Self {
            file_path: path.display().to_string(),
            path: path.to_path_buf(),
            file_name,
            extension,
            size_bytes: metadata.len(),
            modified_timestamp,
            content_type,
        })
    }
}

/// Library file scanner for local VOD directories
pub struct LibraryScanner {
    config: LibraryConfig,
}

/// Completeness required by a caller that will publish targets from this discovery.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanCompleteness {
    BestEffort,
    Complete,
}

impl LibraryScanner {
    pub fn new(config: LibraryConfig) -> Self { Self { config } }

    pub async fn scan_all(&self) -> Result<Vec<ScannedMediaFile>, io::Error> {
        self.scan_all_with_completeness(ScanCompleteness::BestEffort).await
    }

    pub(super) async fn scan_all_with_completeness(
        &self,
        completeness: ScanCompleteness,
    ) -> Result<Vec<ScannedMediaFile>, io::Error> {
        if !self.config.enabled {
            info!("Library media scanning is disabled");
            return Ok(Vec::new());
        }

        let mut all_files = Vec::new();

        if completeness == ScanCompleteness::Complete && !self.config.scan_directories.iter().any(|dir| dir.enabled) {
            return Err(io::Error::other("Library rescan requires an enabled scan directory"));
        }

        for scan_dir in &self.config.scan_directories {
            if !scan_dir.enabled {
                debug!("Skipping disabled scan directory: {}", scan_dir.path);
                continue;
            }

            info!("Scanning directory: {}", scan_dir.path);
            match self.scan_directory(scan_dir, completeness).await {
                Ok(mut files) => {
                    info!("Found {} video files in {}", files.len(), scan_dir.path);
                    all_files.append(&mut files);
                }
                Err(err) => {
                    error!("Failed to scan directory {}: {err}", scan_dir.path);
                    if completeness == ScanCompleteness::Complete {
                        return Err(err);
                    }
                }
            }
        }

        info!("Total video files found: {}", all_files.len());
        Ok(all_files)
    }

    // Recursively scans a single directory for video files
    async fn scan_directory(
        &self,
        scan_directory: &LibraryScanDirectory,
        completeness: ScanCompleteness,
    ) -> io::Result<Vec<ScannedMediaFile>> {
        let path = Path::new(&scan_directory.path);

        if completeness == ScanCompleteness::BestEffort && !fs::try_exists(path).await.unwrap_or(false) {
            warn!("Directory does not exist or is not readable: {}", scan_directory.path);
            return Ok(Vec::new());
        }

        let dir_metadata = fs::metadata(path).await?;
        if !dir_metadata.is_dir() {
            if completeness == ScanCompleteness::Complete {
                return Err(io::Error::other("Library scan root is not a directory"));
            }
            warn!("Path is not a directory: {}", scan_directory.path);
            return Ok(Vec::new());
        }

        // Directory cycles are only possible through symlinks.
        let mut visited_dirs = HashSet::new();
        if scan_directory.follow_symlinks {
            match tolerate(fs::canonicalize(path).await, "canonicalize scan root", path, completeness)? {
                Some(canonical_root) => visited_dirs.insert(canonical_root),
                None => return Ok(Vec::new()),
            };
        }

        let mut files = Vec::new();
        self.scan_directory_recursive(path, scan_directory, &mut files, &mut visited_dirs, completeness).await?;
        Ok(files)
    }

    fn scan_directory_recursive<'a>(
        &'a self,
        path: &'a Path,
        scan_directory: &'a LibraryScanDirectory,
        files: &'a mut Vec<ScannedMediaFile>,
        visited_dirs: &'a mut HashSet<PathBuf>,
        completeness: ScanCompleteness,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut entries = fs::read_dir(path).await?;

            while let Some(entry) = entries.next_entry().await? {
                let entry_path = entry.path();
                let Some(file_type) =
                    tolerate(entry.file_type().await, "read file type for", &entry_path, completeness)?
                else {
                    continue;
                };

                let metadata = if file_type.is_symlink() {
                    if !scan_directory.follow_symlinks {
                        debug!("Skipping symlink because follow_symlinks is false: {}", entry_path.display());
                        continue;
                    }
                    // Follows the link; a dangling link (e.g. offline mount) fails here.
                    fs::metadata(&entry_path).await
                } else {
                    entry.metadata().await
                };
                let Some(metadata) = tolerate(metadata, "read metadata for", &entry_path, completeness)? else {
                    continue;
                };

                if metadata.is_dir() {
                    if scan_directory.recursive {
                        if scan_directory.follow_symlinks {
                            let Some(canonical_dir) = tolerate(
                                fs::canonicalize(&entry_path).await,
                                "canonicalize directory",
                                &entry_path,
                                completeness,
                            )?
                            else {
                                continue;
                            };
                            if !visited_dirs.insert(canonical_dir) {
                                debug!("Skipping already visited directory: {}", entry_path.display());
                                continue;
                            }
                        }

                        // Recursively scan subdirectories
                        if let Err(err) = self
                            .scan_directory_recursive(&entry_path, scan_directory, files, visited_dirs, completeness)
                            .await
                        {
                            error!("Failed to scan subdirectory {}: {err}", entry_path.display());
                            if completeness == ScanCompleteness::Complete {
                                return Err(err);
                            }
                        }
                    }
                } else if metadata.is_file() {
                    // Check if file has a supported video extension
                    if let Some(ext) = entry_path.extension().and_then(|e| e.to_str()) {
                        let ext_lower = ext.to_lowercase();
                        if self.config.supported_extensions.contains(&ext_lower) {
                            if let Some(video_file) = tolerate(
                                ScannedMediaFile::from_path(&entry_path, scan_directory.content_type).await,
                                "read metadata for",
                                &entry_path,
                                completeness,
                            )? {
                                trace!("Found video file: {}", video_file.file_path);
                                files.push(video_file);
                            }
                        }
                    }
                }
            }

            Ok(())
        })
    }

    // Checks if a file has been modified since a given timestamp
    pub async fn is_file_modified_since(path: &Path, since_timestamp: i64) -> bool {
        match fs::metadata(path).await {
            Ok(metadata) => {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                        return i64::try_from(duration.as_secs()).unwrap_or(0) > since_timestamp;
                    }
                }
                false
            }
            Err(_) => false,
        }
    }
}

/// Applies the scan error policy: `Complete` propagates the error, `BestEffort` logs it and yields `None`.
fn tolerate<T>(
    result: io::Result<T>,
    action: &str,
    path: &Path,
    completeness: ScanCompleteness,
) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) => {
            error!("Failed to {action} {}: {err}", path.display());
            if completeness == ScanCompleteness::Complete {
                Err(err)
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::utils::Internable;
    use tuliprox_core::model::{LibraryMetadataConfig, LibraryMetadataReadConfig, LibraryPlaylistConfig};

    fn create_test_config() -> LibraryConfig {
        LibraryConfig {
            enabled: true,
            scan_directories: vec![],
            supported_extensions: vec!["mp4".to_string(), "mkv".to_string(), "avi".to_string()],
            metadata: LibraryMetadataConfig {
                read_existing: LibraryMetadataReadConfig { kodi: false, jellyfin: false, plex: false },
                fallback_to_filename: true,
                formats: vec![],
            },
            playlist: LibraryPlaylistConfig {
                movie_category: "Local Movies".intern(),
                series_category: "Local Series".intern(),
            },
            thumbnails: tuliprox_core::model::ThumbnailConfig { enabled: false, width: 320, height: 180 },
        }
    }

    fn create_group_test_file(
        file_name: &str,
        parent_path: &str,
        modified_timestamp: i64,
        content_type: LibraryContentType,
    ) -> ScannedMediaFile {
        let path = PathBuf::from(parent_path).join(file_name);
        ScannedMediaFile {
            file_path: path.display().to_string(),
            path,
            file_name: file_name.to_string(),
            extension: "mkv".to_string(),
            size_bytes: 1024,
            modified_timestamp,
            content_type,
        }
    }

    #[tokio::test]
    async fn test_scanner_creation() {
        let config = create_test_config();
        let scanner = LibraryScanner::new(config);
        assert!(scanner.config.enabled);
    }

    #[tokio::test]
    async fn test_disabled_scanner() {
        let mut config = create_test_config();
        config.enabled = false;
        let scanner = LibraryScanner::new(config);
        let result = scanner.scan_all().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_group_assigns_fallback_episode_numbers_by_modified_time() {
        let files = vec![
            create_group_test_file("MyShow.2020.1080p.mkv", "/tv/MyShow", 200, LibraryContentType::Series),
            create_group_test_file("MyShow.2020.S01E03.mkv", "/tv/MyShow", 300, LibraryContentType::Series),
            create_group_test_file("MyShow.2021.720p.mkv", "/tv/MyShow", 100, LibraryContentType::Series),
        ];

        let groups = MediaGrouper::group(files);
        let MediaGroup::Series { episodes, .. } = &groups[0] else {
            panic!("Expected series group");
        };

        let episode_numbers: HashMap<_, _> =
            episodes.iter().map(|episode| (episode.file.file_name.as_str(), episode.episode)).collect();

        assert_eq!(episode_numbers["MyShow.2020.S01E03.mkv"], 3);
        assert_eq!(episode_numbers["MyShow.2021.720p.mkv"], 4);
        assert_eq!(episode_numbers["MyShow.2020.1080p.mkv"], 5);
    }

    #[cfg(unix)]
    mod symlink_tests {
        use super::*;
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;

        fn scan_dir(path: &Path, recursive: bool, follow_symlinks: bool) -> LibraryScanDirectory {
            LibraryScanDirectory {
                enabled: true,
                path: path.to_string_lossy().into_owned(),
                content_type: LibraryContentType::Movie,
                recursive,
                follow_symlinks,
            }
        }

        fn test_library_config(scan_dirs: Vec<LibraryScanDirectory>) -> LibraryConfig {
            let mut config = create_test_config();
            config.scan_directories = scan_dirs;
            config
        }

        #[tokio::test]
        async fn test_symlinked_file_found_with_link_path() {
            let temp = tempdir().expect("tempdir");
            let target_file = temp.path().join("real_movie.mp4");
            tokio::fs::write(&target_file, b"content").await.expect("write target");

            let link_file = temp.path().join("link_movie.mp4");
            symlink(&target_file, &link_file).expect("symlink");

            let config = test_library_config(vec![scan_dir(temp.path(), false, true)]);

            let scanner = LibraryScanner::new(config);
            let files = scanner.scan_all().await.expect("scan_all");

            let file_paths: Vec<_> = files.iter().map(|f| f.file_path.as_str()).collect();
            assert_eq!(file_paths.len(), 2);
            assert!(file_paths.contains(&link_file.to_str().unwrap()));
            assert!(file_paths.contains(&target_file.to_str().unwrap()));
        }

        #[tokio::test]
        async fn test_symlinked_directory_outside_root() {
            let temp = tempdir().expect("tempdir");
            let root_dir = temp.path().join("root");
            let outside_dir = temp.path().join("outside");
            tokio::fs::create_dir_all(&root_dir).await.expect("create root");
            tokio::fs::create_dir_all(&outside_dir).await.expect("create outside");

            let media_file = outside_dir.join("outside_movie.mp4");
            tokio::fs::write(&media_file, b"content").await.expect("write media");

            let link_dir = root_dir.join("linked_dir");
            symlink(&outside_dir, &link_dir).expect("symlink dir");

            let config = test_library_config(vec![scan_dir(&root_dir, true, true)]);

            let scanner = LibraryScanner::new(config);
            let files = scanner.scan_all().await.expect("scan_all");

            assert_eq!(files.len(), 1);
            let expected_link_file = link_dir.join("outside_movie.mp4");
            assert_eq!(files[0].file_path, expected_link_file.to_string_lossy().to_string());
        }

        #[tokio::test]
        async fn test_follow_symlinks_false_skips_both() {
            let temp = tempdir().expect("tempdir");
            let root_dir = temp.path().join("root");
            let outside_dir = temp.path().join("outside");
            tokio::fs::create_dir_all(&root_dir).await.expect("create root");
            tokio::fs::create_dir_all(&outside_dir).await.expect("create outside");

            let outside_file = outside_dir.join("outside.mp4");
            tokio::fs::write(&outside_file, b"content").await.expect("write outside");

            let real_file = root_dir.join("real.mp4");
            tokio::fs::write(&real_file, b"content").await.expect("write real");

            let file_link = root_dir.join("link.mp4");
            symlink(&outside_file, &file_link).expect("symlink file");

            let dir_link = root_dir.join("dir_link");
            symlink(&outside_dir, &dir_link).expect("symlink dir");

            let config = test_library_config(vec![scan_dir(&root_dir, true, false)]);

            let scanner = LibraryScanner::new(config);
            let files = scanner.scan_all().await.expect("scan_all");

            assert_eq!(files.len(), 1);
            assert_eq!(files[0].file_path, real_file.to_string_lossy().to_string());
        }

        #[tokio::test]
        async fn test_symlink_cycle_terminates_and_finds_files_once() {
            let temp = tempdir().expect("tempdir");
            let root_dir = temp.path().join("root");
            let sub_dir = root_dir.join("sub");
            tokio::fs::create_dir_all(&sub_dir).await.expect("create sub");

            let movie_file = sub_dir.join("movie.mp4");
            tokio::fs::write(&movie_file, b"content").await.expect("write movie");

            // Loop: root/sub/loop -> root
            let loop_link = sub_dir.join("loop");
            symlink(&root_dir, &loop_link).expect("create loop symlink");

            let config = test_library_config(vec![scan_dir(&root_dir, true, true)]);

            let scanner = LibraryScanner::new(config);
            let files = scanner.scan_all().await.expect("scan_all");

            assert_eq!(files.len(), 1);
            assert_eq!(files[0].file_path, movie_file.to_string_lossy().to_string());
        }

        #[tokio::test]
        async fn test_dangling_symlink_handling() {
            let temp = tempdir().expect("tempdir");
            let root_dir = temp.path().join("root");
            tokio::fs::create_dir_all(&root_dir).await.expect("create root");

            let real_file = root_dir.join("real.mp4");
            tokio::fs::write(&real_file, b"content").await.expect("write real");

            let dangling = root_dir.join("dangling.mp4");
            symlink(temp.path().join("nonexistent.mp4"), &dangling).expect("create dangling symlink");

            let scan_dir = scan_dir(&root_dir, false, true);

            let config = test_library_config(vec![scan_dir.clone()]);
            let scanner = LibraryScanner::new(config);

            // BestEffort continues and finds the real file
            let best_effort_files =
                scanner.scan_all_with_completeness(ScanCompleteness::BestEffort).await.expect("best effort");
            assert_eq!(best_effort_files.len(), 1);
            assert_eq!(best_effort_files[0].file_path, real_file.to_string_lossy().to_string());

            // Complete fails on dangling symlink
            let complete_result = scanner.scan_all_with_completeness(ScanCompleteness::Complete).await;
            assert!(complete_result.is_err(), "Complete completeness must fail on dangling symlink");
        }

        #[tokio::test]
        async fn test_scan_root_is_symlink() {
            let temp = tempdir().expect("tempdir");
            let real_root = temp.path().join("real_root");
            tokio::fs::create_dir_all(&real_root).await.expect("create real root");

            let movie_file = real_root.join("movie.mp4");
            tokio::fs::write(&movie_file, b"content").await.expect("write movie");

            let symlink_root = temp.path().join("symlink_root");
            symlink(&real_root, &symlink_root).expect("create symlink root");

            // follow_symlinks: true
            let config_follow = test_library_config(vec![scan_dir(&symlink_root, false, true)]);
            let scanner_follow = LibraryScanner::new(config_follow);
            let files_follow = scanner_follow.scan_all().await.expect("scan_all follow");
            assert_eq!(files_follow.len(), 1);

            // follow_symlinks: false
            let config_no_follow = test_library_config(vec![scan_dir(&symlink_root, false, false)]);
            let scanner_no_follow = LibraryScanner::new(config_no_follow);
            let files_no_follow = scanner_no_follow.scan_all().await.expect("scan_all no follow");
            assert_eq!(files_no_follow.len(), 1);
        }
    }
}
