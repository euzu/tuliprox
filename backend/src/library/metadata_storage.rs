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
        }
        fs::create_dir_all(&self.storage_dir).await?;
        fs::create_dir_all(self.storage_dir.join("library")).await?;
        fs::create_dir_all(self.storage_dir.join("thumbnails")).await?;
        Ok(())
    }

    // Stores metadata for a video file
    pub async fn store(&self, entry: &MetadataCacheEntry) -> std::io::Result<()> {
        let file_path = self.get_library_metadata_file_path(&entry.uuid);

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
        let file_path = self.get_library_metadata_file_path(uuid);

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
        let library_dir = self.storage_dir.join("library");

        let mut read_dir = match fs::read_dir(&library_dir).await {
            Ok(dir) => dir,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    error!("Failed to read metadata directory {}: {e}", library_dir.display());
                }
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
        let entry = self.load_by_uuid(uuid).await;
        let file_path = self.get_library_metadata_file_path(uuid);

        if fs::try_exists(&file_path).await.unwrap_or(false) {
            debug!("Deleting metadata file: {}", file_path.display());
            fs::remove_file(&file_path).await?;
        }

        if let Some(entry) = entry {
            for thumbnail_id in referenced_thumbnail_ids_for_entry(&entry) {
                self.delete_unreferenced_thumbnail(thumbnail_id.as_str()).await;
            }
        }

        Ok(())
    }

    async fn delete_unreferenced_thumbnail(&self, hash: &str) {
        let still_referenced = self
            .load_all()
            .await
            .into_iter()
            .any(|entry| entry_references_thumbnail(&entry, hash));
        if still_referenced {
            return;
        }

        let path = self.get_thumbnail_path(hash);
        if fs::try_exists(&path).await.unwrap_or(false) {
            debug!("Deleting thumbnail file: {}", path.display());
            let _ = fs::remove_file(path).await;
        }
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

    // Gets the metadata file path for a Local library
    fn get_library_metadata_file_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join("library").join(format!("{uuid}.json"))
    }

    pub fn get_thumbnail_path(&self, hash: &str) -> PathBuf {
        self.storage_dir.join("thumbnails").join(format!("{hash}.jpg"))
    }

    pub async fn store_thumbnail(&self, hash: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.get_thumbnail_path(hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::File::create(&path).await?;
        file.write_all(data).await?;
        file.flush().await?;
        debug!("Stored thumbnail: {}", path.display());
        Ok(())
    }

    pub async fn has_thumbnail(&self, hash: &str) -> bool {
        fs::try_exists(self.get_thumbnail_path(hash)).await.unwrap_or(false)
    }

    /// Removes thumbnail files that are not referenced by any metadata entry.
    pub async fn cleanup_orphaned_thumbnails(&self) {
        let thumb_dir = self.storage_dir.join("thumbnails");
        if !fs::try_exists(&thumb_dir).await.unwrap_or(false) {
            return;
        }

        // Collect all referenced hashes
        let entries = self.load_all().await;
        let referenced: std::collections::HashSet<String> = entries
            .iter()
            .flat_map(referenced_thumbnail_ids_for_entry)
            .collect();

        // Walk thumbnail dir and delete unreferenced files
        let Ok(mut dir) = fs::read_dir(&thumb_dir).await else { return };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if !referenced.contains(stem) {
                    debug!("Removing orphaned thumbnail: {}", path.display());
                    let _ = fs::remove_file(&path).await;
                }
            }
        }
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

fn referenced_thumbnail_ids_for_entry(entry: &MetadataCacheEntry) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(hash) = entry.thumbnail_hash.as_ref() {
        ids.push(hash.clone());
    }
    if let MediaMetadata::Series(series) = &entry.metadata {
        if let Some(episodes) = series.episodes.as_ref() {
            ids.extend(episodes.iter().filter_map(|episode| episode.thumbnail_id.clone()));
        }
    }
    ids
}

fn entry_references_thumbnail(entry: &MetadataCacheEntry, hash: &str) -> bool {
    entry.thumbnail_hash.as_deref() == Some(hash)
        || match &entry.metadata {
            MediaMetadata::Movie(_) => false,
            MediaMetadata::Series(series) => series
                .episodes
                .as_ref()
                .is_some_and(|episodes| episodes.iter().any(|episode| episode.thumbnail_id.as_deref() == Some(hash))),
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

    #[tokio::test]
    async fn test_thumbnail_storage_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = MetadataStorage::new(dir.path().to_path_buf());
        storage.initialize().await.unwrap();

        let hash = "abc123def456";
        let data = b"fake jpeg data";

        assert!(!storage.has_thumbnail(hash).await);
        storage.store_thumbnail(hash, data).await.unwrap();
        assert!(storage.has_thumbnail(hash).await);

        let path = storage.get_thumbnail_path(hash);
        assert!(path.exists());
        let read_back = tokio::fs::read(&path).await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_delete_by_uuid_removes_unreferenced_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let storage = MetadataStorage::new(dir.path().to_path_buf());
        storage.initialize().await.unwrap();

        let mut entry = MetadataCacheEntry::new(
            "/test/movie.mp4".to_string(),
            1024,
            1_234_567_890,
            MediaMetadata::Movie(MovieMetadata::default()),
        );
        entry.thumbnail_hash = Some("thumb-delete-me".to_string());
        storage.store(&entry).await.unwrap();
        storage.store_thumbnail("thumb-delete-me", b"jpeg").await.unwrap();

        storage.delete_by_uuid(&entry.uuid).await.unwrap();

        assert!(!storage.has_thumbnail("thumb-delete-me").await);
    }

    #[tokio::test]
    async fn test_delete_by_uuid_keeps_shared_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let storage = MetadataStorage::new(dir.path().to_path_buf());
        storage.initialize().await.unwrap();

        let mut entry_a = MetadataCacheEntry::new(
            "/test/a.mp4".to_string(),
            100,
            1,
            MediaMetadata::Movie(MovieMetadata::default()),
        );
        entry_a.thumbnail_hash = Some("shared-thumb".to_string());

        let mut entry_b = MetadataCacheEntry::new(
            "/test/b.mp4".to_string(),
            200,
            2,
            MediaMetadata::Movie(MovieMetadata::default()),
        );
        entry_b.thumbnail_hash = Some("shared-thumb".to_string());

        storage.store(&entry_a).await.unwrap();
        storage.store(&entry_b).await.unwrap();
        storage.store_thumbnail("shared-thumb", b"jpeg").await.unwrap();

        storage.delete_by_uuid(&entry_a.uuid).await.unwrap();

        assert!(storage.has_thumbnail("shared-thumb").await);
    }
}
