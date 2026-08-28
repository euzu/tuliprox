use crate::storage_const;
use fs2::FileExt;
use shared::{concat_string, error::TuliproxError};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tuliprox_core::{model::Config, utils};
use uuid::Uuid;

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
            error(format!("Failed to save {label} data, can't create directory {}: {err}", path.display()))
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

pub fn sanitize_name(name: &str) -> String { name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect() }

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
    let path = get_input_storage_path(input_name, &cfg.storage_dir).await.map_err(|err| {
        TuliproxError::RepositoryStorage(format!(
            "Failed to save input data, can't create directory for input {input_name}: {err}"
        ))
    })?;
    cleanup_orphaned_staging_artifacts(&path, ORPHAN_STAGING_MIN_AGE);
    Ok(path)
}

/// Age threshold below which a `.refresh-<uuid>.<ext>` file is treated as
/// still in flight and left alone. Refreshes on large Xtream providers can
/// run 5–15 minutes, so the threshold is set to 30 minutes — comfortably
/// above the slowest legitimate refresh, while still bounding stale-file
/// disk usage to roughly half an hour after a crash.
const ORPHAN_STAGING_MIN_AGE: Duration = Duration::from_mins(30);

const REFRESH_GENERATION_GUARD_PREFIX: &str = ".xtream-refresh-";
const REFRESH_GENERATION_GUARD_SUFFIX: &str = ".lease";
const XTREAM_REFRESH_DATABASE_STEMS: [&str; 3] = ["live", "video", "series"];
const XTREAM_REFRESH_CATEGORY_STEMS: [&str; 3] =
    [storage_const::COL_CAT_LIVE, storage_const::COL_CAT_VOD, storage_const::COL_CAT_SERIES];

/// Owns the cross-process lock for one complete Xtream refresh generation.
///
/// The guard file is created before any staging artifact and the lock remains
/// held until every clone of the corresponding refresh lease has been dropped.
#[derive(Debug)]
pub struct XtreamRefreshGenerationGuard {
    file: Option<File>,
    path: PathBuf,
}

impl XtreamRefreshGenerationGuard {
    pub fn acquire(storage_path: &Path, generation: Uuid) -> io::Result<Self> {
        let path = refresh_generation_guard_path(storage_path, generation);
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        if let Err(error) = file.lock_exclusive() {
            drop(file);
            if let Err(remove_error) = std::fs::remove_file(&path) {
                if remove_error.kind() != io::ErrorKind::NotFound {
                    log::warn!(
                        "Failed to remove unusable Xtream refresh generation guard {}: {remove_error}",
                        path.display()
                    );
                }
            }
            return Err(error);
        }
        Ok(Self { file: Some(file), path })
    }

    fn from_locked_file(path: PathBuf, file: File) -> Self { Self { file: Some(file), path } }
}

impl Drop for XtreamRefreshGenerationGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            if let Err(error) = FileExt::unlock(&file) {
                log::warn!(
                    "Failed to unlock Xtream refresh generation guard {}: {error}; the OS will release it on close",
                    self.path.display()
                );
            }
            drop(file);
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!("Failed to remove Xtream refresh generation guard {}: {error}", self.path.display());
            }
        }
    }
}

pub fn refresh_generation_guard_path(storage_path: &Path, generation: Uuid) -> PathBuf {
    storage_path
        .join(format!("{REFRESH_GENERATION_GUARD_PREFIX}{}{REFRESH_GENERATION_GUARD_SUFFIX}", generation.simple()))
}

enum RefreshGenerationClaim {
    Active,
    Acquired(XtreamRefreshGenerationGuard),
}

fn try_claim_refresh_generation(storage_path: &Path, generation: Uuid) -> io::Result<RefreshGenerationClaim> {
    let path = refresh_generation_guard_path(storage_path, generation);
    for _ in 0..2 {
        let file = match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match OpenOptions::new().read(true).write(true).open(&path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        return match file.try_lock_exclusive() {
            Ok(()) => Ok(RefreshGenerationClaim::Acquired(XtreamRefreshGenerationGuard::from_locked_file(path, file))),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(RefreshGenerationClaim::Active),
            Err(error) => Err(error),
        };
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("Xtream refresh generation guard {} changed while cleanup tried to claim it", path.display()),
    ))
}

