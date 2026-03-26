use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::library::ScannedMediaFile;
use crate::library::tmdb::{TmdbCredits, TmdbNetwork, TmdbSeriesInfoEpisodeDetails};

// Source of metadata information
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataSource {
    #[default]
    // Metadata from TMDB API
    Tmdb,
    // Metadata from Kodi NFO file
    KodiNfo,
    // Metadata from Jellyfin/Emby metadata files
    JellyfinEmby,
    // Metadata from Plex metadata files
    Plex,
    // Metadata parsed from filename
    FilenameParsed,
    // Manually entered metadata
    Manual,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct VideoClipMetadata {
    pub name: String, //"Official Trailer",
    pub key: String,
    pub site: String, // "YouTube",
    pub video_type: String, // "Trailer", "Teaser"
}

// Movie metadata
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct MovieMetadata {
    pub title: String,
    // Original title (if different from title)
    #[serde(default)]
    pub original_title: Option<String>,
    // Release year
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub tagline: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    // MPAA rating (e.g., "PG-13", "R")
    #[serde(default)]
    pub mpaa: Option<String>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tmdb_id: Option<u32>,
    #[serde(default)]
    pub tvdb_id: Option<u32>,
    // Rating (0.0 - 10.0)
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub genres: Option<Vec<String>>,
    #[serde(default)]
    pub directors: Option<Vec<String>>,
    #[serde(default)]
    pub writers: Option<Vec<String>>,
    #[serde(default)]
    pub actors: Option<Vec<Actor>>,
    #[serde(default)]
    pub studios: Option<Vec<String>>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub fanart: Option<String>,
    pub source: MetadataSource,
    pub last_updated: i64,
    pub videos: Option<Vec<VideoClipMetadata>>,
}

// Series/TV show metadata
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct SeriesMetadata {
    pub title: String,
    // Original title (if different)
    #[serde(default)]
    pub original_title: Option<String>,
    // First aired year
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub mpaa: Option<String>,
    #[serde(default)]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub tmdb_id: Option<u32>,
    #[serde(default)]
    pub tvdb_id: Option<u32>,
    // Rating (0.0 - 10.0)
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub genres: Option<Vec<String>>,
    #[serde(default)]
    pub directors: Option<Vec<String>>,
    #[serde(default)]
    pub writers: Option<Vec<String>>,
    #[serde(default)]
    pub actors: Option<Vec<Actor>>,
    #[serde(default)]
    pub studios: Option<Vec<String>>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub fanart: Option<String>,
    // Status (e.g., "Continuing", "Ended")
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub seasons: Option<Vec<SeasonMetadata>>,
    #[serde(default)]
    pub episodes: Option<Vec<EpisodeMetadata>>,
    #[serde(default)]
    pub source: MetadataSource,
    #[serde(default)]
    pub number_of_episodes: u32,
    #[serde(default)]
    pub number_of_seasons: u32,
    // Last updated timestamp (Unix epoch)
    #[serde(default)]
    pub last_updated: i64,
    #[serde(default)]
    pub videos: Option<Vec<VideoClipMetadata>>
}

// Episode metadata for TV series
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpisodeMetadata {
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub tmdb_id: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub season: u32,
    #[serde(default)]
    pub episode: u32,
    // Aired date (ISO 8601 format)
    #[serde(default)]
    pub aired: Option<String>,
    #[serde(default)]
    pub plot: Option<String>,
    #[serde(default)]
    pub runtime: Option<u32>,
    // Rating (0.0 - 10.0)
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub thumb: Option<String>,
    pub file_path: String,
    pub file_size: u64,
    pub file_modified: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeasonMetadata {
    pub id: u32,
    pub air_date: Option<String>,
    #[serde(default)]
    pub episode_count: u32,
    #[serde(default)]
    pub name: String,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    #[serde(default)]
    pub(crate) season_number: u32,
    #[serde(default)]
    pub vote_average: f64,

    pub episodes: Option<Vec<TmdbSeriesInfoEpisodeDetails>>,
    pub networks: Option<Vec<TmdbNetwork>>,
    pub credits: Option<TmdbCredits>,
}

// Actor information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub name: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub thumb: Option<String>,
}

// Complete video metadata (either movie or series)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MediaMetadata {
    #[serde(rename = "movie")]
    Movie(MovieMetadata),
    #[serde(rename = "series")]
    Series(SeriesMetadata),
}

impl MediaMetadata {
    pub fn title(&self) -> &str {
        match self {
            MediaMetadata::Movie(m) => &m.title,
            MediaMetadata::Series(s) => &s.title,
        }
    }

