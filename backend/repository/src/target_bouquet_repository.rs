use log::debug;
use shared::{
    error::TuliproxError,
    model::{TargetBouquetDto, TargetBouquetFileDto, TARGET_BOUQUET_VERSION},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tuliprox_core::{
    model::AppConfig,
    utils::{write_file_atomic, write_text_file_atomic},
};

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

    Ok(Some(dto))
}

/// Saves a target bouquet, including unrestricted selections so the chosen mode is retained.
pub async fn save_target_bouquet(
    app_config: &AppConfig,
    target_name: &str,
    mut bouquet: TargetBouquetDto,
) -> Result<(), TuliproxError> {
    bouquet.groups.canonicalize_for_target();

    let dto = TargetBouquetFileDto::new(target_name, bouquet);
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

    rewrite_bouquet_with_target(&old_path, &new_path, new_target_name).await?;
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

async fn rewrite_bouquet_with_target(src: &Path, dest: &Path, new_target: &str) -> Result<(), TuliproxError> {
    let content = tokio::fs::read_to_string(src).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to read source bouquet file {}: {err}", src.display()))
    })?;

    let mut dto: TargetBouquetFileDto = serde_saphyr::from_str(&content).map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to parse source bouquet file {}: {err}", src.display()))
    })?;

    dto.target = new_target.to_string();
    dto.canonicalize();

    let yaml = serde_saphyr::to_string(&dto)
        .map_err(|err| TuliproxError::TargetBouquet(format!("Failed to serialize renamed bouquet: {err}")))?;

    write_text_file_atomic(dest, &yaml).await.map_err(|err| {
        TuliproxError::TargetBouquet(format!("Failed to write renamed bouquet to {}: {err}", dest.display()))
    })
}

/// Pre-validates that target bouquet rename mutations will not collide with existing destinations.
pub async fn validate_target_bouquet_mutations(
    config_path: &Path,
    renames: &[(String, String)],
    deletions: &[String],
) -> Result<(), TuliproxError> {
    let mut seen_sources = HashSet::with_capacity(renames.len());
    let mut seen_destinations = HashSet::with_capacity(renames.len());
    for (old_name, new_name) in renames {
        if !seen_sources.insert(old_name.as_str()) {
            return Err(TuliproxError::TargetBouquet(format!("Duplicate rename source '{old_name}'")));
        }
        if !seen_destinations.insert(new_name.as_str()) {
            return Err(TuliproxError::TargetBouquet(format!("Duplicate rename destination '{new_name}'")));
        }
    }

    let deleted_names: HashSet<&str> = deletions.iter().map(String::as_str).collect();
    let renamed_old_names: HashSet<&str> = renames.iter().map(|(old, _)| old.as_str()).collect();

    for (old_name, new_name) in renames {
        if old_name == new_name {
            continue;
        }
        let old_path = target_bouquet_path(config_path, old_name);
        let new_path = target_bouquet_path(config_path, new_name);

        if !tokio::fs::try_exists(&old_path).await.unwrap_or(false) {
            continue;
        }

        if tokio::fs::try_exists(&new_path).await.unwrap_or(false)
            && !deleted_names.contains(new_name.as_str())
            && !renamed_old_names.contains(new_name.as_str())
        {
            return Err(TuliproxError::TargetBouquet(format!(
                "Cannot rename target bouquet from '{old_name}' to '{new_name}': destination file {} already exists",
                new_path.display()
            )));
        }
    }
    Ok(())
}

/// Applies source-editor renames and deletions while acquiring the logical mutation lock.
pub async fn apply_target_bouquet_mutations(
    app_config: &AppConfig,
    renames: &[(String, String)],
    deletions: &[String],
) -> Result<(), TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(TARGET_BOUQUET_MUTATION_LOCK).await;
    apply_target_bouquet_mutations_locked(app_config, renames, deletions).await
}

type BouquetFileSnapshot = Vec<(PathBuf, Option<Vec<u8>>)>;

async fn capture_bouquet_snapshot(paths: &[PathBuf]) -> Result<BouquetFileSnapshot, TuliproxError> {
    let mut snapshot = Vec::with_capacity(paths.len());
    for path in paths {
        let content = match tokio::fs::read(path).await {
            Ok(content) => Some(content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(TuliproxError::TargetBouquet(format!(
                    "Failed to snapshot target bouquet {}: {err}",
                    path.display()
                )));
            }
        };
        snapshot.push((path.clone(), content));
    }
    Ok(snapshot)
}

