use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{PlaylistClusterBouquetDto, TargetBouquetFileDto, TARGET_BOUQUET_VERSION},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tuliprox_core::{model::AppConfig, utils::write_text_file_atomic};

pub const TARGET_BOUQUET_MUTATION_LOCK: &str = "target_bouquet:mutation";
const MAX_PREFIX_BYTES: usize = 64;

/// Returns the bouquet storage directory derived from `config_path`.
pub fn target_bouquet_dir(config_path: &Path) -> PathBuf { config_path.join("bouquets") }

/// Sanitizes a target name into a safe filesystem prefix (max 64 UTF-8 bytes).
pub fn sanitize_target_prefix(target_name: &str) -> String {
    let sanitized: String =
        target_name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();

    let mut prefix = String::new();
    for ch in sanitized.chars() {
        if prefix.len() + ch.len_utf8() > MAX_PREFIX_BYTES {
            break;
        }
        prefix.push(ch);
    }
    if prefix.is_empty() {
        prefix.push('_');
    }
    prefix
}

/// Derives the canonical bouquet filename for a given target name.
pub fn target_bouquet_file_name(target_name: &str) -> String {
    let prefix = sanitize_target_prefix(target_name);
    let hash = blake3::hash(target_name.as_bytes());
    let hex = hash.to_hex();
    format!("{}--{}.yml", prefix, &hex[..12])
}

/// Derives the full path to a target bouquet file.
pub fn target_bouquet_path(config_path: &Path, target_name: &str) -> PathBuf {
    target_bouquet_dir(config_path).join(target_bouquet_file_name(target_name))
}

/// Checks if a bouquet file exists for the given target.
pub async fn target_bouquet_exists(config_path: &Path, target_name: &str) -> bool {
    tokio::fs::try_exists(target_bouquet_path(config_path, target_name)).await.unwrap_or(false)
}

/// Loads a target bouquet from disk. Returns `Ok(None)` if the file does not exist.
/// Fails closed with an error on corruption, target mismatch, or unsupported version.
pub async fn load_target_bouquet(
    app_config: &AppConfig,
    target_name: &str,
) -> Result<Option<TargetBouquetFileDto>, TuliproxError> {
    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let file_path = target_bouquet_path(config_path, target_name);

    let _lock = app_config.file_locks.read_lock(&file_path).await;

    if !tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
        return Ok(None);
    }

    let content = match tokio::fs::read_to_string(&file_path).await {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(TuliproxError::TargetBouquet(format!(
                "Failed to read bouquet file {}: {err}",
                file_path.display()
            )));
        }
    };

    let mut dto: TargetBouquetFileDto = serde_saphyr::from_str(&content).map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to parse target bouquet YAML in {}: {err}", file_path.display()))
    })?;

    if dto.version != TARGET_BOUQUET_VERSION {
        return Err(TuliproxError::TargetBouquet(format!(
            "Unsupported target bouquet version {} in {}",
            dto.version,
            file_path.display()
        )));
    }

    if dto.target != target_name {
        return Err(TuliproxError::TargetBouquet(format!(
            "Target name mismatch in {}: file contains target '{}', expected '{target_name}'",
            file_path.display(),
            dto.target
        )));
    }

    dto.canonicalize();

    if dto.is_unrestricted() {
        warn!(
            "Target bouquet file {} is normalized to unrestricted; please save to clean up file",
            file_path.display()
        );
        return Ok(None);
    }

    Ok(Some(dto))
}

/// Saves a target bouquet. If `groups` is unrestricted (all clusters empty or None),
/// the file is removed from disk.
pub async fn save_target_bouquet(
    app_config: &AppConfig,
    target_name: &str,
    mut groups: PlaylistClusterBouquetDto,
) -> Result<(), TuliproxError> {
    groups.canonicalize_for_target();

    if groups.is_target_unrestricted() {
        return delete_target_bouquet(app_config, target_name).await;
    }

    let dto = TargetBouquetFileDto::new(target_name, groups);
    let yaml = serde_saphyr::to_string(&dto)
        .map_err(|err| TuliproxError::TargetBouquet(format!("Failed to serialize target bouquet to YAML: {err}")))?;

    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let file_path = target_bouquet_path(config_path, target_name);

    let _mutation_lock = app_config.file_locks.write_lock_str(TARGET_BOUQUET_MUTATION_LOCK).await;
    let _file_lock = app_config.file_locks.write_lock(&file_path).await;

    write_text_file_atomic(&file_path, &yaml).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to write target bouquet {}: {err}", file_path.display()))
    })?;

    debug!("Saved target bouquet for '{}' to {}", target_name, file_path.display());
    Ok(())
}

/// Deletes a target bouquet file idempotently.
pub async fn delete_target_bouquet(app_config: &AppConfig, target_name: &str) -> Result<(), TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(TARGET_BOUQUET_MUTATION_LOCK).await;
    delete_target_bouquet_locked(app_config, target_name).await
}

