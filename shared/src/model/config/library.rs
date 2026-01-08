use crate::error::{TuliproxError};
use crate::utils::{default_as_true, default_metadata_path, default_movie_category, default_series_category, default_storage_formats, default_supported_library_extensions, default_tmdb_api_key, default_tmdb_cache_duration_days, default_tmdb_language, default_tmdb_rate_limit_ms, is_default_supported_library_extensions, is_default_tmdb_cache_duration_days, is_default_tmdb_language, is_default_tmdb_rate_limit_ms, is_tmdb_default_api_key, is_true, TMDB_API_KEY};
use serde::{Deserialize, Serialize};
use crate::info_err_res;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub scan_directories: Vec<LibraryScanDirectoryDto>,
    #[serde(
        default = "default_supported_library_extensions",
        skip_serializing_if = "is_default_supported_library_extensions"
    )]
    pub supported_extensions: Vec<String>,
    #[serde(default)]
    pub metadata: LibraryMetadataConfigDto,
    #[serde(default)]
    pub playlist: LibraryPlaylistConfigDto,
}

impl LibraryConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.scan_directories.is_empty()
            && is_default_supported_library_extensions(&self.supported_extensions)
            && self.metadata.is_empty()
            && self.playlist.is_empty()
    }
    pub fn clean(&mut self) {
        self.scan_directories.retain(|d| !d.path.trim().is_empty());
        self.metadata.clean();
        self.playlist.clean();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LibraryScanDirectoryDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub path: String,
    #[serde(default)]
    pub content_type: LibraryContentType,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub recursive: bool,
}
impl Default for LibraryScanDirectoryDto {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
            content_type: LibraryContentType::default(),
            recursive: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LibraryContentType {
    #[default]
    Auto,
    Movie,
    Series,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct LibraryMetadataConfigDto {
    #[serde(default = "default_metadata_path")]
    pub path: String,
    #[serde(default)]
    pub read_existing: LibraryMetadataReadConfigDto,
    #[serde(default)]
    pub tmdb: LibraryTmdbConfigDto,
    #[serde(default = "default_as_true")]
    pub fallback_to_filename: bool,
    #[serde(default = "default_storage_formats", skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<LibraryMetadataFormat>,
}

impl LibraryMetadataConfigDto {
    pub fn is_empty(&self) -> bool {
        self.fallback_to_filename
            && self.path == default_metadata_path()
            && self.read_existing.is_empty()
            && self.tmdb.is_empty()
            && self.formats.is_empty()
    }
    pub fn clean(&mut self) {
        if self.path.trim().is_empty() {
            self.path = default_metadata_path();
        }
        self.tmdb.clean();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LibraryMetadataReadConfigDto {
    #[serde(default = "default_as_true")]
    pub kodi: bool,
    #[serde(default = "default_as_true")]
    pub jellyfin: bool,
    #[serde(default = "default_as_true")]
    pub plex: bool,
}

impl LibraryMetadataReadConfigDto {
    pub fn is_empty(&self) -> bool {
        self.kodi && self.jellyfin && self.plex
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct LibraryTmdbConfigDto {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tmdb_api_key", skip_serializing_if = "is_tmdb_default_api_key")]
    pub api_key: Option<String>,
    #[serde(
        default = "default_tmdb_rate_limit_ms",
        skip_serializing_if = "is_default_tmdb_rate_limit_ms"
    )]
    pub rate_limit_ms: u64,
    #[serde(
        default = "default_tmdb_cache_duration_days",
        skip_serializing_if = "is_default_tmdb_cache_duration_days"
    )]
    pub cache_duration_days: u32,
    #[serde(default = "default_tmdb_language", skip_serializing_if = "is_default_tmdb_language")]
    pub language: String,
}

impl LibraryTmdbConfigDto {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.api_key.as_ref().is_none_or(|api_key| api_key == TMDB_API_KEY)
            && self.rate_limit_ms == default_tmdb_rate_limit_ms()
            && self.cache_duration_days == default_tmdb_cache_duration_days()
            && self.language == default_tmdb_language()
    }
    pub fn clean(&mut self) {
        if self.api_key.as_ref().is_some_and(|api_key| api_key == TMDB_API_KEY) {
            self.api_key = None;
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LibraryMetadataFormat {
    Nfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct LibraryPlaylistConfigDto {
    #[serde(default = "default_movie_category")]
    pub movie_category: String,
    #[serde(default = "default_series_category")]
    pub series_category: String,
}

impl LibraryPlaylistConfigDto {
    pub fn is_empty(&self) -> bool {
        self.movie_category == default_movie_category()
            && self.series_category == default_series_category()
    }
    pub fn clean(&mut self) {
        if self.movie_category.trim().is_empty() {
            self.movie_category = default_movie_category();
        }
        if self.series_category.trim().is_empty() {
            self.series_category = default_series_category();
        }
    }
}

impl LibraryConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        // Validate enabled state
        if self.enabled && self.scan_directories.is_empty() {
            return info_err_res!("Library enabled but no scan_directories configured");
        }

        // Validate scan directories
        for dir in &self.scan_directories {
            if dir.path.is_empty() {
                return info_err_res!("Library scan directory path cannot be empty");
            }
        }

        // Validate metadata storage location
        if self.metadata.path.is_empty() {
            return info_err_res!("Library Metadata storage location cannot be empty");
        }

        Ok(())
    }
}
