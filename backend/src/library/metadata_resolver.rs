use log::{debug, error, warn};
use shared::utils::{clean_playlist_title, TMDB_API_KEY};
use crate::library::metadata::{MediaMetadata, MetadataSource, MovieMetadata, SeriesMetadata};
use crate::library::scanner::ScannedMediaFile;
use crate::library::tmdb_client::TmdbClient;
use crate::library::{MediaGroup, MetadataStorage};
use crate::model::LibraryConfig;
use crate::ptt::{ptt_parse_title, PttMetadata};

// Metadata resolver that tries multiple sources to get video metadata
pub struct MetadataResolver {
    tmdb_client: Option<TmdbClient>,
    fallback_to_filename: bool,
}

impl MetadataResolver {
    // Creates a new metadata resolver from configuration
    pub fn new(config: Option<&LibraryConfig>, client: reqwest::Client) -> Self {
        let storage = match config {
            None => None,
            Some(c) => {
                let storage_path = std::path::PathBuf::from(&c.metadata.path);
                Some(MetadataStorage::new(storage_path))
            }
        };
        Self::from_config(config, client, storage)
    }

    pub fn from_config(config: Option<&LibraryConfig>, client: reqwest::Client, storage: Option<MetadataStorage>) -> Self {
        let tmdb_client = config.filter(|c| c.metadata.tmdb.enabled)
            .zip(storage)
            .map(|(c, s)|{
            let api_key = c.metadata.tmdb.api_key.as_ref().map_or_else(|| TMDB_API_KEY.to_string(), ToString::to_string);
            TmdbClient::new(api_key, c.metadata.tmdb.rate_limit_ms, client, s)
        });

        Self {
            tmdb_client,
            fallback_to_filename: config.is_some_and(|c|c.metadata.fallback_to_filename),
        }
    }

    // Resolves metadata for a video file using multiple sources (Main entry point for Library Scanner)
    pub async fn resolve(&self, group: &MediaGroup) -> Option<MediaMetadata> {
        debug!("Resolving metadata for: {group}");

        // Step 1: Classify the file
        let (is_movie, Some(file), metadata) = (match group {
            MediaGroup::Movie { file, metadata } => (true, Some(file), metadata.as_ref()),
            MediaGroup::Series { show_key: _, episodes } => {
                let episode = episodes.first()?;
                (false, Some(&episode.file), episode.metadata.as_ref())
            }
        }) else { return None };

        self.resolve_internal(is_movie, metadata, Some(file), true).await
    }

    /// Public helper to resolve metadata purely from a title string (Main entry point for Xtream Processors)
    pub async fn resolve_from_title(&self, title: &str, known_tmdb_id: Option<u32>, is_movie: bool, resolve_from_tmdb: bool) -> Option<MediaMetadata> {
        // If the title is empty or just whitespace, we can't search.
        if title.trim().is_empty() {
             return None;
        }

        // Clean common IPTV prefixes before parsing
        let cleaned_title = clean_playlist_title(title);
        
        let mut metadata = ptt_parse_title(&cleaned_title);
        
        // Fallback: If PTT parser stripped everything (e.g. unusual formatting), use original title
        if metadata.title.is_empty() {
            metadata.title = if cleaned_title.trim().is_empty() { 
                title.to_string() 
            } else { 
                cleaned_title 
            };
        }

        // Inject the known ID if available to prevent unnecessary name search
        if known_tmdb_id.is_some() {
            metadata.tmdb = known_tmdb_id;
        }
        self.resolve_internal(is_movie, &metadata, None, resolve_from_tmdb).await
    }