async fn delete_target_bouquet_locked(app_config: &AppConfig, target_name: &str) -> Result<(), TuliproxError> {
    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let file_path = target_bouquet_path(config_path, target_name);

    let _file_lock = app_config.file_locks.write_lock(&file_path).await;

    match tokio::fs::remove_file(&file_path).await {
        Ok(()) => {
            debug!("Deleted target bouquet file {}", file_path.display());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(TuliproxError::TargetBouquet(format!(
            "Failed to delete target bouquet file {}: {err}",
            file_path.display()
        ))),
    }
}

/// Renames a target bouquet when a target is renamed in Source Editor.
pub async fn rename_target_bouquet(
    app_config: &AppConfig,
    old_target_name: &str,
    new_target_name: &str,
) -> Result<(), TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(TARGET_BOUQUET_MUTATION_LOCK).await;
    rename_target_bouquet_locked(app_config, old_target_name, new_target_name).await
}

async fn rename_target_bouquet_locked(
    app_config: &AppConfig,
    old_target_name: &str,
    new_target_name: &str,
) -> Result<(), TuliproxError> {
    if old_target_name == new_target_name {
        return Ok(());
    }

    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    let old_path = target_bouquet_path(config_path, old_target_name);
    let new_path = target_bouquet_path(config_path, new_target_name);

    if old_path == new_path {
        return Ok(());
    }

    // Acquire locks in lexicographical order to prevent deadlocks
    let (_first_lock, _second_lock) = if old_path <= new_path {
        (app_config.file_locks.write_lock(&old_path).await, app_config.file_locks.write_lock(&new_path).await)
    } else {
        (app_config.file_locks.write_lock(&new_path).await, app_config.file_locks.write_lock(&old_path).await)
    };

    if !tokio::fs::try_exists(&old_path).await.unwrap_or(false) {
        return Ok(());
    }

    if tokio::fs::try_exists(&new_path).await.unwrap_or(false) {
        return Err(TuliproxError::TargetBouquet(format!(
            "Cannot rename bouquet: destination file {} already exists",
            new_path.display()
        )));
    }

    let content = tokio::fs::read_to_string(&old_path).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to read source bouquet file {}: {err}", old_path.display()))
    })?;

    let mut dto: TargetBouquetFileDto = serde_saphyr::from_str(&content).map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to parse source bouquet file {}: {err}", old_path.display()))
    })?;

    dto.target = new_target_name.to_string();
    dto.canonicalize();

    let yaml = serde_saphyr::to_string(&dto)
        .map_err(|err| TuliproxError::TargetBouquet(format!("Failed to serialize renamed bouquet: {err}")))?;

    write_text_file_atomic(&new_path, &yaml).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to write renamed bouquet to {}: {err}", new_path.display()))
    })?;

    let _ = tokio::fs::remove_file(&old_path).await;
    debug!(
        "Renamed target bouquet from '{}' ({}) to '{}' ({})",
        old_target_name,
        old_path.display(),
        new_target_name,
        new_path.display()
    );
    Ok(())
}

/// Applies source-editor renames and deletions while holding the logical mutation lock exactly once.
pub async fn apply_target_bouquet_mutations(
    app_config: &AppConfig,
    renames: &[(String, String)],
    deletions: &[String],
) -> Result<(), TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(TARGET_BOUQUET_MUTATION_LOCK).await;
    for (old_name, new_name) in renames {
        rename_target_bouquet_locked(app_config, old_name, new_name).await?;
    }
    for target_name in deletions {
        delete_target_bouquet_locked(app_config, target_name).await?;
    }
    Ok(())
}

/// Lists all bouquet files in `config/bouquets/`.
pub async fn list_target_bouquet_files(config_path: &Path) -> Result<Vec<PathBuf>, TuliproxError> {
    let dir = target_bouquet_dir(config_path);
    if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut dir_reader = tokio::fs::read_dir(&dir).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to read bouquets directory {}: {err}", dir.display()))
    })?;

    while let Some(entry) = dir_reader
        .next_entry()
        .await
        .map_err(|err| TuliproxError::TargetBouquet(format!("Failed to read bouquet directory entry: {err}")))?
    {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
            entries.push(path);
        }
    }
    Ok(entries)
}

/// Audit report for bouquets on disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TargetBouquetAuditReport {
    pub orphan_files: Vec<PathBuf>,
    pub corrupt_files: Vec<(PathBuf, String)>,
}

