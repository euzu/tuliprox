use crate::model::macros;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{ConfigDto, LibraryConfigDto, LibraryContentType, LibraryMetadataFormat};
use shared::utils::{default_metadata_path, Internable};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct LibraryScanDirectory {
    pub enabled: bool,
    pub path: String,
    pub content_type: LibraryContentType,
    pub recursive: bool,
}

#[derive(Debug, Clone)]
pub struct LibraryMetadataConfig {
    pub path: String,
    pub read_existing: LibraryMetadataReadConfig,
    pub tmdb: LibraryTmdbConfig,
    pub fallback_to_filename: bool,
    pub formats: Vec<LibraryMetadataFormat>,
}

#[derive(Debug, Clone)]
pub struct LibraryMetadataReadConfig {
    pub kodi: bool,
    pub jellyfin: bool,
    pub plex: bool,
}

#[derive(Debug, Clone)]
pub struct LibraryTmdbConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
    pub rate_limit_ms: u64,
    pub cache_duration_days: u32,
    pub language: String,
}

#[derive(Debug, Clone)]
pub struct LibraryPlaylistConfig {
    pub movie_category: Arc<str>,
    pub series_category: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct LibraryConfig {
    pub enabled: bool,
    pub scan_directories: Vec<LibraryScanDirectory>,
    pub supported_extensions: Vec<String>,
    pub metadata: LibraryMetadataConfig,
    pub playlist: LibraryPlaylistConfig,
}

impl LibraryConfig {
    fn canonicalize_scan_directory_path(path: &str, working_dir: &str) -> Result<String, TuliproxError> {
        let path = path.trim();
        if path.is_empty() {
            return info_err_res!("Library scan directory path cannot be empty");
        }

        let scan_path = PathBuf::from(path);
        let scan_path = if scan_path.is_relative() { PathBuf::from(working_dir).join(scan_path) } else { scan_path };

        match scan_path.canonicalize() {
            Ok(path_buf) => Ok(path_buf.to_string_lossy().to_string()),
            Err(err) => info_err_res!("Failed to canonicalize directory path {}: {err}", path),
        }
    }

    fn canonicalize_scan_directories(&mut self, working_dir: &str) -> Result<(), TuliproxError> {
        for dir in &mut self.scan_directories {
            dir.path = Self::canonicalize_scan_directory_path(&dir.path, working_dir)?;
        }
        Ok(())
    }

    pub fn prepare(&mut self, working_dir: &str) -> Result<(), TuliproxError> {
        if self.enabled {
            if self.metadata.path.is_empty() {
                self.metadata.path = default_metadata_path();
            }

            // Resolve metadata path to absolute path based on working_dir
            let meta_path = PathBuf::from(&self.metadata.path);
            let meta_path =
                if meta_path.is_relative() { PathBuf::from(working_dir).join(&meta_path) } else { meta_path };
            self.metadata.path = meta_path.to_string_lossy().to_string();

            self.canonicalize_scan_directories(working_dir)?;
        }
        Ok(())
    }
}

// impl Default for LibraryConfig {
//     fn default() -> Self {
//         Self::from(&LibraryConfigDto::default())
//     }
// }

macros::from_impl!(LibraryConfig);

impl From<&LibraryConfigDto> for LibraryConfig {
    fn from(dto: &LibraryConfigDto) -> Self {
        Self {
            enabled: dto.enabled,
            scan_directories: dto
                .scan_directories
                .iter()
                .map(|d| LibraryScanDirectory {
                    enabled: d.enabled,
                    path: d.path.clone(),
                    content_type: d.content_type,
                    recursive: d.recursive,
                })
                .collect(),
            supported_extensions: dto.supported_extensions.iter().map(|ext| ext.to_lowercase()).collect(),
            metadata: LibraryMetadataConfig {
                path: dto.metadata.path.clone(),
                read_existing: LibraryMetadataReadConfig {
                    kodi: dto.metadata.read_existing.kodi,
                    jellyfin: dto.metadata.read_existing.jellyfin,
                    plex: dto.metadata.read_existing.plex,
                },
                tmdb: LibraryTmdbConfig {
                    enabled: dto.metadata.tmdb.enabled,
                    api_key: dto.metadata.tmdb.api_key.clone(),
                    rate_limit_ms: dto.metadata.tmdb.rate_limit_ms,
                    cache_duration_days: dto.metadata.tmdb.cache_duration_days,
                    language: dto.metadata.tmdb.language.clone(),
                },
                fallback_to_filename: dto.metadata.fallback_to_filename,
                formats: dto.metadata.formats.clone(),
            },
            playlist: LibraryPlaylistConfig {
                movie_category: dto.playlist.movie_category.as_str().intern(),
                series_category: dto.playlist.series_category.as_str().intern(),
            },
        }
    }
}

pub fn validate_library_paths_from_dto(cfg: &ConfigDto) -> Result<(), TuliproxError> {
    let Some(library_dto) = cfg.library.as_ref() else {
        return Ok(());
    };

    let mut library_cfg = LibraryConfig::from(library_dto);
    // Always validate configured scan directories, even when library is disabled.
    // This prevents persisting invalid paths that would later break startup when enabled.
    library_cfg.canonicalize_scan_directories(&cfg.working_dir)?;
    if library_cfg.enabled {
        library_cfg.prepare(&cfg.working_dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_library_paths_from_dto;
    use shared::model::{ConfigDto, LibraryConfigDto, LibraryScanDirectoryDto};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_missing_path() -> String {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("system time must be after unix epoch").as_nanos();
        std::env::temp_dir().join(format!("tuliprox-library-missing-{nonce}")).to_string_lossy().to_string()
    }

    #[test]
    fn validate_library_paths_rejects_missing_scan_directory() {
        let cfg = ConfigDto {
            working_dir: std::env::current_dir()
                .expect("current_dir should be available")
                .to_string_lossy()
                .to_string(),
            library: Some(LibraryConfigDto {
                enabled: true,
                scan_directories: vec![LibraryScanDirectoryDto { path: unique_missing_path(), ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = validate_library_paths_from_dto(&cfg).expect_err("missing scan directory must be rejected");
        assert!(err.to_string().contains("Failed to canonicalize directory path"));
    }

    #[test]
    fn validate_library_paths_rejects_missing_scan_directory_even_when_disabled() {
        let cfg = ConfigDto {
            working_dir: std::env::current_dir()
                .expect("current_dir should be available")
                .to_string_lossy()
                .to_string(),
            library: Some(LibraryConfigDto {
                enabled: false,
                scan_directories: vec![LibraryScanDirectoryDto { path: unique_missing_path(), ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = validate_library_paths_from_dto(&cfg)
            .expect_err("missing scan directory must be rejected even when disabled");
        assert!(err.to_string().contains("Failed to canonicalize directory path"));
    }

    #[test]
    fn validate_library_paths_accepts_existing_scan_directory() {
        let cfg = ConfigDto {
            working_dir: std::env::current_dir()
                .expect("current_dir should be available")
                .to_string_lossy()
                .to_string(),
            library: Some(LibraryConfigDto {
                enabled: true,
                scan_directories: vec![LibraryScanDirectoryDto {
                    path: std::env::temp_dir().to_string_lossy().to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        validate_library_paths_from_dto(&cfg).expect("existing scan directory should pass validation");
    }
}