    // Internal logic shared by both resolve methods
    async fn resolve_internal(&self, is_movie: bool, metadata: &PttMetadata, file: Option<&ScannedMediaFile>, resolve_from_tmdb: bool) -> Option<MediaMetadata> {
        if resolve_from_tmdb {
            // Try TMDB if enabled
            if let Some(ref tmdb) = self.tmdb_client {
                // Determine if we should use file-based logic or pure title/stream logic
                let result = if let Some(f) = file {
                    self.resolve_from_tmdb_file(is_movie, metadata, tmdb, f).await
                } else {
                    self.resolve_from_tmdb_title(is_movie, metadata, tmdb).await
                };

                match result {
                    Ok(Some(metadata)) => {
                        if let Some(f) = file {
                            debug!("Found TMDB metadata for: {}", f.file_path);
                        } else {
                            debug!("Found TMDB metadata for title: {}", metadata.title());
                        }
                        return Some(metadata);
                    }
                    Ok(None) => {
                        // continue to fallback
                    }
                    Err(err) => error!("Error resolving TMDB metadata: {err}"),
                }
            }
        }

        // TODO series implementation missing for NFO
        // if classification == MediaClassification::Movie {
        //     // Step 3: Try to read existing NFO file
        //     if let Some(metadata) = NfoReader::read_metadata(&file.path).await {
        //         info!("Found NFO metadata for: {}", file.file_path);
        //         return Some(metadata);
        //     }
        // }

        // Step 4: Fallback to filename parsing
        if self.fallback_to_filename {
            if let Some(f) = file {
                debug!("Using filename-based metadata for: {}", f.file_path);
            }
            Some(Self::resolve_from_filename(is_movie, metadata))
        } else {
            if let Some(f) = file {
                warn!("No metadata found for: {}", f.file_path);
            }
            None
        }
    }
    
    // Attempts to resolve metadata from TMDB using a local file context (allowing parent directory fallback)
    async fn resolve_from_tmdb_file(&self, movie: bool, metadata: &PttMetadata, tmdb: &TmdbClient, file: &ScannedMediaFile) -> Result<Option<MediaMetadata>, String> {
        if movie {
            tmdb.search_movie(metadata.tmdb, metadata.title.as_str(), metadata.year).await
        } else {
            let (series_year, tmdb_id) = if metadata.year.is_some() {
                (metadata.year, metadata.tmdb)
            } else {
                // Try to extract year from parent directory if available
                file.path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map_or((None, None), |s| {
                        let ptt = ptt_parse_title(s);
                        (ptt.year, ptt.tmdb)
                    })
            };
            tmdb.search_series(tmdb_id, metadata.title.as_str(), series_year).await
        }
    }

    // Attempts to resolve metadata from TMDB using only the parsed title/metadata (for streams)
    async fn resolve_from_tmdb_title(&self, movie: bool, metadata: &PttMetadata, tmdb: &TmdbClient) -> Result<Option<MediaMetadata>, String> {
        if movie {
            tmdb.search_movie(metadata.tmdb, metadata.title.as_str(), metadata.year).await
        } else {
            tmdb.search_series(metadata.tmdb, metadata.title.as_str(), metadata.year).await
        }
    }

