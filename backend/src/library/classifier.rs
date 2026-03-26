use crate::library::scanner::ScannedMediaFile;
use crate::library::{SeriesKey};
use crate::ptt::{ptt_parse_title, PttMetadata};
use shared::model::LibraryContentType;

// Classification result for a video file
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaClassification {
    Movie {
        metadata: PttMetadata
    },
    Series {
        key: SeriesKey,
        episode: u32,
        season: u32,
        metadata: PttMetadata,
    },
}

impl MediaClassification {
    pub fn is_movie(&self) -> bool {
        matches!(self, MediaClassification::Movie { .. })
    }

    pub fn is_series(&self) -> bool {
        matches!(self, MediaClassification::Series { .. })
    }
}

/// Classifier for determining if a video file is a movie or series
pub struct MediaClassifier {
}

impl MediaClassifier {
    /// Classifies a video file as either Movie or Series.
    /// `episode_counter` is used when `content_type` is `Series` but no episode/season
    /// pattern is detected in the filename. In that case the file is classified as
    /// Series with season 1 and the current counter value as episode, then the counter
    /// is incremented.
    pub fn classify(file: &ScannedMediaFile, episode_counter: &mut u32) -> MediaClassification {
        let file_name = &file.file_name;
        let ptt_metadata = ptt_parse_title(file_name);

        match file.content_type {
            LibraryContentType::Auto => {
                // Auto-detection based on filename patterns
                Self::classify_as_series_or_movie(ptt_metadata)
            }
            LibraryContentType::Movie => {
                // Force movie classification regardless of filename
                MediaClassification::Movie { metadata: ptt_metadata }
            }
            LibraryContentType::Series => {
                if let (Some(episode), Some(season)) = (ptt_metadata.episodes.first(), ptt_metadata.seasons.first()) {
                    Self::make_series(*episode, *season, ptt_metadata)
                } else {
                    // No episode/season pattern found — auto-assign sequential episode
                    let episode = *episode_counter;
                    *episode_counter += 1;
                    log::debug!(
                        "No episode/season pattern in '{}', auto-assigning S01E{:02}.",
                        file.file_name, episode
                    );
                    Self::make_series(episode, 1, ptt_metadata)
                }
            }
        }
    }

    fn classify_as_series_or_movie(ptt_metadata: PttMetadata) -> MediaClassification {
        if let (Some(episode), Some(season)) = (ptt_metadata.episodes.first(), ptt_metadata.seasons.first()) {
            Self::make_series(*episode, *season, ptt_metadata)
        } else {
            MediaClassification::Movie { metadata: ptt_metadata }
        }
    }

    fn make_series(episode: u32, season: u32, ptt_metadata: PttMetadata) -> MediaClassification {
        MediaClassification::Series {
            key: SeriesKey {
                title: ptt_metadata.title.clone(),
                year: ptt_metadata.year,
                tmdb_id: ptt_metadata.tmdb,
            },
            episode,
            season,
            metadata: ptt_metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_file(file_name: &str, parent_path: &str, content_type: LibraryContentType) -> ScannedMediaFile {
        ScannedMediaFile {
            path: PathBuf::from(parent_path).join(file_name),
            file_path: parent_path.to_string(),
            file_name: file_name.to_string(),
            extension: "mkv".to_string(),
            size_bytes: 1024,
            modified_timestamp: 0,
            content_type,
        }
    }

    #[test]
    fn test_extract_show_name() {
        let mut counter = 1;
        let file = create_test_file("Breaking.Bad.S01E01.mkv", "/tv/Breaking.Bad", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counter);
        match classification {
            MediaClassification::Movie { .. } => {
                panic!("Expected Series classification");
            }
            MediaClassification::Series { key, episode, season, metadata, .. } => {
                assert_eq!(key.title, "Breaking Bad");
                assert_eq!(episode, 1);
                assert_eq!(season, 1);
                assert_eq!(metadata.extension, Some("mkv".to_string()));
            }
        }
    }

    #[test]
    fn test_extract_movie_title() {
        let mut counter = 1;
        let file = create_test_file("The.Matrix.1999.1080p.BluRay.mkv", "/movies", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counter);
        match classification {
            MediaClassification::Movie { metadata } => {
                assert_eq!(metadata.title, "The Matrix");
                assert_eq!(metadata.year, Some(1999));
                assert_eq!(metadata.extension, Some("mkv".to_string()));
            }
            MediaClassification::Series { .. } => {
                panic!("Expected Movie classification");
            }
        }
    }

    #[test]
    fn test_extract_movie_title_without_year() {
        let mut counter = 1;
        let file = create_test_file("Inception.1080p.BluRay.mkv", "/movies", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counter);
        match classification {
            MediaClassification::Movie { metadata } => {
                assert_eq!(metadata.title, "Inception");
                assert_eq!(metadata.year, None);
                assert_eq!(metadata.extension, Some("mkv".to_string()));
            }
            MediaClassification::Series { .. } => {
                panic!("Expected Movie classification");
            }
        }
    }

    #[test]
    fn test_force_movie_classification() {
        let mut counter = 1;
        let file = create_test_file("Breaking.Bad.S01E01.mkv", "/tv/Breaking.Bad", LibraryContentType::Movie);
        let classification = MediaClassifier::classify(&file, &mut counter);
        assert!(classification.is_movie());
        assert!(!classification.is_series());
    }

    #[test]
    fn test_force_series_with_pattern() {
        let mut counter = 1;
        let file = create_test_file("Breaking.Bad.S02E05.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);
        let classification = MediaClassifier::classify(&file, &mut counter);
        match classification {
            MediaClassification::Series { episode, season, .. } => {
                assert_eq!(season, 2);
                assert_eq!(episode, 5);
            }
            _ => panic!("Expected Series classification"),
        }
        // Counter should not have been incremented
        assert_eq!(counter, 1);
    }

    #[test]
    fn test_force_series_without_pattern_auto_increments() {
        let mut counter = 1;
        let file1 = create_test_file("Breaking.Bad.1999.1080p.BluRay.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);
        let file2 = create_test_file("Breaking.Bad.2000.720p.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);

        let c1 = MediaClassifier::classify(&file1, &mut counter);
        let c2 = MediaClassifier::classify(&file2, &mut counter);

        // Both should be series with auto-assigned episodes
        match c1 {
            MediaClassification::Series { episode, season, .. } => {
                assert_eq!(season, 1);
                assert_eq!(episode, 1);
            }
            _ => panic!("Expected Series classification"),
        }
        match c2 {
            MediaClassification::Series { episode, season, .. } => {
                assert_eq!(season, 1);
                assert_eq!(episode, 2);
            }
            _ => panic!("Expected Series classification"),
        }
        assert_eq!(counter, 3);
    }
}