    pub fn year(&self) -> Option<u32> {
        match self {
            MediaMetadata::Movie(m) => m.year,
            MediaMetadata::Series(s) => s.year,
        }
    }

    pub fn imdb_id(&self) -> Option<&str> {
        match self {
            MediaMetadata::Movie(m) => m.imdb_id.as_deref(),
            MediaMetadata::Series(s) => s.imdb_id.as_deref(),
        }
    }

    pub fn tmdb_id(&self) -> Option<u32> {
        match self {
            MediaMetadata::Movie(m) => m.tmdb_id,
            MediaMetadata::Series(s) => s.tmdb_id,
        }
    }

    pub fn poster(&self) -> Option<&str> {
        match self {
            MediaMetadata::Movie(m) => m.poster.as_deref().or(m.fanart.as_deref()),
            MediaMetadata::Series(s) => s.poster.as_deref().or(s.fanart.as_deref()),
        }
    }

    pub fn source(&self) -> &MetadataSource {
        match self {
            MediaMetadata::Movie(m) => &m.source,
            MediaMetadata::Series(s) => &s.source,
        }
    }

    pub fn last_updated(&self) -> i64 {
        match self {
            MediaMetadata::Movie(m) => m.last_updated,
            MediaMetadata::Series(s) => s.last_updated,
        }
    }

    pub fn is_movie(&self) -> bool {
        matches!(self, MediaMetadata::Movie(_))
    }

    pub fn is_series(&self) -> bool {
        matches!(self, MediaMetadata::Series(_))
    }
}

// Metadata cache entry that links a file to its metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataCacheEntry {
    pub uuid: String,
    pub file_path: String,
    pub file_size: u64,
    pub file_modified: i64,
    pub metadata: MediaMetadata,
}

impl MetadataCacheEntry {
    // Creates a new cache entry with a generated UUID
    pub fn new(
        file_path: String,
        file_size: u64,
        file_modified: i64,
        metadata: MediaMetadata,
    ) -> Self {
        Self {
            uuid: Self::generate_uuid(),
            file_path,
            file_size,
            file_modified,
            metadata,
        }
    }

    // Generates a simple UUID-like identifier
    fn generate_uuid() -> String {
        Uuid::new_v4().to_string()
    }

    // Checks if the file has been modified since this entry was created
    pub fn is_file_modified(&self, file: &ScannedMediaFile, season_num: u32, episode_num: u32) -> bool {
        match &self.metadata {
            MediaMetadata::Movie(_) => {
                self.file_size != file.size_bytes || self.file_modified != file.modified_timestamp || self.file_path != file.file_path
            }

            MediaMetadata::Series(series) => {
                let Some(episodes) = series.episodes.as_ref() else {
                    // no episodes -> update
                    return true;
                };

                for episode in episodes {
                    if episode.season == season_num && episode.episode == episode_num && episode.file_path == file.file_path {
                        return episode.file_size != file.size_bytes || episode.file_modified != file.modified_timestamp;
                    }
                }

                // episode not found -> update
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_movie_metadata_creation() {
        let movie = MovieMetadata {
            title: "The Matrix".to_string(),
            original_title: None,
            year: Some(1999),
            plot: Some("A computer hacker learns about the true nature of reality.".to_string()),
            tagline: Some("Welcome to the Real World".to_string()),
            runtime: Some(136),
            mpaa: Some("R".to_string()),
            imdb_id: Some("tt0133093".to_string()),
            tmdb_id: Some(603),
            rating: Some(8.7),
            genres: Some(vec!["Action".to_string(), "Sci-Fi".to_string()]),
            directors: Some(vec!["Lana Wachowski".to_string(), "Lilly Wachowski".to_string()]),
            studios: Some(vec!["Warner Bros.".to_string()]),
            source: MetadataSource::Tmdb,
            last_updated: 0,
            ..MovieMetadata::default()
        };

        assert_eq!(movie.title, "The Matrix");
        assert_eq!(movie.year, Some(1999));
    }

    #[test]
    fn test_video_metadata_accessors() {
        let movie_meta = MediaMetadata::Movie(MovieMetadata {
            title: "Inception".to_string(),
            year: Some(2010),
            imdb_id: Some("tt1375666".to_string()),
            tmdb_id: Some(27205),
            fanart: None,
            source: MetadataSource::Tmdb,
            last_updated: 0,
            ..MovieMetadata::default()
        });

        assert_eq!(movie_meta.title(), "Inception");
        assert_eq!(movie_meta.year(), Some(2010));
        assert_eq!(movie_meta.imdb_id(), Some("tt1375666"));
        assert_eq!(movie_meta.tmdb_id(), Some(27205));
        assert!(movie_meta.is_movie());
        assert!(!movie_meta.is_series());
    }
}
