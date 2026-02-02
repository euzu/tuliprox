use crate::library::{Actor, MediaMetadata, MetadataSource, MovieMetadata, SeriesMetadata};
use log::{debug, error, warn};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;
use tokio::fs;

macro_rules! push_to_field_list {
    ($field:expr, $current:expr) => {
        if let Some(field) = $field.as_mut() {
            field.push($current.clone());
        } else {
            $field = Some(vec![$current.clone()]);
        }
    };
}

/// NFO reader for parsing Kodi/Jellyfin/Emby/Plex metadata files
pub struct NfoReader;

impl NfoReader {
    /// Attempts to find and read an NFO file for the given video file
    /// Looks for: movie.nfo, tvshow.nfo, or {filename}.nfo
    pub async fn read_metadata(video_path: &Path) -> Option<MediaMetadata> {
        let parent_dir = video_path.parent()?;
        let file_stem = video_path.file_stem()?.to_str()?;

        // Try different NFO file locations
        let nfo_candidates = vec![
            parent_dir.join(format!("{file_stem}.nfo")), // filename.nfo
            parent_dir.join("movie.nfo"),                   // movie.nfo
            parent_dir.join("tvshow.nfo"),                  // tvshow.nfo
        ];

        for nfo_path in nfo_candidates {
            if fs::try_exists(&nfo_path).await.unwrap_or(false) {
                debug!("Found NFO file: {}", nfo_path.display());
                if let Ok(content) = fs::read_to_string(&nfo_path).await {
                    if let Some(metadata) = Self::parse_nfo(&content) {
                        return Some(metadata);
                    }
                }
            }
        }

        None
    }

    /// Parses NFO XML content into `VideoMetadata`
    fn parse_nfo(content: &str) -> Option<MediaMetadata> {
        // Determine if this is a movie or TV show NFO
        let is_movie = content.contains("<movie") || (!content.contains("<tvshow") && !content.contains("<episodedetails"));
        let is_series = !is_movie && (content.contains("<tvshow") || content.contains("<episodedetails"));

        if is_movie {
            Self::parse_movie_nfo(content)
        } else if is_series {
            Self::parse_series_nfo(content)
        } else {
            warn!("Unknown NFO format");
            None
        }
    }