async fn restore_bouquet_snapshot(snapshot: &BouquetFileSnapshot) -> Result<(), TuliproxError> {
    let mut failures = Vec::new();
    for (path, _) in snapshot {
        if let Err(err) = tokio::fs::remove_file(path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                failures.push(format!("failed to clear {} during rollback: {err}", path.display()));
            }
        }
    }
    for (path, content) in snapshot {
        if let Some(content) = content {
            if let Err(err) = write_file_atomic(path, content).await {
                failures.push(format!("failed to restore {} during rollback: {err}", path.display()));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(TuliproxError::TargetBouquet(failures.join("; ")))
    }
}

async fn rollback_bouquet_mutation(snapshot: &BouquetFileSnapshot, mutation_error: TuliproxError) -> TuliproxError {
    match restore_bouquet_snapshot(snapshot).await {
        Ok(()) => mutation_error,
        Err(rollback_error) => TuliproxError::TargetBouquet(format!(
            "{mutation_error}; target bouquet rollback also failed: {rollback_error}"
        )),
    }
}

/// Applies source-editor renames and deletions.
/// Caller MUST already hold `TARGET_BOUQUET_MUTATION_LOCK`.
/// All affected files are snapshotted and locked before mutation so chained renames,
/// swaps, deletions, and failures restore the exact pre-mutation state.
pub async fn apply_target_bouquet_mutations_locked(
    app_config: &AppConfig,
    renames: &[(String, String)],
    deletions: &[String],
) -> Result<(), TuliproxError> {
    let paths = app_config.paths.load();
    let config_path = Path::new(&paths.config_path);
    validate_target_bouquet_mutations(config_path, renames, deletions).await?;

    let mut affected_paths = Vec::with_capacity(renames.len() * 2 + deletions.len());
    for target_name in deletions {
        affected_paths.push(target_bouquet_path(config_path, target_name));
    }
    for (old_name, new_name) in renames {
        affected_paths.push(target_bouquet_path(config_path, old_name));
        affected_paths.push(target_bouquet_path(config_path, new_name));
    }
    affected_paths.sort_unstable();
    affected_paths.dedup();

    let mut file_locks = Vec::with_capacity(affected_paths.len());
    for path in &affected_paths {
        file_locks.push(app_config.file_locks.write_lock(path).await);
    }

    let snapshot = capture_bouquet_snapshot(&affected_paths).await?;
    let mut writes = Vec::with_capacity(renames.len());
    for (old_name, new_name) in renames {
        if old_name == new_name {
            continue;
        }
        let old_path = target_bouquet_path(config_path, old_name);
        let Some(content) =
            snapshot.iter().find(|(path, _)| path == &old_path).and_then(|(_, content)| content.as_deref())
        else {
            continue;
        };
        let content = std::str::from_utf8(content).map_err(|err| {
            TuliproxError::TargetBouquet(format!("Source bouquet file {} is not UTF-8: {err}", old_path.display()))
        })?;
        let mut dto: TargetBouquetFileDto = serde_saphyr::from_str(content).map_err(|err| {
            TuliproxError::TargetBouquet(format!("Failed to parse source bouquet file {}: {err}", old_path.display()))
        })?;
        if dto.version != TARGET_BOUQUET_VERSION || dto.target != *old_name {
            return Err(TuliproxError::TargetBouquet(format!(
                "Source bouquet {} does not match target '{old_name}' or version {TARGET_BOUQUET_VERSION}",
                old_path.display()
            )));
        }
        dto.target.clone_from(new_name);
        dto.canonicalize();
        let yaml = serde_saphyr::to_string(&dto)
            .map_err(|err| TuliproxError::TargetBouquet(format!("Failed to serialize renamed bouquet: {err}")))?;
        writes.push((target_bouquet_path(config_path, new_name), yaml));
    }

    for path in &affected_paths {
        if let Err(err) = tokio::fs::remove_file(path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                let mutation_error = TuliproxError::TargetBouquet(format!(
                    "Failed to clear target bouquet {} during mutation: {err}",
                    path.display()
                ));
                return Err(rollback_bouquet_mutation(&snapshot, mutation_error).await);
            }
        }
    }

    for (new_path, yaml) in writes {
        if let Err(err) = write_text_file_atomic(&new_path, &yaml).await {
            let mutation_error = TuliproxError::TargetBouquet(format!(
                "Failed to publish renamed target bouquet {}: {err}",
                new_path.display()
            ));
            return Err(rollback_bouquet_mutation(&snapshot, mutation_error).await);
        }
        debug!("Finalized renamed target bouquet at {}", new_path.display());
    }

    drop(file_locks);
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
    pub leftover_temp_files: Vec<PathBuf>,
}

