use crate::target_bouquet_repository::sanitize_target_prefix;
use log::debug;
use shared::{
    error::TuliproxError,
    model::{FailoverFileDto, OverrideValidationError, TargetFailoverOverridesDto, FAILOVER_FILE_VERSION},
};
use std::{
    collections::{hash_map::Entry, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, FailoverOverrides, TargetOverridesEntry},
    utils::{
        collect_config_yaml_files, remove_file_if_exists, resolve_failover_path, write_text_file_atomic, FailoverSource,
    },
};

/// Serializes all failover write-backs; the per-file lock only covers the file being rewritten.
const FAILOVER_MUTATION_LOCK: &str = "failover_overrides_mutation";

#[derive(Debug)]
pub enum FailoverWriteError {
    /// The section on disk differs from what the caller based its change on.
    Conflict {
        current_etag: Option<String>,
    },
    /// The section has validation findings and was not written.
    Invalid(Vec<OverrideValidationError>),
    /// The failover files cannot be edited, because they failed to load.
    Locked(String),
    Io(TuliproxError),
}

impl From<TuliproxError> for FailoverWriteError {
    fn from(err: TuliproxError) -> Self { Self::Io(err) }
}

fn failover_error(message: String) -> TuliproxError { TuliproxError::FailoverGroup(message) }

fn failover_files(source: &FailoverSource) -> Result<Vec<PathBuf>, TuliproxError> {
    match source {
        FailoverSource::File(path) => Ok(if path.is_file() { vec![path.clone()] } else { Vec::new() }),
        FailoverSource::Directory(dir) if dir.is_dir() => collect_config_yaml_files(dir)
            .map_err(|err| failover_error(format!("Failed to read failover directory {}: {err}", dir.display()))),
        FailoverSource::Directory(_) => Ok(Vec::new()),
    }
}

/// True for content without any YAML document: empty, whitespace only or comments only.
fn is_blank_yaml(content: &str) -> bool {
    content.lines().map(str::trim).all(|line| line.is_empty() || line.starts_with('#'))
}

/// Reads one failover file. The content is never env-resolved, so placeholders survive a write-back.
fn read_failover_file(path: &Path) -> Result<FailoverFileDto, TuliproxError> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| failover_error(format!("Failed to read failover file {}: {err}", path.display())))?;
    if is_blank_yaml(&content) {
        return Ok(FailoverFileDto { version: FAILOVER_FILE_VERSION, targets: Vec::new() });
    }
    let dto: FailoverFileDto = serde_saphyr::from_str(&content)
        .map_err(|err| failover_error(format!("Failed to parse failover file {}: {err}", path.display())))?;
    if dto.version != FAILOVER_FILE_VERSION {
        return Err(failover_error(format!(
            "Unsupported failover file version {} in {}, expected {FAILOVER_FILE_VERSION}",
            dto.version,
            path.display()
        )));
    }
    Ok(dto)
}

/// Adds `section` of `file` to `sections`; a target must be defined in one place only.
fn insert_section(
    sections: &mut HashMap<Arc<str>, Arc<TargetOverridesEntry>>,
    mut section: TargetFailoverOverridesDto,
    file: &Path,
) -> Result<(), TuliproxError> {
    section.canonicalize();
    match sections.entry(Arc::from(section.target.as_str())) {
        Entry::Occupied(existing) => Err(failover_error(format!(
            "Target '{}' is defined in {} and {}",
            existing.key(),
            existing.get().file.display(),
            file.display()
        ))),
        Entry::Vacant(slot) => {
            let findings = section.validate();
            let etag = Arc::from(section.etag());
            slot.insert(Arc::new(TargetOverridesEntry {
                overrides: section,
                file: file.to_path_buf(),
                etag,
                findings,
            }));
            Ok(())
        }
    }
}

/// Reads all failover files of `source` into a snapshot. A missing file or directory yields an empty snapshot.
pub fn load_failover_overrides(source: &FailoverSource) -> Result<FailoverOverrides, TuliproxError> {
    let files = failover_files(source)?;
    let mut sections: HashMap<Arc<str>, Arc<TargetOverridesEntry>> = HashMap::new();
    for file in &files {
        for section in read_failover_file(file)?.targets {
            insert_section(&mut sections, section, file)?;
        }
    }
    Ok(FailoverOverrides { source: Some(source.clone()), sections, files_used: files, error: None })
}

/// Runs blocking work off the async runtime; a task that did not finish is a failover error.
pub async fn run_blocking<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, TuliproxError> {
    tokio::task::spawn_blocking(job).await.map_err(|err| failover_error(format!("Failover task failed: {err}")))
}