    /// Parses movie NFO content
    fn parse_movie_nfo(content: &str) -> Option<MediaMetadata> {
        let mut reader = Reader::from_str(content);
        reader.config_mut().trim_text(true);

        let mut movie = MovieMetadata {
            source: MetadataSource::KodiNfo,
            last_updated: chrono::Utc::now().timestamp(),
            ..MovieMetadata::default()
        };

        let mut buf = Vec::new();
        let mut current_text = String::new();
        let mut in_actor = false;
        let mut current_actor = Actor {
            name: String::new(),
            role: None,
            thumb: None,
        };

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if tag_name == "actor" {
                        in_actor = true;
                        current_actor = Actor {
                            name: String::new(),
                            role: None,
                            thumb: None,
                        };
                    }
                    current_text.clear();
                }
                Ok(Event::Text(e)) => {
                    if let Ok(decoded) = e.decode() {
                        current_text.push_str(decoded.trim());
                    } else {
                        current_text.clear();
                    }
                }
                Ok(Event::End(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match tag_name.as_str() {
                        "title" if !in_actor => movie.title.clone_from(&current_text),
                        "originaltitle" => movie.original_title = Some(current_text.clone()),
                        "year" => movie.year = current_text.parse().ok(),
                        "plot" => movie.plot = Some(current_text.clone()),
                        "tagline" => movie.tagline = Some(current_text.clone()),
                        "runtime" => {
                            // Runtime might be in format "136" or "136 min"
                            let runtime_str = current_text.split_whitespace().next().unwrap_or("");
                            movie.runtime = runtime_str.parse().ok();
                        }
                        "mpaa" => movie.mpaa = Some(current_text.clone()),
                        "id" | "imdb" | "imdbid" => movie.imdb_id = Some(current_text.clone()),
                        "tmdbid" => movie.tmdb_id = current_text.parse().ok(),
                        "rating" => movie.rating = current_text.parse().ok(),
                        "genre" => push_to_field_list!(movie.genres, current_text),
                        "director" => push_to_field_list!(movie.directors, current_text),
                        "credits" | "writer" => push_to_field_list!(movie.writers, current_text),
                        "studio" => push_to_field_list!(movie.studios, current_text),
                        "thumb" | "poster" => movie.poster = Some(current_text.clone()),
                        "fanart" => movie.fanart = Some(current_text.clone()),
                        "name" if in_actor => current_actor.name.clone_from(&current_text),
                        "role" if in_actor => current_actor.role = Some(current_text.clone()),
                        "actor" => {
                            if !current_actor.name.is_empty() {
                                push_to_field_list!(movie.actors, current_actor);
                            }
                            in_actor = false;
                        }
                        _ => {}
                    }
                    current_text.clear();
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    error!("Error parsing movie NFO: {e}");
                    return None;
                }
                _ => {}
            }
            buf.clear();
        }

        if movie.title.is_empty() {
            None
        } else {
            Some(MediaMetadata::Movie(movie))
        }
    }

    /// Parses TV series NFO content
    fn parse_series_nfo(content: &str) -> Option<MediaMetadata> {
        let mut reader = Reader::from_str(content);
        reader.config_mut().trim_text(true);

        let mut series = SeriesMetadata {
            source: MetadataSource::KodiNfo,
            last_updated: chrono::Utc::now().timestamp(),
            ..SeriesMetadata::default()
        };

        let mut buf = Vec::new();
        let mut current_text = String::new();
        let mut in_actor = false;
        let mut current_actor = Actor {
            name: String::new(),
            role: None,
            thumb: None,
        };

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if tag_name == "actor" {
                        in_actor = true;
                        current_actor = Actor {
                            name: String::new(),
                            role: None,
                            thumb: None,
                        };
                    }
                    current_text.clear();
                }
                Ok(Event::Text(e)) => {
                    if let Ok(decoded) = e.decode() {
                        current_text.push_str(decoded.trim());
                    } else {
                        current_text.clear();
                    }
                }
                Ok(Event::End(e)) => {
                    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match tag_name.as_str() {
                        "title" if !in_actor => series.title.clone_from(&current_text),
                        "originaltitle" => series.original_title = Some(current_text.clone()),
                        "year" | "premiered" => {
                            // Extract year from date like "2008-01-20"
                            if let Some(year_str) = current_text.split('-').next() {
                                series.year = year_str.parse().ok();
                            }
                        }
                        "plot" => series.plot = Some(current_text.clone()),
                        "mpaa" => series.mpaa = Some(current_text.clone()),
                        "id" | "imdb" | "imdbid" => series.imdb_id = Some(current_text.clone()),
                        "tmdbid" => series.tmdb_id = current_text.parse().ok(),
                        "tvdbid" => series.tvdb_id = current_text.parse().ok(),
                        "rating" => series.rating = current_text.parse().ok(),
                        "genre" => push_to_field_list!(series.genres, current_text),
                        "studio" => push_to_field_list!(series.studios, current_text),
                        "thumb" | "poster" => series.poster = Some(current_text.clone()),
                        "fanart" => series.fanart = Some(current_text.clone()),
                        "status" => series.status = Some(current_text.clone()),
                        "name" if in_actor => current_actor.name.clone_from(&current_text),
                        "role" if in_actor => current_actor.role = Some(current_text.clone()),
                        "actor" => {
                            if !current_actor.name.is_empty() {
                                push_to_field_list!(series.actors, current_actor);
                            }
                            in_actor = false;
                        }
                        _ => {}
                    }
                    current_text.clear();
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    error!("Error parsing series NFO: {e}");
                    return None;
                }
                _ => {}
            }
            buf.clear();
        }

        if series.title.is_empty() {
            None
        } else {
            Some(MediaMetadata::Series(series))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_movie_nfo() {
        let nfo_content = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<movie>
    <title>The Matrix</title>
    <originaltitle>The Matrix</originaltitle>
    <year>1999</year>
    <plot>A computer hacker learns about the true nature of reality.</plot>
    <tagline>Welcome to the Real World</tagline>
    <runtime>136</runtime>
    <mpaa>R</mpaa>
    <imdbid>tt0133093</imdbid>
    <tmdbid>603</tmdbid>
    <rating>8.7</rating>
    <genre>Action</genre>
    <genre>Sci-Fi</genre>
    <director>Lana Wachowski</director>
    <director>Lilly Wachowski</director>
    <studio>Warner Bros.</studio>
</movie>"#;

        let metadata = NfoReader::parse_movie_nfo(nfo_content);
        assert!(metadata.is_some());

        if let Some(MediaMetadata::Movie(movie)) = metadata {
            assert_eq!(movie.title, "The Matrix");
            assert_eq!(movie.year, Some(1999));
            assert_eq!(movie.imdb_id, Some("tt0133093".to_string()));
            assert_eq!(movie.tmdb_id, Some(603));
            assert_eq!(movie.genres.as_ref().map(Vec::len).unwrap_or_default(), 2);
            assert_eq!(movie.directors.as_ref().map(Vec::len).unwrap_or_default(), 2);
        } else {
            panic!("Expected movie metadata");
        }
    }

    #[tokio::test]
    async fn test_parse_series_nfo() {
        let nfo_content = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<tvshow>
    <title>Breaking Bad</title>
    <year>2008</year>
    <plot>A high school chemistry teacher turned methamphetamine producer.</plot>
    <mpaa>TV-MA</mpaa>
    <imdbid>tt0903747</imdbid>
    <tmdbid>1396</tmdbid>
    <tvdbid>81189</tvdbid>
    <rating>9.5</rating>
    <genre>Crime</genre>
    <genre>Drama</genre>
    <genre>Thriller</genre>
    <studio>AMC</studio>
    <status>Ended</status>
</tvshow>"#;

        let metadata = NfoReader::parse_series_nfo(nfo_content);
        assert!(metadata.is_some());

        if let Some(MediaMetadata::Series(series)) = metadata {
            assert_eq!(series.title, "Breaking Bad");
            assert_eq!(series.year, Some(2008));
            assert_eq!(series.imdb_id, Some("tt0903747".to_string()));
            assert_eq!(series.tmdb_id, Some(1396));
            assert_eq!(series.tvdb_id, Some(81189));
            assert_eq!(series.genres.as_ref().map(Vec::len).unwrap_or_default(), 3);
            assert_eq!(series.status, Some("Ended".to_string()));
        } else {
            panic!("Expected series metadata");
        }
    }
}
