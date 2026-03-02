use crate::library::metadata::{MediaMetadata, MetadataCacheEntry};
use log::{debug, error, info};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use path_clean::PathClean;

// Metadata storage for local VOD files
// Stores metadata as JSON files with UUID-based filenames
#[derive(Clone)]
pub struct MetadataStorage {
    storage_dir: PathBuf,
}

impl MetadataStorage {
    // Creates a new metadata storage instance
    pub fn new(storage_dir: PathBuf) -> Self {
        Self { storage_dir }
    }

    // Initializes the storage directory
    pub async fn initialize(&self) -> std::io::Result<()> {
        if !fs::try_exists(&self.storage_dir).await.unwrap_or(false) {
            info!("Creating metadata storage directory: {}", self.storage_dir.display());
            fs::create_dir_all(&self.storage_dir).await?;
        }
        Ok(())
    }

    // Stores metadata for a video file
    pub async fn store(&self, entry: &MetadataCacheEntry) -> std::io::Result<()> {
        let file_path = self.get_metadata_file_path(&entry.uuid);

        debug!("Storing metadata for {}: {}", entry.file_path, file_path.clean().display());

        let json = serde_json::to_string_pretty(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Ensure parent directory exists - unconditionally to avoid race conditions or false negatives from try_exists
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = fs::File::create(&file_path).await?;
        file.write_all(json.as_bytes()).await?;
        file.flush().await?;

        Ok(())
    }

    // Loads metadata for a specific UUID
    pub async fn load_by_uuid(&self, uuid: &str) -> Option<MetadataCacheEntry> {
        let file_path = self.get_metadata_file_path(uuid);

        if !fs::try_exists(&file_path).await.unwrap_or(false) {
            return None;
        }

        match fs::read_to_string(&file_path).await {
            Ok(content) => match serde_json::from_str::<MetadataCacheEntry>(&content) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    error!("Failed to parse metadata file {}: {}", file_path.display(), e);
                    None
                }
            },
            Err(e) => {
                error!("Failed to read metadata file {}: {}", file_path.display(), e);
                None
            }
        }
    }

    // Loads all metadata entries from storage
    pub async fn load_all(&self) -> Vec<MetadataCacheEntry> {
        let mut entries = Vec::new();

        let mut read_dir = match fs::read_dir(&self.storage_dir).await {
            Ok(dir) => dir,
            Err(e) => {
                error!("Failed to read metadata directory: {e}");
                return entries;
            }
        };

        while let Ok(Some(dir_entry)) = read_dir.next_entry().await {
            let path = dir_entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path).await {
                    match serde_json::from_str::<MetadataCacheEntry>(&content) {
                        Ok(entry) => entries.push(entry),
                        Err(e) => {
                            error!("Failed to parse metadata file {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }

        debug!("Loaded {} metadata entries from storage", entries.len());
        entries
    }

    // Deletes metadata for a specific UUID
    pub async fn delete_by_uuid(&self, uuid: &str) -> std::io::Result<()> {
        let file_path = self.get_metadata_file_path(uuid);

        if fs::try_exists(&file_path).await.unwrap_or(false) {
            debug!("Deleting metadata file: {}", file_path.display());
            fs::remove_file(&file_path).await?;
        }

        Ok(())
    }

    // Cleans up metadata for files that no longer exist
    pub async fn cleanup_orphaned(&self) -> std::io::Result<usize> {
        let entries = self.load_all().await;
        let mut deleted_count = 0;

        for entry in entries {
            if !fs::try_exists(&entry.file_path).await.unwrap_or(false) {
                info!("Removing orphaned metadata for missing file: {}", entry.file_path);
                if let Err(e) = self.delete_by_uuid(&entry.uuid).await {
                    error!("Failed to delete orphaned metadata: {e}");
                } else {
                    deleted_count += 1;
                }
            }
        }

        if deleted_count > 0 {
            info!("Cleaned up {deleted_count} orphaned metadata entries");
        }

        Ok(deleted_count)
    }

    // Builds a map of file paths to UUIDs for quick lookups
    pub async fn build_path_index(&self) -> HashMap<String, String> {
        let entries = self.load_all().await;
        entries
            .into_iter()
            .map(|entry| (entry.file_path.clone(), entry.uuid.clone()))
            .collect()
    }

    // Gets the metadata file path for a UUID
    fn get_metadata_file_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join("library").join(format!("{uuid}.json"))
    }

    fn get_tmdb_movie_data_file_path(&self, tmdb_id: u32) -> PathBuf {
        self.storage_dir.join("movie").join(format!("movie_{tmdb_id}.tmdb"))
    }

    fn get_tmdb_series_data_file_path(&self, tmdb_id: u32) -> PathBuf {
        self.storage_dir.join("series").join(format!("series_{tmdb_id}.tmdb"))
    }

    // write raw tmdb movie info
    pub async fn store_tmdb_movie_info(&self, movie_id: u32, content: &[u8]) -> std::io::Result<PathBuf> {
        let file_path = self.get_tmdb_movie_data_file_path(movie_id);
        debug!("Storing raw tmdb movie metadata for {}", file_path.clean().display());
        self.store_file(content, file_path).await
    }

    pub async fn read_tmdb_movie_info(&self, movie_id: u32) -> std::io::Result<Vec<u8>> {
        fs::read(self.get_tmdb_movie_data_file_path(movie_id)).await
    }

    // write raw tmdb series info
    pub async fn store_tmdb_series_info(&self, series_id: u32, content: &[u8]) -> std::io::Result<PathBuf> {
        let file_path = self.get_tmdb_series_data_file_path(series_id);
        debug!("Storing raw tmdb series metadata for {}", file_path.clean().display());
        self.store_file(content, file_path).await
    }

    pub async fn read_tmdb_series_info(&self, series_id: u32) -> std::io::Result<Vec<u8>> {
        fs::read(self.get_tmdb_series_data_file_path(series_id)).await
    }

    async fn store_file(&self, content: &[u8], file_path: PathBuf) -> std::io::Result<PathBuf> {
        // Ensure parent directory exists - unconditionally
        if let Some(parent) = file_path.parent() {
             fs::create_dir_all(parent).await?;
        }

        let mut file = fs::File::create(&file_path).await?;
        file.write_all(content).await?;
        file.flush().await?;
        Ok(file_path)
    }

    // Writes an NFO file for the given metadata
    pub async fn write_nfo(&self, entry: &MetadataCacheEntry) -> std::io::Result<()> {
        let nfo_content = Self::generate_nfo_content(&entry.metadata);
        let nfo_path = PathBuf::from(entry.file_path.clone()).with_extension("nfo");

        if !fs::try_exists(&nfo_path).await.unwrap_or(false) {
            debug!("Writing NFO file: {}", nfo_path.display());
            self.store_file(nfo_content.as_bytes(), nfo_path).await?;
        }

        Ok(())
    }

    // Generates NFO XML content from metadata
    fn generate_nfo_content(metadata: &MediaMetadata) -> String {
        match metadata {
            MediaMetadata::Movie(movie) => {
                let mut nfo = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<movie>\n");
                let _ = writeln!(nfo, "  <title>{}</title>", Self::xml_escape(&movie.title));

                if let Some(ref original_title) = movie.original_title {
                    let _ = writeln!(nfo, "  <originaltitle>{}</originaltitle>", Self::xml_escape(original_title));
                }

                if let Some(year) = movie.year {
                    let _ = writeln!(nfo, "  <year>{year}</year>");
                }

                if let Some(ref plot) = movie.plot {
                    let _ = writeln!(nfo, "  <plot>{}</plot>", Self::xml_escape(plot));
                }

                if let Some(ref tagline) = movie.tagline {
                    let _ = writeln!(nfo, "  <tagline>{}</tagline>", Self::xml_escape(tagline));
                }

                if let Some(runtime) = movie.runtime {
                    let _ = writeln!(nfo, "  <runtime>{runtime}</runtime>");
                }

                if let Some(ref imdb_id) = movie.imdb_id {
                    let _ = writeln!(nfo, "  <imdbid>{}</imdbid>", Self::xml_escape(imdb_id));
                }

                if let Some(tmdb_id) = movie.tmdb_id {
                    let _ = writeln!(nfo, "  <tmdbid>{tmdb_id}</tmdbid>");
                }

                if let Some(rating) = movie.rating {
                    let _ = writeln!(nfo, "  <rating>{rating}</rating>");
                }

                if let Some(genres) = movie.genres.as_ref() {
                    for genre in genres {
                        let _ = writeln!(nfo, "  <genre>{}</genre>", Self::xml_escape(genre));
                    }
                }

                if let Some(directors) = movie.directors.as_ref() {
                    for director in directors {
                        let _ = writeln!(nfo, "  <director>{}</director>", Self::xml_escape(director));
                    }
                }

                if let Some(ref poster) = movie.poster {
                    let _ = writeln!(nfo, "  <thumb>{}</thumb>", Self::xml_escape(poster));
                }

                nfo.push_str("</movie>\n");
                nfo
            }
            MediaMetadata::Series(series) => {
                let mut nfo = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<tvshow>\n");
                let _ = writeln!(nfo, "  <title>{}</title>", Self::xml_escape(&series.title));

                if let Some(year) = series.year {
                    let _ = writeln!(nfo, "  <year>{year}</year>");
                }

                if let Some(ref plot) = series.plot {
                    let _ = writeln!(nfo, "  <plot>{}</plot>", Self::xml_escape(plot));
                }

                if let Some(ref imdb_id) = series.imdb_id {
                    let _ = writeln!(nfo, "  <imdbid>{}</imdbid>", Self::xml_escape(imdb_id));
                }

                if let Some(tmdb_id) = series.tmdb_id {
                    let _ = writeln!(nfo, "  <tmdbid>{tmdb_id}</tmdbid>");
                }

                if let Some(tvdb_id) = series.tvdb_id {
                    let _ = writeln!(nfo, "  <tvdbid>{tvdb_id}</tvdbid>");
                }

                if let Some(genres) = series.genres.as_ref() {
                    for genre in genres {
                        let _ = writeln!(nfo, "  <genre>{}</genre>", Self::xml_escape(genre));
                    }
                }

                if let Some(ref status) = series.status {
                    let _ = writeln!(nfo, "  <status>{}</status>", Self::xml_escape(status));
                }

                nfo.push_str("</tvshow>\n");
                nfo
            }
        }
    }

    // Escapes XML special characters
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::metadata::{MetadataSource, MovieMetadata};

    #[tokio::test]
    async fn test_store_and_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = MetadataStorage::new(temp_dir.path().to_path_buf());
        storage.initialize().await.unwrap();

        let entry = MetadataCacheEntry::new(
            "/test/movie.mp4".to_string(),
            1024,
            1_234_567_890,
            MediaMetadata::Movie(MovieMetadata {
                title: "Test Movie".to_string(),
                year: Some(2020),
                plot: Some("Test Movie plot".to_string()),
                rating: Some(7.23f64),
                source: MetadataSource::FilenameParsed,
                ..MovieMetadata::default()
            }),
        );

        // Store
        storage.store(&entry).await.unwrap();

        // Load by UUID
        let loaded = storage.load_by_uuid(&entry.uuid).await;
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().file_path, entry.file_path);

        // Delete
        storage.delete_by_uuid(&entry.uuid).await.unwrap();
        let deleted = storage.load_by_uuid(&entry.uuid).await;
        assert!(deleted.is_none());
    }
}