    // Creates basic metadata from filename parsing
    fn resolve_from_filename(movie: bool, metadata: &PttMetadata) -> MediaMetadata {
        let timestamp = chrono::Utc::now().timestamp();

        if movie {
            MediaMetadata::Movie(MovieMetadata {
                title: metadata.title.clone(),
                year: metadata.year,
                tmdb_id: metadata.tmdb,
                tvdb_id: metadata.tvdb,
                source: MetadataSource::FilenameParsed,
                last_updated: timestamp,
                ..MovieMetadata::default()
            })
        } else {
            MediaMetadata::Series(SeriesMetadata {
                title: metadata.title.clone(),
                year: metadata.year,
                tmdb_id: metadata.tmdb,
                tvdb_id: metadata.tvdb,
                source: MetadataSource::FilenameParsed,
                last_updated: timestamp,
                ..SeriesMetadata::default()
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LibraryMetadataConfig, LibraryMetadataReadConfig, LibraryPlaylistConfig, LibraryTmdbConfig};
    use std::path::PathBuf;
    use std::time::Duration;
    use shared::utils::Internable;
    use crate::library::{MediaClassification, MediaClassifier};

    fn create_test_config(tmdb_enabled: bool, fallback_filename: bool) -> LibraryConfig {
        LibraryConfig {
            enabled: true,
            scan_directories: vec![],
            supported_extensions: vec![],
            metadata: LibraryMetadataConfig {
                path: "/tmp/vod".to_string(),
                read_existing: LibraryMetadataReadConfig {
                    kodi: true,
                    jellyfin: true,
                    plex: true,
                },
                tmdb: LibraryTmdbConfig {
                    enabled: tmdb_enabled,
                    api_key: if tmdb_enabled {
                        Some("test_key".to_string())
                    } else {
                        None
                    },
                    rate_limit_ms: 250,
                    cache_duration_days: 0,
                    language: "en-US".to_string(),
                },
                fallback_to_filename: fallback_filename,
                formats: vec![],
            },
            playlist: LibraryPlaylistConfig {
                movie_category: "Movies".intern(),
                series_category: "Series".intern(),
            },
        }
    }

    fn create_test_file(name: &str) -> ScannedMediaFile {
        ScannedMediaFile {
            path: PathBuf::from(format!("/test/{name}")),
            file_path: format!("/test/{name}"),
            file_name: name.to_string(),
            extension: "mkv".to_string(),
            size_bytes: 1024,
            modified_timestamp: 0,
        }
    }

    #[tokio::test]
    async fn test_resolve_from_filename_movie() {
        let config = create_test_config(false, true);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let resolver = MetadataResolver::from_config(Some(&config), client, Some(MetadataStorage::new(PathBuf::from("/tmp"))));
        let file = create_test_file("The.Matrix.1999.1080p.mkv");
        let metadata = match MediaClassifier::classify(&file) {
            MediaClassification::Movie { metadata, .. } | MediaClassification::Series { metadata, .. } => metadata,
        };
        let group = MediaGroup::Movie { file, metadata: Box::new(metadata) };

        let metadata = resolver.resolve(&group).await;
        assert!(metadata.is_some());

        if let Some(MediaMetadata::Movie(movie)) = metadata {
            assert_eq!(movie.title, "The Matrix");
            assert_eq!(movie.year, Some(1999));
            assert_eq!(movie.source, MetadataSource::FilenameParsed);
        } else {
            panic!("Expected movie metadata");
        }
    }

    #[tokio::test]
    async fn test_resolve_from_title_stream() {
        let config = create_test_config(false, true);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let resolver = MetadataResolver::from_config(Some(&config), client, Some(MetadataStorage::new(PathBuf::from("/tmp"))));
        
        let metadata = resolver.resolve_from_title("Inception.2010", None, true, true).await;
        assert!(metadata.is_some());

        if let Some(MediaMetadata::Movie(movie)) = metadata {
            assert_eq!(movie.title, "Inception");
            assert_eq!(movie.year, Some(2010));
            assert_eq!(movie.source, MetadataSource::FilenameParsed);
        } else {
            panic!("Expected movie metadata");
        }
    }

    #[tokio::test]
    async fn test_fallback_disabled() {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let config = create_test_config(false, false);
        let resolver = MetadataResolver::from_config(Some(&config), client, Some(MetadataStorage::new(PathBuf::from("/tmp"))));
        let file = create_test_file("343jfkjh4789dkjfh934z3.Movie.mkv");
        let metadata = match MediaClassifier::classify(&file) {
            MediaClassification::Movie { metadata, .. } | MediaClassification::Series { metadata, .. } => metadata,
        };
        let group = MediaGroup::Movie { file, metadata: Box::new(metadata) };

        let metadata = resolver.resolve(&group).await;
        assert!(metadata.is_none());
    }
}
