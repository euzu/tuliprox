use crate::model::Config;
use crate::repository::bplustree::common::sidecar_lock_path;
use crate::repository::storage_const;
use crate::utils;
use fs2::FileExt;
use shared::concat_string;
use shared::error::TuliproxError;
use std::path::{Path, PathBuf};

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
    let path = get_input_storage_path(input_name, &cfg.storage_dir).await
        .map_err(|err| {
            TuliproxError::RepositoryStorage(format!("Failed to save input data, can't create directory for input {input_name}: {err}"))
        })?;
    cleanup_orphaned_staging_artifacts(&path, ORPHAN_STAGING_MIN_AGE);
    Ok(path)
}

/// Age threshold below which a `.refresh-<uuid>.<ext>` file is treated as
/// still in flight and left alone. Refreshes on large Xtream providers can
/// run 5–15 minutes, so the threshold is set to 30 minutes — comfortably
/// above the slowest legitimate refresh, while still bounding stale-file
/// disk usage to roughly half an hour after a crash.
#[allow(clippy::duration_suboptimal_units)]
const ORPHAN_STAGING_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Removes staging files left behind by aborted refreshes (`.refresh-<uuid>.<ext>`).
///
/// Best-effort: only the leaf filename pattern and an mtime check decide what
/// to delete, never the file contents, so a successfully published file is
/// never deleted and an in-flight parallel refresh is never interrupted.
/// Errors are logged and otherwise ignored — this is a cleanup helper, not a
/// guard.
pub(crate) fn cleanup_orphaned_staging_artifacts(storage_path: &Path, min_age: std::time::Duration) {
    let now = std::time::SystemTime::now();
    let entries = match std::fs::read_dir(storage_path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!(
                "Failed to scan storage path {} for orphaned refresh artifacts: {error}",
                storage_path.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let leaf = entry.file_name();
        let Some(name) = leaf.to_str() else { continue };
        if !is_orphan_staging_name(name) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                log::warn!(
                    "Failed to stat refresh artifact {}: {error}",
                    entry.path().display()
                );
                continue;
            }
        };
        let age = match metadata.modified() {
            Ok(modified) => now.duration_since(modified).unwrap_or_default(),
            Err(error) => {
                log::warn!(
                    "Failed to read mtime of refresh artifact {}: {error}",
                    entry.path().display()
                );
                continue;
            }
        };
        if age < min_age {
            log::debug!(
                "Skipping recent refresh artifact {} (age {:?} < {:?})",
                entry.path().display(),
                age,
                min_age
            );
            continue;
        }
        // Cross-process safety: an active refresh holds an exclusive `flock` on the
        // staging sidecar lock file. A non-blocking attempt that fails means another
        // process (or a still-active older lease in this process) owns the artifact,
        // even if the mtime looks stale. The lock handle is held through `remove_file`
        // so a new refresh cannot acquire the sidecar and start writing before the
        // deletion lands. We never create or truncate the sidecar: a missing sidecar
        // means nothing owns the artifact, and creating it here would leave a stale
        // `.lock` behind for every probed orphan.
        let lock_path = sidecar_lock_path(&entry.path());
        let lock_file = match std::fs::OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                log::debug!(
                    "Skipping refresh artifact {}: sidecar lock probe failed: {error}",
                    entry.path().display()
                );
                continue;
            }
        };
        if let Some(lock_file) = lock_file {
            if lock_file.try_lock_exclusive().is_err() {
                log::debug!(
                    "Skipping refresh artifact {} owned by an active generation",
                    entry.path().display()
                );
                continue;
            }
        }
        let remove_result = std::fs::remove_file(entry.path());
        debug_assert!(
            !lock_path.exists(),
            "cleanup must not leave a generated sidecar behind at {}",
            lock_path.display()
        );
        if let Err(error) = remove_result {
            log::warn!(
                "Failed to remove orphaned refresh artifact {}: {error}",
                entry.path().display()
            );
        } else {
            log::info!("Removed orphaned refresh artifact {}", entry.path().display());
        }
    }
}

