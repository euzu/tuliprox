use log::{debug, error, info, warn};
use shared::utils::TMDB_API_KEY;
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
    pub fn from_config(config: &LibraryConfig, client: reqwest::Client, storage: MetadataStorage) -> Self {
        let tmdb_client = if config.metadata.tmdb.enabled {
            let api_key = config.metadata.tmdb.api_key.as_ref().map_or_else(|| TMDB_API_KEY.to_string(), ToString::to_string);
            Some(TmdbClient::new(api_key, config.metadata.tmdb.rate_limit_ms, client, storage))
        } else {
            None
        };

        Self {
            tmdb_client,
            fallback_to_filename: config.metadata.fallback_to_filename,
        }
    }

    // Resolves metadata for a video file using multiple sources
    pub async fn resolve(&self, group: &MediaGroup) -> Option<MediaMetadata> {
        debug!("Resolving metadata for: {group}");

        // Step 1: Classify the file
        let (movie, Some(file), metadata) = (match group {
            MediaGroup::Movie { file, metadata } => (true, Some(file), metadata),
            MediaGroup::Series { show_key: _, episodes } => {
                let episode = episodes.first()?;
                (false, Some(&episode.file), &episode.metadata)
            }
        }) else { return None };

        // Step 2: Try TMDB if enabled
        if let Some(ref tmdb) = self.tmdb_client {
            match self.resolve_from_tmdb(movie, file, metadata, tmdb).await {
                Ok(Some(metadata)) => {
                    info!("Found TMDB metadata for: {}", file.file_path);
                    return Some(metadata);
                }
                Ok(None) => {
                    return None;
                }
                Err(err) => error!("Error resolving TMDB metadata: {err}"),
            }
        }

        // TODO series implementation missing
        // if classification == MediaClassification::Movie {
        //     // Step 3: Try to read existing NFO file
        //     if let Some(metadata) = NfoReader::read_metadata(&file.path).await {
        //         info!("Found NFO metadata for: {}", file.file_path);
        //         return Some(metadata);
        //     }
        // }

        // Step 4: Fallback to filename parsing
        if self.fallback_to_filename {
            info!("Using filename-based metadata for: {}", file.file_path);
            Some(Self::resolve_from_filename(movie, metadata))
        } else {
            warn!("No metadata found for: {}", file.file_path);
            None
        }
    }

    // Attempts to resolve metadata from TMDB
    async fn resolve_from_tmdb(&self, movie: bool, file: &ScannedMediaFile, metadata: &PttMetadata, tmdb: &TmdbClient) -> Result<Option<MediaMetadata>, String> {
        if movie {
            tmdb.search_movie(metadata.tmdb, metadata.title.as_str(), metadata.year).await
        } else {
            let (series_year, tmdb_id) = if metadata.year.is_some() { (metadata.year, metadata.tmdb) } else {
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
    use shared::utils::intern;
    use crate::library::{MediaClassification, MediaClassifier};

    fn create_test_config(tmdb_enabled: bool) -> LibraryConfig {
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
                    enabled: true,
                    api_key: if tmdb_enabled {
                        Some("test_key".to_string())
                    } else {
                        None
                    },
                    rate_limit_ms: 250,
                    cache_duration_days: 0,
                    language: "en-US".to_string(),
                },
                fallback_to_filename: true,
                formats: vec![],
            },
            playlist: LibraryPlaylistConfig {
                movie_category: intern("Movies"),
                series_category: intern("Series"),
            },
        }
    }

    fn create_test_file(name: &str) -> ScannedMediaFile {
        ScannedMediaFile {
            path: PathBuf::from(format!("/test/{}", name)),
            file_path: format!("/test/{}", name),
            file_name: name.to_string(),
            extension: "mkv".to_string(),
            size_bytes: 1024,
            modified_timestamp: 0,
        }
    }

    #[tokio::test]
    async fn test_resolve_from_filename_movie() {
        let config = create_test_config(false);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let resolver = MetadataResolver::from_config(&config, client, MetadataStorage::new(PathBuf::from("/tmp")));
        let file = create_test_file("The.Matrix.1999.1080p.mkv");
        let metadata = match MediaClassifier::classify(&file) {
            MediaClassification::Movie { metadata, .. } => metadata,
            MediaClassification::Series { metadata, .. } => metadata,
        };
        let group = MediaGroup::Movie { file, metadata: Box::new(metadata) };

        let metadata = resolver.resolve(&group).await;
        assert!(metadata.is_some());

        if let Some(MediaMetadata::Movie(movie)) = metadata {
            assert_eq!(movie.title, "The Matrix");
            assert_eq!(movie.year, Some(1999));
            assert_eq!(movie.source, MetadataSource::Tmdb);
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
        let mut config = create_test_config(false);
        config.metadata.fallback_to_filename = false;
        let resolver = MetadataResolver::from_config(&config, client, MetadataStorage::new(PathBuf::from("/tmp")));
        let file = create_test_file("343jfkjh4789dkjfh934z3.Movie.mkv");
        let metadata = match MediaClassifier::classify(&file) {
            MediaClassification::Movie { metadata, .. } => metadata,
            MediaClassification::Series { metadata, .. } => metadata,
        };
        let group = MediaGroup::Movie { file, metadata: Box::new(metadata) };

        let metadata = resolver.resolve(&group).await;
        assert!(metadata.is_none());
    }
}