#[derive(Default)]
struct OrphanRefreshGeneration {
    artifacts: Vec<PathBuf>,
    newest_modified: Option<SystemTime>,
    inspection_failed: bool,
}

/// Removes staging files left behind by aborted refreshes (`.refresh-<uuid>.<ext>`).
///
/// The mtime threshold only makes a generation eligible for cleanup. Before
/// deleting anything, cleanup must exclusively claim the generation guard and
/// retain it through every deletion attempt. Errors are logged and otherwise
/// ignored because cleanup is best-effort and must fail closed.
pub fn cleanup_orphaned_staging_artifacts(storage_path: &Path, min_age: Duration) {
    cleanup_orphaned_staging_artifacts_with_hook(storage_path, min_age, |_, _| {});
}

fn cleanup_orphaned_staging_artifacts_with_hook(
    storage_path: &Path,
    min_age: Duration,
    mut before_delete: impl FnMut(Uuid, &[PathBuf]),
) {
    let now = SystemTime::now();
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
    let mut generations = HashMap::<Uuid, OrphanRefreshGeneration>::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Deletion starts only after the complete scan. An incomplete directory
                // view cannot prove that every artifact of a generation is old, so abort
                // the entire best-effort pass without deleting anything.
                log::warn!(
                    "Failed to enumerate storage path {} for orphaned refresh artifacts: {error}",
                    storage_path.display()
                );
                return;
            }
        };
        let leaf = entry.file_name();
        let Some(name) = leaf.to_str() else { continue };
        let (generation, is_guard) = if let Some(generation) = refresh_generation_from_staging_name(name) {
            (generation, false)
        } else if let Some(generation) = refresh_generation_from_guard_name(name) {
            (generation, true)
        } else {
            continue;
        };
        let generation_artifacts = generations.entry(generation).or_default();
        let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(error) => {
                log::warn!("Failed to inspect Xtream refresh generation artifact {}: {error}", entry.path().display());
                generation_artifacts.inspection_failed = true;
                continue;
            }
        };
        if generation_artifacts.newest_modified.is_none_or(|newest_modified| modified > newest_modified) {
            generation_artifacts.newest_modified = Some(modified);
        }
        if !is_guard {
            generation_artifacts.artifacts.push(entry.path());
        }
    }

    for (generation, generation_artifacts) in generations {
        if generation_artifacts.inspection_failed {
            log::debug!("Skipping Xtream refresh generation {generation}: artifact inspection failed");
            continue;
        }
        let Some(newest_modified) = generation_artifacts.newest_modified else {
            continue;
        };
        let age = now.duration_since(newest_modified).unwrap_or_default();
        if age < min_age {
            log::debug!("Skipping recent Xtream refresh generation {generation} (age {age:?} < {min_age:?})");
            continue;
        }
        let generation_guard = match try_claim_refresh_generation(storage_path, generation) {
            Ok(RefreshGenerationClaim::Active) => {
                log::debug!("Skipping Xtream refresh generation {generation}: generation lease is active");
                continue;
            }
            Ok(RefreshGenerationClaim::Acquired(generation_guard)) => generation_guard,
            Err(error) => {
                log::debug!("Skipping Xtream refresh generation {generation}: generation guard probe failed: {error}");
                continue;
            }
        };
        before_delete(generation, &generation_artifacts.artifacts);
        for artifact in &generation_artifacts.artifacts {
            match std::fs::remove_file(artifact) {
                Ok(()) => log::info!("Removed orphaned refresh artifact {}", artifact.display()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    log::warn!("Failed to remove orphaned refresh artifact {}: {error}", artifact.display());
                }
            }
        }
        drop(generation_guard);
    }
}

#[cfg(test)]
fn is_orphan_staging_name(leaf: &str) -> bool { refresh_generation_from_staging_name(leaf).is_some() }