/// Audits target bouquet files on disk against current active target names.
pub async fn audit_target_bouquets<S: std::hash::BuildHasher>(
    config_path: &Path,
    active_target_names: &HashSet<String, S>,
) -> Result<TargetBouquetAuditReport, TuliproxError> {
    let files = list_target_bouquet_files(config_path).await?;
    let mut report = TargetBouquetAuditReport::default();

    // Clean up and report any orphaned temporary staging files in the bouquets directory
    let dir = target_bouquet_dir(config_path);
    if let Ok(mut dir_reader) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = dir_reader.next_entry().await {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') && name.contains(".tmp-") {
                        let _ = tokio::fs::remove_file(&path).await;
                        report.leftover_temp_files.push(path);
                    }
                }
            }
        }
    }

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
    use shared::model::{ConfigPaths, PlaylistClusterBouquetDto, TargetBouquetMode};
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

    fn whitelist(groups: PlaylistClusterBouquetDto) -> TargetBouquetDto { TargetBouquetDto::whitelist(groups) }

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

        save_target_bouquet(&app_config, "family", whitelist(bouquet.clone())).await.unwrap();

        let loaded = load_target_bouquet(&app_config, "family").await.unwrap();
        assert!(loaded.is_some());
        let dto = loaded.unwrap();
        assert_eq!(dto.target, "family");
        assert_eq!(dto.bouquet.mode, TargetBouquetMode::Whitelist);
        assert_eq!(dto.bouquet.groups.live, Some(vec!["Kids".to_string(), "News".to_string()]));
        assert_eq!(dto.bouquet.groups.vod, Some(vec!["Action".to_string()]));
        assert!(dto.bouquet.groups.series.is_none());
    }

    #[tokio::test]
    async fn saving_unrestricted_retains_the_selected_mode() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };

        save_target_bouquet(&app_config, "family", whitelist(bouquet)).await.unwrap();
        assert!(target_bouquet_exists(temp_dir.path(), "family").await);

        // Save unrestricted (empty vectors)
        let empty_bouquet = PlaylistClusterBouquetDto { live: Some(vec![]), vod: None, series: None };
        save_target_bouquet(&app_config, "family", TargetBouquetDto::new(TargetBouquetMode::Blacklist, empty_bouquet))
            .await
            .unwrap();
        assert!(target_bouquet_exists(temp_dir.path(), "family").await);
        let loaded = load_target_bouquet(&app_config, "family").await.unwrap().unwrap();
        assert_eq!(loaded.bouquet.mode, TargetBouquetMode::Blacklist);
        assert!(loaded.is_unrestricted());
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
        save_target_bouquet(&app_config, "old_name", whitelist(bouquet)).await.unwrap();
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
        save_target_bouquet(&app_config, "rename_me", whitelist(bouquet.clone())).await.unwrap();
        save_target_bouquet(&app_config, "delete_me", whitelist(bouquet)).await.unwrap();

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

    #[tokio::test]
    async fn mutation_validation_detects_destination_collisions() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());
        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };

        save_target_bouquet(&app_config, "target_a", whitelist(bouquet.clone())).await.unwrap();
        save_target_bouquet(&app_config, "target_b", whitelist(bouquet)).await.unwrap();

        // target_a -> target_b should collide because target_b exists and is not deleted
        let result = validate_target_bouquet_mutations(
            temp_dir.path(),
            &[("target_a".to_string(), "target_b".to_string())],
            &[],
        )
        .await;
        assert!(result.is_err());

        // target_a -> target_b is allowed if target_b is in deletions
        let result_with_deletion = validate_target_bouquet_mutations(
            temp_dir.path(),
            &[("target_a".to_string(), "target_b".to_string())],
            &["target_b".to_string()],
        )
        .await;
        assert!(result_with_deletion.is_ok());

        // Applying deletion before rename succeeds
        apply_target_bouquet_mutations(
            &app_config,
            &[("target_a".to_string(), "target_b".to_string())],
            &["target_b".to_string()],
        )
        .await
        .unwrap();

        assert!(!target_bouquet_exists(temp_dir.path(), "target_a").await);
        assert!(target_bouquet_exists(temp_dir.path(), "target_b").await);
    }

    #[tokio::test]
    async fn mutation_application_handles_chained_renames_and_swaps() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        // Test chained rename: 1 -> 2, 2 -> 3
        let bouquet1 = PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), vod: None, series: None };
        let bouquet2 = PlaylistClusterBouquetDto { live: Some(vec!["Sports".to_string()]), vod: None, series: None };
        save_target_bouquet(&app_config, "target_1", whitelist(bouquet1)).await.unwrap();
        save_target_bouquet(&app_config, "target_2", whitelist(bouquet2)).await.unwrap();

        let chained_renames =
            vec![("target_1".to_string(), "target_2".to_string()), ("target_2".to_string(), "target_3".to_string())];
        validate_target_bouquet_mutations(temp_dir.path(), &chained_renames, &[]).await.unwrap();
        apply_target_bouquet_mutations(&app_config, &chained_renames, &[]).await.unwrap();

        assert!(!target_bouquet_exists(temp_dir.path(), "target_1").await);
        let b2 = load_target_bouquet(&app_config, "target_2").await.unwrap().unwrap();
        assert_eq!(b2.bouquet.groups.live, Some(vec!["News".to_string()]));
        let b3 = load_target_bouquet(&app_config, "target_3").await.unwrap().unwrap();
        assert_eq!(b3.bouquet.groups.live, Some(vec!["Sports".to_string()]));

        // Test swap: 2 <-> 3
        let swap_renames =
            vec![("target_2".to_string(), "target_3".to_string()), ("target_3".to_string(), "target_2".to_string())];
        validate_target_bouquet_mutations(temp_dir.path(), &swap_renames, &[]).await.unwrap();
        apply_target_bouquet_mutations(&app_config, &swap_renames, &[]).await.unwrap();

        let swapped_b2 = load_target_bouquet(&app_config, "target_2").await.unwrap().unwrap();
        assert_eq!(swapped_b2.bouquet.groups.live, Some(vec!["Sports".to_string()]));
        let swapped_b3 = load_target_bouquet(&app_config, "target_3").await.unwrap().unwrap();
        assert_eq!(swapped_b3.bouquet.groups.live, Some(vec!["News".to_string()]));
    }

    #[tokio::test]
    async fn mutation_validation_rejects_duplicate_destinations() {
        let temp_dir = TempDir::new().unwrap();
        let renames =
            vec![("target_a".to_string(), "target_c".to_string()), ("target_b".to_string(), "target_c".to_string())];
        let err = validate_target_bouquet_mutations(temp_dir.path(), &renames, &[]).await.unwrap_err();
        assert!(err.to_string().contains("Duplicate rename destination 'target_c'"));
    }

    #[tokio::test]
    async fn mutation_application_rolls_back_on_corrupt_yaml() {
        let temp_dir = TempDir::new().unwrap();
        let app_config = test_app_config(temp_dir.path().to_str().unwrap());

        // Create target_ok with valid bouquet
        let bouquet = PlaylistClusterBouquetDto { live: Some(vec!["News".to_string()]), vod: None, series: None };
        save_target_bouquet(&app_config, "target_ok", whitelist(bouquet)).await.unwrap();
        let deleted_bouquet =
            PlaylistClusterBouquetDto { live: Some(vec!["Kids".to_string()]), vod: None, series: None };
        save_target_bouquet(&app_config, "delete_me", whitelist(deleted_bouquet)).await.unwrap();

        // Create target_corrupt with corrupt content directly on disk
        let corrupt_path = target_bouquet_path(temp_dir.path(), "target_corrupt");
        tokio::fs::create_dir_all(corrupt_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&corrupt_path, "not: [valid: {yaml").await.unwrap();

        let renames = vec![
            ("target_ok".to_string(), "target_ok_renamed".to_string()),
            ("target_corrupt".to_string(), "target_corrupt_renamed".to_string()),
        ];

        // Validation passes since destinations do not exist on disk
        validate_target_bouquet_mutations(temp_dir.path(), &renames, &[]).await.unwrap();

        // Application must fail due to corrupt YAML and roll back target_ok
        let result = apply_target_bouquet_mutations(&app_config, &renames, &["delete_me".to_string()]).await;
        assert!(result.is_err());

        // target_ok must have been restored or remain intact
        assert!(target_bouquet_exists(temp_dir.path(), "target_ok").await);
        assert!(target_bouquet_exists(temp_dir.path(), "delete_me").await);
        // target_corrupt must have been restored
        assert!(tokio::fs::try_exists(&corrupt_path).await.unwrap_or(false));
    }

    #[tokio::test]
    async fn audit_sweeps_leftover_temporary_staging_files() {
        let temp_dir = TempDir::new().unwrap();
        let bouquet_dir = target_bouquet_dir(temp_dir.path());
        tokio::fs::create_dir_all(&bouquet_dir).await.unwrap();

        let leftover_tmp = bouquet_dir.join(".some_target.yml.tmp-123-456");
        tokio::fs::write(&leftover_tmp, "stale temp content").await.unwrap();

        let report = audit_target_bouquets(temp_dir.path(), &HashSet::new()).await.unwrap();
        assert_eq!(report.leftover_temp_files, vec![leftover_tmp.clone()]);
        assert!(!tokio::fs::try_exists(&leftover_tmp).await.unwrap_or(false));
    }
}