async fn load_from_disk(source: &FailoverSource) -> Result<FailoverOverrides, TuliproxError> {
    let source = source.clone();
    run_blocking(move || load_failover_overrides(&source)).await?
}

/// Reads the failover files of `source` under the failover mutation lock, so the result never mixes in a
/// half finished Web UI write. Nothing is published.
pub async fn load_failover_snapshot(
    app_config: &AppConfig,
    source: &FailoverSource,
) -> Result<FailoverOverrides, TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(FAILOVER_MUTATION_LOCK).await;
    load_from_disk(source).await
}

/// Re-reads the failover files and swaps the snapshot, both under the failover mutation lock, so a reload
/// triggered by the file watcher cannot publish content older than a concurrent Web UI save.
/// Returns the previous and the new snapshot; a failing read keeps the previous one active.
pub async fn reload_failover_snapshot(
    app_config: &AppConfig,
    source: &FailoverSource,
) -> Result<(Arc<FailoverOverrides>, Arc<FailoverOverrides>), TuliproxError> {
    let _mutation_lock = app_config.file_locks.write_lock_str(FAILOVER_MUTATION_LOCK).await;
    let next = Arc::new(load_from_disk(source).await?);
    let previous = app_config.failover.swap(Arc::clone(&next));
    Ok((previous, next))
}

/// Source of the current snapshot; resolved from the paths when no snapshot was ever loaded.
fn editable_source(app_config: &AppConfig) -> Result<FailoverSource, FailoverWriteError> {
    let snapshot = app_config.failover.load();
    if let Some(error) = &snapshot.error {
        return Err(FailoverWriteError::Locked(error.clone()));
    }
    Ok(snapshot.source.clone().unwrap_or_else(|| {
        let paths = app_config.paths.load();
        let config = app_config.config.load();
        resolve_failover_path(
            &paths.config_path,
            paths.failover_file_path.as_deref().or(config.failover_path.as_deref()),
        )
    }))
}

fn prepare_section(
    target: &str,
    section: Option<TargetFailoverOverridesDto>,
) -> Result<Option<TargetFailoverOverridesDto>, FailoverWriteError> {
    let Some(mut section) = section else {
        return Ok(None);
    };
    section.canonicalize();
    section.target = target.to_string();
    if section.is_empty() {
        return Ok(None);
    }
    let findings = section.validate();
    if findings.is_empty() {
        Ok(Some(section))
    } else {
        Err(FailoverWriteError::Invalid(findings))
    }
}

fn section_index(file: &FailoverFileDto, target: &str) -> Option<usize> {
    file.targets.iter().position(|section| section.target.trim() == target)
}

fn canonical_etag(section: &TargetFailoverOverridesDto) -> String {
    let mut section = section.clone();
    section.canonicalize();
    section.etag()
}

fn check_etag(current_etag: Option<String>, expected_etag: Option<&str>) -> Result<(), FailoverWriteError> {
    if current_etag.as_deref() == expected_etag {
        Ok(())
    } else {
        Err(FailoverWriteError::Conflict { current_etag })
    }
}

/// File that receives a section no file defines yet.
fn new_section_file(source: &FailoverSource, target: &str) -> PathBuf {
    match source {
        FailoverSource::File(path) => path.clone(),
        FailoverSource::Directory(dir) => {
            let base = sanitize_target_prefix(target);
            let mut candidate = dir.join(format!("{base}.yml"));
            let mut suffix = 2;
            while candidate.exists() {
                candidate = dir.join(format!("{base}-{suffix}.yml"));
                suffix += 1;
            }
            candidate
        }
    }
}

/// Writes `file` back, or deletes it when it holds no sections and lives in a failover directory.
async fn persist_file(
    app_config: &AppConfig,
    source: &FailoverSource,
    path: &Path,
    file: &FailoverFileDto,
) -> Result<(), TuliproxError> {
    if file.targets.is_empty() && matches!(source, FailoverSource::Directory(_)) {
        return remove_file_if_exists(path)
            .map_err(|err| failover_error(format!("Failed to delete failover file {}: {err}", path.display())));
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            failover_error(format!("Failed to create failover directory {}: {err}", parent.display()))
        })?;
    }
    let yaml = serde_saphyr::to_string(file)
        .map_err(|err| failover_error(format!("Failed to serialize failover file {}: {err}", path.display())))?;
    write_text_file_atomic(path, &yaml)
        .await
        .map_err(|err| failover_error(format!("Failed to write failover file {}: {err}", path.display())))?;
    app_config.file_locks.mark_internal_write_content(path, yaml.as_bytes()).await;
    debug!("Saved failover overrides to {}", path.display());
    Ok(())
}