fn is_orphan_staging_name(leaf: &str) -> bool {
    // `<stem>.refresh-<uuid>.<ext>`. The UUID is 32 lowercase hex chars via
    // `Uuid::simple()`; requiring the dot boundary after the 32 hex chars and
    // lowercase-only hex keeps operators safe from accidental matches on
    // filenames that happen to contain `.refresh-`.
    leaf.match_indices(".refresh-").any(|(index, _)| {
        let after = &leaf[index + ".refresh-".len()..];
        let uuid_bytes = after.as_bytes();
        if uuid_bytes.len() <= 33 || uuid_bytes[32] != b'.' {
            return false;
        }
        let uuid = &uuid_bytes[..32];
        uuid.iter().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}


pub fn get_geoip_path(storage_dir: &str) -> PathBuf {
    Path::new(storage_dir).join("geoip.db")
}

pub fn get_file_path_for_db_index(db_path: &Path) -> PathBuf {
    db_path.with_extension(storage_const::FILE_SUFFIX_INDEX)
}

#[cfg(test)]
mod tests {
    use super::{cleanup_orphaned_staging_artifacts, is_orphan_staging_name};
    use fs2::FileExt;
    use std::{
        fs,
        path::Path,
        time::{Duration, SystemTime},
    };

    #[test]
    fn refresh_artifacts_match_orphan_pattern() {
        assert!(is_orphan_staging_name("live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8f.db"));
        assert!(is_orphan_staging_name("cat_live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8f.json"));
        assert!(!is_orphan_staging_name("live.db"));
        assert!(!is_orphan_staging_name("cat_live.json"));
        assert!(!is_orphan_staging_name("live.refresh-fixed.db"));
    }

    #[test]
    fn orphan_pattern_rejects_missing_extension_boundary() {
        // 32 hex chars but no dot boundary after them — must not match.
        assert!(!is_orphan_staging_name("live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8fdb"));
        // 32 hex chars plus extra suffix with no extension — must not match.
        assert!(!is_orphan_staging_name("live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8fX"));
        // 32 hex chars plus dot but no extension after the dot — must not match.
        assert!(!is_orphan_staging_name("live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8f."));
    }

    #[test]
    fn orphan_pattern_rejects_uppercase_hex_identifiers() {
        // Uppercase hex must be rejected — `Uuid::simple()` always emits lowercase.
        assert!(!is_orphan_staging_name("live.refresh-0D8F0D8F0D8F0D8F0D8F0D8F0D8F0D8F.db"));
        assert!(!is_orphan_staging_name("live.refresh-0d8f0d8f0d8f0d8f0d8f0D8F0D8F0D8F.db"));
    }

    fn touch_with_age(path: &Path, age: Duration) {
        fs::write(path, b"fixture").expect("write fixture");
        let mtime = SystemTime::now()
            .checked_sub(age)
            .expect("age fits in system time");
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("reopen fixture");
        file.set_modified(mtime).expect("set mtime");
    }

    #[test]
    fn cleanup_leaves_recent_staging_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let active = dir.path().join("live.refresh-0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8f.db");
        // 5 seconds old — well under the 10-minute threshold, an in-flight refresh
        // would write here.
        touch_with_age(&active, Duration::from_secs(5));

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        assert!(active.exists(), "recent staging file must not be deleted by a parallel refresh");
    }

    #[test]
    fn cleanup_removes_only_old_refresh_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let published = dir.path().join("live.db");
        let active = dir.path().join("live.refresh-11111111111111111111111111111111.db");
        let orphan_db = dir.path().join("live.refresh-22222222222222222222222222222222.db");
        let orphan_cat = dir.path().join("cat_live.refresh-33333333333333333333333333333333.json");

        fs::write(&published, b"kept").expect("write published");
        touch_with_age(&active, Duration::from_secs(5));
        touch_with_age(&orphan_db, Duration::from_hours(1));
        touch_with_age(&orphan_cat, Duration::from_hours(1));

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        assert!(published.exists());
        assert!(active.exists(), "in-flight staging must survive");
        assert!(!orphan_db.exists());
        assert!(!orphan_cat.exists());
    }

    #[test]
    fn cleanup_skips_active_generation_older_than_min_age() {
        // An in-flight refresh that exceeds `min_age` because it is unusually slow
        // must not be reaped: the sidecar lock proves another owner is still alive.
        let dir = tempfile::tempdir().expect("tempdir");
        let active = dir.path().join("live.refresh-44444444444444444444444444444444.db");
        let sidecar = dir.path().join(".live.refresh-44444444444444444444444444444444.lock");

        touch_with_age(&active, Duration::from_hours(1));
        let lock_file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&sidecar)
            .expect("open sidecar");
        lock_file.lock_exclusive().expect("acquire active lease");

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        assert!(active.exists(), "active generation older than min_age must survive");
    }
}