/// Audits target bouquet files on disk against current active target names.
pub async fn audit_target_bouquets<S: std::hash::BuildHasher>(
    config_path: &Path,
    active_target_names: &HashSet<String, S>,
) -> Result<TargetBouquetAuditReport, TuliproxError> {
    let files = list_target_bouquet_files(config_path).await?;
    let mut report = TargetBouquetAuditReport::default();

    for file in files {
        let content = match tokio::fs::read_to_string(&file).await {
            Ok(c) => c,
            Err(err) => {
                report.corrupt_files.push((file, format!("Failed to read: {err}")));
                continue;
            }
        };

        let dto: TargetBouquetFileDto = match serde_saphyr::from_str(&content) {
            Ok(d) => d,
            Err(err) => {
                report.corrupt_files.push((file, format!("YAML parse error: {err}")));
                continue;
            }
        };

        if !active_target_names.contains(&dto.target) {
            report.orphan_files.push(file);
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::ConfigPaths;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tuliprox_core::{
        model::{Config, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };

    fn test_app_config(config_path: &str) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: config_path.to_string(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    #[test]
    fn path_derivation_sanitizes_and_hashes() {
        let config_path = Path::new("/app/config");
        let path = target_bouquet_path(config_path, "Family & Kids/Living Room");
        let name = path.file_name().unwrap().to_str().unwrap();

        assert!(name.starts_with("Family___Kids_Living_Room--"));
        assert!(name.ends_with(".yml"));
        assert_eq!(path.parent().unwrap(), Path::new("/app/config/bouquets"));
    }

    #[test]
    fn different_names_with_same_sanitized_prefix_have_distinct_paths() {
        let config_path = Path::new("/app/config");
        let path1 = target_bouquet_path(config_path, "Target A!");
        let path2 = target_bouquet_path(config_path, "Target A?");
        assert_ne!(path1, path2);
    }

    #[tokio::test]
    async fn save_load_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let bouquet = PlaylistClusterBouquetDto {
            live: Some(vec!["Kids".to_string(), "News".to_string()]),
            vod: Some(vec!["Action".to_string()]),
            series: None,
        };

        save_target_bouquet(&app_config, "family", bouquet.clone()).await.unwrap();

        let loaded = load_target_bouquet(&app_config, "family").await.unwrap();
        assert!(loaded.is_some());
        let dto = loaded.unwrap();
        assert_eq!(dto.target, "family");
        assert_eq!(dto.groups.live, Some(vec!["Kids".to_string(), "News".to_string()]));
        assert_eq!(dto.groups.vod, Some(vec!["Action".to_string()]));
        assert!(dto.groups.series.is_none());
    }

    #[tokio::test]
    async fn saving_unrestricted_deletes_file() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };

        save_target_bouquet(&app_config, "family", bouquet).await.unwrap();
        assert!(target_bouquet_exists(temp_dir.path(), "family").await);

        // Save unrestricted (empty vectors)
        let empty_bouquet = PlaylistClusterBouquetDto { live: Some(vec![]), vod: None, series: None };
        save_target_bouquet(&app_config, "family", empty_bouquet).await.unwrap();
        assert!(!target_bouquet_exists(temp_dir.path(), "family").await);
        assert_eq!(load_target_bouquet(&app_config, "family").await.unwrap(), None);
    }

    #[tokio::test]
    async fn corrupt_yaml_fails_closed() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let file_path = target_bouquet_path(temp_dir.path(), "family");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, "not: valid: yaml: [").await.unwrap();

        let result = load_target_bouquet(&app_config, "family").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embedded_target_mismatch_fails() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let file_path = target_bouquet_path(temp_dir.path(), "family");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, "version: 1\ntarget: other_target\ngroups:\n  live:\n    - Kids\n").await.unwrap();

        let result = load_target_bouquet(&app_config, "family").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rename_moves_file_and_updates_embedded_target() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };
        save_target_bouquet(&app_config, "old_name", bouquet).await.unwrap();
        assert!(target_bouquet_exists(temp_dir.path(), "old_name").await);

        rename_target_bouquet(&app_config, "old_name", "new_name").await.unwrap();
        assert!(!target_bouquet_exists(temp_dir.path(), "old_name").await);
        assert!(target_bouquet_exists(temp_dir.path(), "new_name").await);

        let loaded = load_target_bouquet(&app_config, "new_name").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().target, "new_name");
    }

    #[tokio::test]
    async fn mutation_batch_holds_global_lock_without_self_deadlock() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());
        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };
        save_target_bouquet(&app_config, "rename_me", bouquet.clone()).await.unwrap();
        save_target_bouquet(&app_config, "delete_me", bouquet).await.unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            apply_target_bouquet_mutations(
                &app_config,
                &[("rename_me".to_string(), "renamed".to_string())],
                &["delete_me".to_string()],
            ),
        )
        .await
        .expect("mutation batch must not deadlock")
        .unwrap();

        assert!(target_bouquet_exists(temp_dir.path(), "renamed").await);
        assert!(!target_bouquet_exists(temp_dir.path(), "delete_me").await);
    }
}