fn refresh_generation_from_staging_name(leaf: &str) -> Option<Uuid> {
    let (stem, generation_and_suffix) = leaf.split_once(".refresh-")?;
    let generation_bytes = generation_and_suffix.as_bytes();
    if generation_bytes.len() <= 32 {
        return None;
    }
    let generation = generation_and_suffix.get(..32)?;
    if !generation.as_bytes().iter().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) {
        return None;
    }
    let suffix = generation_and_suffix.get(32..)?;
    let known_artifact = match suffix {
        ".db" | ".idx" | ".db.wal" | ".db.wal.tmp" => XTREAM_REFRESH_DATABASE_STEMS.contains(&stem),
        ".lock" => {
            stem.strip_prefix('.').is_some_and(|database_stem| XTREAM_REFRESH_DATABASE_STEMS.contains(&database_stem))
        }
        ".json" => XTREAM_REFRESH_CATEGORY_STEMS.contains(&stem),
        _ => false,
    };
    if known_artifact {
        Uuid::parse_str(generation).ok()
    } else {
        None
    }
}

fn refresh_generation_from_guard_name(leaf: &str) -> Option<Uuid> {
    let generation =
        leaf.strip_prefix(REFRESH_GENERATION_GUARD_PREFIX)?.strip_suffix(REFRESH_GENERATION_GUARD_SUFFIX)?;
    let bytes = generation.as_bytes();
    if bytes.len() != 32 || !bytes.iter().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) {
        return None;
    }
    Uuid::parse_str(generation).ok()
}

pub fn get_geoip_path(storage_dir: &str) -> PathBuf { Path::new(storage_dir).join("geoip.db") }

// The sorted-index sidecar name is part of the B+Tree on-disk layout, so the
// engine owns it. Re-exported here because every repository already reaches for
// it through `repository::storage`.
pub use super::bplustree::get_file_path_for_db_index;

