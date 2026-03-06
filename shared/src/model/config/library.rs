use crate::{
    error::TuliproxError,
    info_err_res,
    utils::{
        default_as_true, default_movie_category, default_series_category, default_storage_formats,
        default_supported_library_extensions, is_default_supported_library_extensions, is_true,
    },
};
use serde::{Deserialize, Serialize};

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
        Self { enabled: true, path: String::new(), content_type: LibraryContentType::default(), recursive: true }
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
    #[serde(default)]
    pub read_existing: LibraryMetadataReadConfigDto,
    #[serde(default = "default_as_true")]
    pub fallback_to_filename: bool,
    #[serde(default = "default_storage_formats", skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<LibraryMetadataFormat>,
}

impl LibraryMetadataConfigDto {
    pub fn is_empty(&self) -> bool {
        self.fallback_to_filename && self.read_existing.is_empty() && self.formats.is_empty()
    }
    pub fn clean(&mut self) {}
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
    pub fn is_empty(&self) -> bool { self.kodi && self.jellyfin && self.plex }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LibraryMetadataFormat {
    Nfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LibraryPlaylistConfigDto {
    #[serde(default = "default_movie_category")]
    pub movie_category: String,
    #[serde(default = "default_series_category")]
    pub series_category: String,
}

impl LibraryPlaylistConfigDto {
    pub fn is_empty(&self) -> bool {
        self.movie_category == default_movie_category() && self.series_category == default_series_category()
    }
    pub fn prepare(&mut self) {
        if self.movie_category.trim().is_empty() {
            self.movie_category = default_movie_category();
        }
        if self.series_category.trim().is_empty() {
            self.series_category = default_series_category();
        }
    }
    pub fn clean(&mut self) { self.prepare(); }
}

impl Default for LibraryPlaylistConfigDto {
    fn default() -> Self {
        Self { movie_category: default_movie_category(), series_category: default_series_category() }
    }
}

impl LibraryConfigDto {
    pub fn prepare(&mut self) -> Result<(), TuliproxError> {
        self.playlist.prepare();

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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{default_movie_category, default_series_category, LibraryPlaylistConfigDto};

    #[test]
    fn playlist_default_uses_default_categories() {
        let playlist = LibraryPlaylistConfigDto::default();
        assert_eq!(playlist.movie_category, default_movie_category());
        assert_eq!(playlist.series_category, default_series_category());
    }

    #[test]
    fn playlist_prepare_sets_default_categories_when_empty() {
        let mut playlist =
            LibraryPlaylistConfigDto { movie_category: String::new(), series_category: "   ".to_string() };
        playlist.prepare();
        assert_eq!(playlist.movie_category, default_movie_category());
        assert_eq!(playlist.series_category, default_series_category());
    }
}
