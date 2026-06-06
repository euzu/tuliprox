use crate::model::Config;
use crate::repository::storage_const;
use crate::utils;
use shared::error::TuliproxError;
use std::path::{Path, PathBuf};
use shared::concat_string;

pub fn get_target_id_mapping_file(target_path: &Path) -> PathBuf {
    // Join directly with &str to avoid an intermediate PathBuf allocation
    target_path.join(storage_const::FILE_ID_MAPPING)
}

pub async fn ensure_target_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    if let Some(path) = get_target_storage_path(cfg, target_name) {
            tokio::fs::create_dir_all(&path).await.map_err(|err| {
                TuliproxError::RepositoryStorage(format!(
                    "Failed to save target data, can't create directory {}: {err}",
                    path.display()
                ))
            })?;
        Ok(path)
    } else {
        let msg = format!("Failed to save target data, can't create directory for target {target_name}");
        Err(TuliproxError::RepositoryStorage(msg))
    }
}

/// Resolve a per-target storage subdirectory via `path_resolver`, create the
/// directory if missing, and map any failure into a domain-specific
/// `TuliproxError` variant via `error`.
///
/// Used by the per-backend `ensure_*_storage_path` helpers (xtream, m3u) which
/// differ only in their subdirectory resolver, error variant, and human-readable
/// label.
pub async fn ensure_target_storage_subpath<E, F>(
    cfg: &Config,
    target_name: &str,
    label: &str,
    path_resolver: F,
    error: E,
) -> Result<PathBuf, TuliproxError>
where
    F: Fn(&Config, &str) -> Option<PathBuf>,
    E: Fn(String) -> TuliproxError,
{
    if let Some(path) = path_resolver(cfg, target_name) {
        tokio::fs::create_dir_all(&path).await.map_err(|err| {
            error(format!(
                "Failed to save {label} data, can't create directory {}: {err}",
                path.display()
            ))
        })?;
        Ok(path)
    } else {
        let msg = format!("Failed to save {label} data, can't create directory for target {target_name}");
        Err(error(msg))
    }
}

pub fn get_target_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
    utils::get_file_path(&cfg.storage_dir, Some(std::path::PathBuf::from(target_name.replace(' ', "_"))))
}

pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

pub fn build_input_storage_path(input_name: &str, storage_dir: &str) -> PathBuf {
    let sanitized_name: String = sanitize_name(input_name);
    let name = concat_string!(cap = 6 + sanitized_name.len(); "input_", &sanitized_name);
    Path::new(storage_dir).join(name)
}

pub async fn get_input_storage_path(input_name: &str, storage_dir: &str) -> std::io::Result<PathBuf> {
    let path = build_input_storage_path(input_name, storage_dir);
    // Create the directory and return the path or propagate the error
    tokio::fs::create_dir_all(&path).await.map(|()| path)
}

pub async fn ensure_input_storage_path(cfg: &Config, input_name: &str) -> Result<PathBuf, TuliproxError> {
    get_input_storage_path(input_name, &cfg.storage_dir).await
        .map_err(|err| {
            TuliproxError::RepositoryStorage(format!("Failed to save input data, can't create directory for input {input_name}: {err}"))
        })
}

pub fn get_geoip_path(storage_dir: &str) -> PathBuf {
    Path::new(storage_dir).join("geoip.db")
}

pub fn get_file_path_for_db_index(db_path: &Path) -> PathBuf {
    db_path.with_extension(storage_const::FILE_SUFFIX_INDEX)
}