/// Snapshot after `path` was rewritten with `file`: the `disk` snapshot read under the same lock, with the
/// sections of that file replaced. Only the directory listing is read again, no failover file is parsed twice.
fn snapshot_after_write(
    source: &FailoverSource,
    disk: FailoverOverrides,
    path: &Path,
    file: FailoverFileDto,
) -> Result<FailoverOverrides, TuliproxError> {
    let mut sections = disk.sections;
    sections.retain(|_, entry| entry.file != path);
    for section in file.targets {
        insert_section(&mut sections, section, path)?;
    }
    Ok(FailoverOverrides { source: Some(source.clone()), sections, files_used: failover_files(source)?, error: None })
}

/// Publishes the snapshot that matches the disk after a write.
fn publish_snapshot(app_config: &AppConfig, snapshot: FailoverOverrides) -> Arc<FailoverOverrides> {
    let snapshot = Arc::new(snapshot);
    app_config.failover.store(Arc::clone(&snapshot));
    snapshot
}

/// Replaces (`Some`) or removes (`None`) the section of `target`.
///
/// `expected_etag` is the etag the caller based its change on, `None` meaning the section must not exist yet.
/// The check runs under the locks against the file content on disk, so hand edits that were not
/// reloaded yet are detected. Returns the stored entry, `None` when the section was removed.
pub async fn write_failover_section(
    app_config: &AppConfig,
    target: &str,
    expected_etag: Option<&str>,
    section: Option<TargetFailoverOverridesDto>,
) -> Result<Option<Arc<TargetOverridesEntry>>, FailoverWriteError> {
    let section = prepare_section(target, section)?;
    let source = editable_source(app_config)?;

    let _mutation_lock = app_config.file_locks.write_lock_str(FAILOVER_MUTATION_LOCK).await;
    let disk = load_from_disk(&source).await.map_err(|err| FailoverWriteError::Locked(err.to_string()))?;
    let path = disk.sections.get(target).map_or_else(|| new_section_file(&source, target), |entry| entry.file.clone());

    let _file_lock = app_config.file_locks.write_lock(&path).await;
    let mut file = if path.is_file() {
        let file_path = path.clone();
        run_blocking(move || read_failover_file(&file_path)).await??
    } else {
        FailoverFileDto { version: FAILOVER_FILE_VERSION, targets: Vec::new() }
    };

    let index = section_index(&file, target);
    check_etag(index.map(|idx| canonical_etag(&file.targets[idx])), expected_etag)?;
    match (index, section) {
        (Some(idx), Some(section)) => file.targets[idx] = section,
        (Some(idx), None) => {
            file.targets.remove(idx);
        }
        (None, Some(section)) => file.targets.push(section),
        (None, None) => return Ok(None),
    }

    persist_file(app_config, &source, &path, &file).await?;
    let snapshot = publish_snapshot(app_config, snapshot_after_write(&source, disk, &path, file)?);
    Ok(snapshot.sections.get(target).map(Arc::clone))
}

/// Moves the section of `from` to the target `to` by rewriting its `target:` field in place.
/// Conflicts when `from` is missing or changed on disk, or when `to` already has a section.
pub async fn reassign_failover_section(
    app_config: &AppConfig,
    from: &str,
    to: &str,
    expected_etag: &str,
) -> Result<Arc<TargetOverridesEntry>, FailoverWriteError> {
    let source = editable_source(app_config)?;

    let _mutation_lock = app_config.file_locks.write_lock_str(FAILOVER_MUTATION_LOCK).await;
    let disk = load_from_disk(&source).await.map_err(|err| FailoverWriteError::Locked(err.to_string()))?;
    if let Some(taken) = disk.sections.get(to) {
        return Err(FailoverWriteError::Conflict { current_etag: Some(taken.etag.to_string()) });
    }
    let Some(owner) = disk.sections.get(from) else {
        return Err(FailoverWriteError::Conflict { current_etag: None });
    };
    let path = owner.file.clone();

    let _file_lock = app_config.file_locks.write_lock(&path).await;
    let mut file = {
        let file_path = path.clone();
        run_blocking(move || read_failover_file(&file_path)).await??
    };
    let Some(index) = section_index(&file, from) else {
        return Err(FailoverWriteError::Conflict { current_etag: None });
    };
    check_etag(Some(canonical_etag(&file.targets[index])), Some(expected_etag))?;
    file.targets[index].target = to.to_string();

    persist_file(app_config, &source, &path, &file).await?;
    let snapshot = publish_snapshot(app_config, snapshot_after_write(&source, disk, &path, file)?);
    snapshot.sections.get(to).map(Arc::clone).ok_or_else(|| {
        FailoverWriteError::Io(failover_error(format!("Section '{to}' is missing after writing {}", path.display())))
    })
}

#[cfg(test)]
mod tests;