#[cfg(test)]
mod tests {
    use super::{
        cleanup_orphaned_staging_artifacts, cleanup_orphaned_staging_artifacts_with_hook, is_orphan_staging_name,
        refresh_generation_guard_path, XtreamRefreshGenerationGuard,
    };
    use fs2::FileExt;
    use std::{
        env, fs, io,
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant, SystemTime},
    };
    use uuid::Uuid;

    const CROSS_PROCESS_TEST_ROOT: &str = "TULIPROX_XTREAM_GENERATION_GUARD_TEST_ROOT";
    const CROSS_PROCESS_GENERATION: &str = "55555555555555555555555555555555";
    const MATCHER_TEST_GENERATION: &str = "0d8f0d8f0d8f0d8f0d8f0d8f0d8f0d8f";

    #[test]
    fn refresh_artifact_matcher_accepts_only_production_xtream_artifacts() {
        for database_stem in ["live", "video", "series"] {
            for suffix in [".db", ".idx", ".db.wal", ".db.wal.tmp"] {
                let artifact = format!("{database_stem}.refresh-{MATCHER_TEST_GENERATION}{suffix}");
                assert!(is_orphan_staging_name(&artifact), "production artifact did not match: {artifact}");
            }
            let sidecar = format!(".{database_stem}.refresh-{MATCHER_TEST_GENERATION}.lock");
            assert!(is_orphan_staging_name(&sidecar), "production sidecar did not match: {sidecar}");
        }
        for category_stem in ["cat_live", "cat_vod", "cat_series"] {
            let categories = format!("{category_stem}.refresh-{MATCHER_TEST_GENERATION}.json");
            assert!(is_orphan_staging_name(&categories), "production category did not match: {categories}");
        }

        assert!(!is_orphan_staging_name("live.db"));
        assert!(!is_orphan_staging_name("cat_live.json"));
        assert!(!is_orphan_staging_name("live.refresh-fixed.db"));
        assert!(!is_orphan_staging_name(&format!("unknown.refresh-{MATCHER_TEST_GENERATION}.db")));
        assert!(!is_orphan_staging_name(&format!("live.refresh-{MATCHER_TEST_GENERATION}.backup")));
        assert!(!is_orphan_staging_name(&format!("live.extra.refresh-{MATCHER_TEST_GENERATION}.db")));
        assert!(!is_orphan_staging_name(&format!("live.refresh-{MATCHER_TEST_GENERATION}.db.extra")));
        assert!(!is_orphan_staging_name(&format!("é.refresh-{MATCHER_TEST_GENERATION}.db")));
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
        let mtime = SystemTime::now().checked_sub(age).expect("age fits in system time");
        let file = fs::OpenOptions::new().write(true).open(path).expect("reopen fixture");
        file.set_modified(mtime).expect("set mtime");
    }

    fn wait_for_path(path: &Path, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, format!("timed out waiting for {}", path.display())))
    }

    struct ReapChildOnDrop {
        child: Option<Child>,
    }

    impl ReapChildOnDrop {
        fn new(child: Child) -> Self { Self { child: Some(child) } }

        fn wait_for_exit(mut self, timeout: Duration) -> io::Result<ExitStatus> {
            let deadline = Instant::now() + timeout;
            loop {
                let child = self
                    .child
                    .as_mut()
                    .ok_or_else(|| io::Error::other("Xtream generation guard child is unavailable"))?;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.child.take();
                        return Ok(status);
                    }
                    Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                    Ok(None) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "Xtream generation guard child timed out"));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    impl Drop for ReapChildOnDrop {
        fn drop(&mut self) {
            let Some(mut child) = self.child.take() else {
                return;
            };
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
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
        let unknown_stem = dir.path().join("custom.refresh-22222222222222222222222222222222.db");
        let unknown_suffix = dir.path().join("live.refresh-22222222222222222222222222222222.backup");

        fs::write(&published, b"kept").expect("write published");
        touch_with_age(&active, Duration::from_secs(5));
        touch_with_age(&orphan_db, Duration::from_hours(1));
        touch_with_age(&orphan_cat, Duration::from_hours(1));
        touch_with_age(&unknown_stem, Duration::from_hours(1));
        touch_with_age(&unknown_suffix, Duration::from_hours(1));

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        assert!(published.exists());
        assert!(active.exists(), "in-flight staging must survive");
        assert!(!orphan_db.exists());
        assert!(!orphan_cat.exists());
        assert!(unknown_stem.exists(), "unknown refresh-like stem must survive");
        assert!(unknown_suffix.exists(), "unknown refresh-like suffix must survive");
    }

    #[test]
    fn cleanup_skips_active_generation_older_than_min_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generation = Uuid::parse_str("44444444444444444444444444444444").expect("valid generation");
        let active = dir.path().join("live.refresh-44444444444444444444444444444444.db");
        let categories = dir.path().join("cat_live.refresh-44444444444444444444444444444444.json");
        let sidecar = dir.path().join(".live.refresh-44444444444444444444444444444444.lock");
        let guard_path = refresh_generation_guard_path(dir.path(), generation);
        let generation_guard =
            XtreamRefreshGenerationGuard::acquire(dir.path(), generation).expect("acquire active generation guard");

        touch_with_age(&active, Duration::from_hours(1));
        touch_with_age(&categories, Duration::from_hours(1));
        touch_with_age(&sidecar, Duration::from_hours(1));

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);

        assert!(active.exists(), "active generation older than min_age must survive");
        assert!(categories.exists(), "active category staging must survive");
        assert!(sidecar.exists(), "active generation sidecar must survive");
        assert!(guard_path.exists(), "active generation guard must survive");

        drop(generation_guard);
        cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);

        assert!(!active.exists());
        assert!(!categories.exists());
        assert!(!sidecar.exists());
        assert!(!guard_path.exists());
    }

    #[test]
    fn cleanup_removes_every_known_artifact_of_old_orphan_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generation = Uuid::parse_str("66666666666666666666666666666666").expect("valid generation");
        let artifacts = [
            dir.path().join("live.refresh-66666666666666666666666666666666.db"),
            dir.path().join("live.refresh-66666666666666666666666666666666.idx"),
            dir.path().join("live.refresh-66666666666666666666666666666666.db.wal"),
            dir.path().join("live.refresh-66666666666666666666666666666666.db.wal.tmp"),
            dir.path().join(".live.refresh-66666666666666666666666666666666.lock"),
            dir.path().join("cat_live.refresh-66666666666666666666666666666666.json"),
        ];
        let guard_path = refresh_generation_guard_path(dir.path(), generation);
        for artifact in &artifacts {
            touch_with_age(artifact, Duration::from_hours(1));
        }

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        for artifact in artifacts {
            assert!(!artifact.exists(), "known orphan artifact survived: {}", artifact.display());
        }
        assert!(!guard_path.exists());
    }

    #[test]
    fn cleanup_removes_old_guard_without_generation_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generation = Uuid::parse_str("88888888888888888888888888888888").expect("valid generation");
        let guard_path = refresh_generation_guard_path(dir.path(), generation);
        touch_with_age(&guard_path, Duration::from_hours(1));

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::from_mins(10));

        assert!(!guard_path.exists(), "old unlocked guard-only orphan must be removed");
    }

    #[test]
    fn cleanup_holds_generation_claim_through_artifact_deletion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let generation = Uuid::parse_str("77777777777777777777777777777777").expect("valid generation");
        let orphan = dir.path().join("live.refresh-77777777777777777777777777777777.db");
        let guard_path = refresh_generation_guard_path(dir.path(), generation);
        touch_with_age(&orphan, Duration::from_hours(1));
        touch_with_age(&guard_path, Duration::from_hours(1));

        let cleanup_path = dir.path().to_path_buf();
        let (claimed_sender, claimed_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let cleanup = thread::spawn(move || {
            cleanup_orphaned_staging_artifacts_with_hook(
                &cleanup_path,
                Duration::from_mins(10),
                move |claimed_generation, _| {
                    claimed_sender.send(claimed_generation).expect("report claimed generation");
                    release_receiver.recv().expect("wait for competing lock probe");
                },
            );
        });

        assert_eq!(
            claimed_receiver.recv_timeout(Duration::from_secs(5)).expect("cleanup should claim generation"),
            generation
        );
        let competing_file =
            fs::OpenOptions::new().read(true).write(true).open(&guard_path).expect("open claimed generation guard");
        let contention =
            competing_file.try_lock_exclusive().expect_err("cleanup must retain generation lock through deletion");
        assert_eq!(contention.kind(), io::ErrorKind::WouldBlock);
        release_sender.send(()).expect("release cleanup");
        cleanup.join().expect("cleanup thread should complete");

        assert!(!orphan.exists());
        assert!(!guard_path.exists());
    }

    #[test]
    fn generation_guard_child_process() -> io::Result<()> {
        let Some(storage_root) = env::var_os(CROSS_PROCESS_TEST_ROOT) else {
            return Ok(());
        };
        let storage_root = PathBuf::from(storage_root);
        let generation = Uuid::parse_str(CROSS_PROCESS_GENERATION).map_err(io::Error::other)?;
        let _generation_guard = XtreamRefreshGenerationGuard::acquire(&storage_root, generation)?;
        let orphan = storage_root.join(format!("live.refresh-{CROSS_PROCESS_GENERATION}.db"));
        let sidecar = storage_root.join(format!(".live.refresh-{CROSS_PROCESS_GENERATION}.lock"));
        touch_with_age(&orphan, Duration::from_hours(1));
        touch_with_age(&sidecar, Duration::from_hours(1));
        fs::write(storage_root.join("ready"), b"ready")?;
        wait_for_path(&storage_root.join("release"), Duration::from_secs(10))?;

        std::process::exit(0);
    }

    #[test]
    fn cleanup_coordinates_generation_ownership_across_processes() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let generation = Uuid::parse_str(CROSS_PROCESS_GENERATION).map_err(io::Error::other)?;
        let orphan = dir.path().join(format!("live.refresh-{CROSS_PROCESS_GENERATION}.db"));
        let sidecar = dir.path().join(format!(".live.refresh-{CROSS_PROCESS_GENERATION}.lock"));
        let guard_path = refresh_generation_guard_path(dir.path(), generation);
        let child = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("storage::tests::generation_guard_child_process")
            .arg("--nocapture")
            .env(CROSS_PROCESS_TEST_ROOT, dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let child = ReapChildOnDrop::new(child);
        wait_for_path(&dir.path().join("ready"), Duration::from_secs(10))?;

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);
        assert!(orphan.exists(), "artifact owned by child generation must survive");
        assert!(sidecar.exists(), "sidecar owned by child generation must survive");
        assert!(guard_path.exists(), "active child generation guard must survive");

        fs::write(dir.path().join("release"), b"release")?;
        let status = child.wait_for_exit(Duration::from_secs(10))?;
        assert!(status.success(), "generation guard child failed with {status}");

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);
        assert!(!orphan.exists());
        assert!(!sidecar.exists());
        assert!(!guard_path.exists());
        Ok(())
    }
}
