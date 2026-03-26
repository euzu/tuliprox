use crate::library::scanner::ScannedMediaFile;
use crate::library::SeriesKey;
use crate::ptt::{ptt_parse_title, PttMetadata};
use shared::model::LibraryContentType;
use std::collections::HashMap;

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
    /// `episode_counters` tracks the next episode number per (`series_title`, `season`)
    /// to scope auto-increment numbering per series and avoid collisions with
    /// explicitly parsed `SxxExx` values.
    pub fn classify(file: &ScannedMediaFile, episode_counters: &mut HashMap<(String, u32), u32>) -> MediaClassification {
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
                    // Record the parsed episode so later fallback files in the same
                    // series/season don't collide with it.
                    let key = (ptt_metadata.title.clone(), *season);
                    let counter = episode_counters.entry(key).or_insert(1);
                    if *episode >= *counter {
                        *counter = *episode + 1;
                    }
                    // Use normalized key (no year/tmdb) so patterned and fallback
                    // files in the same forced-Series directory group together.
                    Self::make_series_normalized(*episode, *season, ptt_metadata)
                } else {
                    // No episode/season pattern found — auto-assign sequential episode
                    // scoped to this series title + season 1.
                    let key = (ptt_metadata.title.clone(), 1);
                    let counter = episode_counters.entry(key).or_insert(1);
                    let episode = *counter;
                    *counter += 1;
                    log::debug!(
                        "No episode/season pattern in '{}', auto-assigning S01E{:02}.",
                        file.file_name, episode
                    );
                    // Normalize key: strip year/tmdb so files from the same show
                    // with different per-file years group into one series.
                    Self::make_series_normalized(episode, 1, ptt_metadata)
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

    /// Like `make_series` but strips year/tmdb from the key so that files from the
    /// same show with different per-file years (e.g. encoding year) are grouped together.
    fn make_series_normalized(episode: u32, season: u32, ptt_metadata: PttMetadata) -> MediaClassification {
        MediaClassification::Series {
            key: SeriesKey {
                title: ptt_metadata.title.clone(),
                year: None,
                tmdb_id: None,
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
        let mut counters = HashMap::new();
        let file = create_test_file("Breaking.Bad.S01E01.mkv", "/tv/Breaking.Bad", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counters);
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
        let mut counters = HashMap::new();
        let file = create_test_file("The.Matrix.1999.1080p.BluRay.mkv", "/movies", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counters);
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
        let mut counters = HashMap::new();
        let file = create_test_file("Inception.1080p.BluRay.mkv", "/movies", LibraryContentType::Auto);
        let classification = MediaClassifier::classify(&file, &mut counters);
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
        let mut counters = HashMap::new();
        let file = create_test_file("Breaking.Bad.S01E01.mkv", "/tv/Breaking.Bad", LibraryContentType::Movie);
        let classification = MediaClassifier::classify(&file, &mut counters);
        assert!(classification.is_movie());
        assert!(!classification.is_series());
    }

    #[test]
    fn test_force_series_with_pattern() {
        let mut counters = HashMap::new();
        let file = create_test_file("Breaking.Bad.S02E05.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);
        let classification = MediaClassifier::classify(&file, &mut counters);
        match classification {
            MediaClassification::Series { episode, season, .. } => {
                assert_eq!(season, 2);
                assert_eq!(episode, 5);
            }
            _ => panic!("Expected Series classification"),
        }
        // Counter should record next episode after the parsed one
        assert_eq!(counters[&("Breaking Bad".to_string(), 2)], 6);
    }

    #[test]
    fn test_force_series_without_pattern_auto_increments() {
        let mut counters = HashMap::new();
        let file1 = create_test_file("Breaking.Bad.1999.1080p.BluRay.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);
        let file2 = create_test_file("Breaking.Bad.2000.720p.mkv", "/tv/Breaking.Bad", LibraryContentType::Series);

        let c1 = MediaClassifier::classify(&file1, &mut counters);
        let c2 = MediaClassifier::classify(&file2, &mut counters);

        // Both should be series with auto-assigned episodes, scoped per title
        match (&c1, &c2) {
            (
                MediaClassification::Series { key: key1, episode: ep1, season: s1, .. },
                MediaClassification::Series { key: key2, episode: ep2, season: s2, .. },
            ) => {
                assert_eq!(*s1, 1);
                assert_eq!(*ep1, 1);
                assert_eq!(*s2, 1);
                assert_eq!(*ep2, 2);
                // Despite different per-file years, keys must be identical (normalized)
                assert_eq!(key1, key2);
                assert_eq!(key1.year, None);
                assert_eq!(key1.tmdb_id, None);
            }
            _ => panic!("Expected Series classification for both"),
        }
        assert_eq!(counters[&("Breaking Bad".to_string(), 1)], 3);
    }

    #[test]
    fn test_force_series_counter_scoped_per_title() {
        let mut counters = HashMap::new();
        let a1 = create_test_file("ShowA.1080p.mkv", "/tv/ShowA", LibraryContentType::Series);
        let b1 = create_test_file("ShowB.720p.mkv", "/tv/ShowB", LibraryContentType::Series);
        let a2 = create_test_file("ShowA.720p.mkv", "/tv/ShowA", LibraryContentType::Series);

        let ca1 = MediaClassifier::classify(&a1, &mut counters);
        let cb1 = MediaClassifier::classify(&b1, &mut counters);
        let ca2 = MediaClassifier::classify(&a2, &mut counters);

        // ShowA and ShowB each start at episode 1 independently
        match ca1 {
            MediaClassification::Series { episode, .. } => assert_eq!(episode, 1),
            _ => panic!("Expected Series"),
        }
        match cb1 {
            MediaClassification::Series { episode, .. } => assert_eq!(episode, 1),
            _ => panic!("Expected Series"),
        }
        match ca2 {
            MediaClassification::Series { episode, .. } => assert_eq!(episode, 2),
            _ => panic!("Expected Series"),
        }
    }

    #[test]
    fn test_force_series_mixed_pattern_and_fallback_same_key() {
        let mut counters = HashMap::new();
        // Patterned file with year in filename
        let f1 = create_test_file("MyShow.2020.S01E03.mkv", "/tv/MyShow", LibraryContentType::Series);
        // Fallback file: same title parsed by PTT but no SxxExx pattern
        let f2 = create_test_file("MyShow.2020.1080p.mkv", "/tv/MyShow", LibraryContentType::Series);

        let c1 = MediaClassifier::classify(&f1, &mut counters);
        let c2 = MediaClassifier::classify(&f2, &mut counters);

        match (&c1, &c2) {
            (
                MediaClassification::Series { key: k1, episode: ep1, season: s1, .. },
                MediaClassification::Series { key: k2, episode: ep2, season: s2, .. },
            ) => {
                // Both must share the same normalized key (no year/tmdb)
                assert_eq!(k1, k2);
                assert_eq!(k1.year, None);
                assert_eq!(k1.tmdb_id, None);
                // Patterned file keeps its parsed episode
                assert_eq!(*s1, 1);
                assert_eq!(*ep1, 3);
                // Fallback gets auto-assigned episode 4 (after recorded episode 3)
                assert_eq!(*s2, 1);
                assert_eq!(*ep2, 4);
            }
            _ => panic!("Expected Series for both"),
        }
    }
}
