use super::bplustree::{BPlusTree, MAGIC, STORAGE_VERSION};
use super::storage_const;
use fs2::FileExt as _;
use log::{info, warn};
use shared::model::{ConfigPaths, ProxyType, ProxyUserStatus};
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const LEGACY_STORAGE_VERSION: u32 = 1;
const METADATA_LEN_OFFSET: u64 = 16;
const METADATA_MAX_SIZE: u32 = 4000;
const HEADER_FLAG_HAS_METADATA_FLAGS: u32 = 1 << 31;
const HEADER_FLAG_HAS_TOMBSTONES: u32 = 1 << 30;
const HEADER_METADATA_LEN_MASK: u32 = !(HEADER_FLAG_HAS_METADATA_FLAGS | HEADER_FLAG_HAS_TOMBSTONES);
const MARKER_FILE_GUARD_PREFIX: &str = ".db_mergeto_v";
const MARKER_FILE_GUARD_PREFIX_LEGACY_ALT: &str = ".db_mergedto";
const MARKER_FILE_API_USER_GUARD: &str = ".userdb_mergeto_v3";
const MARKER_VERSION_KEY: &str = "migrated_to";
const MARKER_ROOTS_FINGERPRINT_KEY: &str = "roots_fingerprint";

#[derive(Debug, Clone, Copy, Default)]
pub struct BPlusTreeMigrationStats {
    pub scanned_files: usize,
    pub bplustree_files: usize,
    pub migrated_files: usize,
    pub skipped_by_marker: bool,
}

#[derive(Debug)]
struct BPlusTreeStartupMigrator {
    roots: Vec<PathBuf>,
    migration_marker_path: Option<PathBuf>,
}

impl BPlusTreeStartupMigrator {
    pub fn new(roots: Vec<PathBuf>) -> Self { Self { roots, migration_marker_path: None } }

    pub fn new_with_marker(roots: Vec<PathBuf>, migration_marker_path: PathBuf) -> Self {
        Self { roots, migration_marker_path: Some(migration_marker_path) }
    }

    pub fn run(&self) -> io::Result<BPlusTreeMigrationStats> {
        let mut stats = BPlusTreeMigrationStats::default();
        self.cleanup_legacy_root_markers(self.migration_marker_path.as_deref())?;
        let resolved_roots = Self::resolve_scan_roots(&self.roots);
        let roots_fingerprint = Self::roots_fingerprint(&resolved_roots);
        if let Some(marker_path) = &self.migration_marker_path {
            if Self::marker_matches(marker_path, &roots_fingerprint)? {
                stats.skipped_by_marker = true;
                return Ok(stats);
            }
        }

        for root in &resolved_roots {
            let files = Self::collect_db_files_for_root(root)?;
            for file in files {
                stats.scanned_files = stats.scanned_files.saturating_add(1);
                match Self::migrate_file_if_needed(&file)? {
                    FileMigrationOutcome::NotBPlusTree => {}
                    FileMigrationOutcome::AlreadyCurrent | FileMigrationOutcome::Locked => {
                        stats.bplustree_files = stats.bplustree_files.saturating_add(1);
                    }
                    FileMigrationOutcome::Migrated => {
                        stats.bplustree_files = stats.bplustree_files.saturating_add(1);
                        stats.migrated_files = stats.migrated_files.saturating_add(1);
                    }
                }
            }
        }

        if let Some(marker_path) = &self.migration_marker_path {
            Self::write_migration_marker(marker_path, &roots_fingerprint)?;
        }

        Ok(stats)
    }

    fn cleanup_legacy_root_markers(&self, keep_marker_path: Option<&Path>) -> io::Result<()> {
        let marker_name = marker_file_name();
        let marker_name_alt = format!("{MARKER_FILE_GUARD_PREFIX_LEGACY_ALT}{STORAGE_VERSION}");
        let mut visited_roots: HashSet<PathBuf> = HashSet::new();

        for root in &self.roots {
            if !root.exists() || !root.is_dir() {
                continue;
            }
            if !visited_roots.insert(root.clone()) {
                continue;
            }

            for candidate in [root.join(&marker_name), root.join(&marker_name_alt)] {
                if keep_marker_path.is_some_and(|keep| Self::marker_paths_match(keep, candidate.as_path())) {
                    continue;
                }
                match std::fs::remove_file(&candidate) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }

        Ok(())
    }

    fn marker_paths_match(keep_marker_path: &Path, candidate: &Path) -> bool {
        if keep_marker_path == candidate {
            return true;
        }

        if let (Ok(keep_canon), Ok(candidate_canon)) =
            (std::fs::canonicalize(keep_marker_path), std::fs::canonicalize(candidate))
        {
            return keep_canon == candidate_canon;
        }

        let (Some(keep_name), Some(candidate_name)) = (keep_marker_path.file_name(), candidate.file_name()) else {
            return false;
        };
        if keep_name != candidate_name {
            return false;
        }

        let (Some(keep_parent), Some(candidate_parent)) = (keep_marker_path.parent(), candidate.parent()) else {
            return false;
        };

        match (std::fs::canonicalize(keep_parent), std::fs::canonicalize(candidate_parent)) {
            (Ok(keep_parent_canon), Ok(candidate_parent_canon)) => keep_parent_canon == candidate_parent_canon,
            _ => false,
        }
    }

    fn resolve_scan_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut resolved: Vec<PathBuf> = Vec::new();

        // Normalize paths (canonicalize where possible)
        for root in roots {
            if !root.exists() || !root.is_dir() {
                continue;
            }
            // Try to resolve the absolute/real path, fall back to the original path on failure
            let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            resolved.push(canon);
        }

        // Sort paths so parent directories come before child directories
        resolved.sort();
        resolved.dedup();

        // Keep only top-level directories, remove descendants
        let mut final_roots: Vec<PathBuf> = Vec::new();
        for path in resolved {
            // If this path is already covered by a parent in `final_roots`, skip it
            if final_roots.iter().any(|parent| path.starts_with(parent)) {
                continue;
            }
            final_roots.push(path);
        }

        final_roots
    }

    fn roots_fingerprint(roots: &[PathBuf]) -> String {
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = FNV_OFFSET_BASIS;
        for path in roots {
            for byte in path.to_string_lossy().as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(FNV_PRIME);
        }

        format!("{hash:016x}")
    }

    fn collect_db_files_for_root(root: &Path) -> io::Result<Vec<PathBuf>> {
        let mut files: Vec<PathBuf> = Vec::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();

        if visited.insert(root.to_path_buf()) {
            queue.push_back(root.to_path_buf());
        }

        while let Some(dir) = queue.pop_front() {
            for entry_res in std::fs::read_dir(&dir)? {
                let entry = entry_res?;
                let path = entry.path();
                let file_type = entry.file_type()?;

                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    if visited.insert(path.clone()) {
                        queue.push_back(path);
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                if path.extension().and_then(OsStr::to_str).is_some_and(|ext| ext.eq_ignore_ascii_case("db")) {
                    files.push(path);
                }
            }
        }

        Ok(files)
    }

    fn marker_matches(marker_path: &Path, expected_fingerprint: &str) -> io::Result<bool> {
        let Some(stored_fingerprint) = Self::read_migration_marker_fingerprint(marker_path)? else {
            return Ok(false);
        };
        Ok(stored_fingerprint == expected_fingerprint)
    }

    fn read_migration_marker_fingerprint(marker_path: &Path) -> io::Result<Option<String>> {
        let content = match std::fs::read_to_string(marker_path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        let mut marker_version: Option<String> = None;
        let mut roots_fingerprint: Option<String> = None;

        for line in content.lines() {
            if let Some(value) = line.strip_prefix(MARKER_VERSION_KEY).and_then(|rest| rest.strip_prefix('=')) {
                marker_version = Some(value.trim().to_string());
                continue;
            }
            if let Some(value) = line.strip_prefix(MARKER_ROOTS_FINGERPRINT_KEY).and_then(|rest| rest.strip_prefix('='))
            {
                roots_fingerprint = Some(value.trim().to_string());
            }
        }

        let expected_version = STORAGE_VERSION.to_string();
        if marker_version.as_deref() != Some(expected_version.as_str()) {
            return Ok(None);
        }

        Ok(roots_fingerprint.filter(|value| !value.is_empty()))
    }

    fn write_migration_marker(marker_path: &Path, roots_fingerprint: &str) -> io::Result<()> {
        if let Some(parent) = marker_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(marker_path)?;
        file.write_all(
            format!("{MARKER_VERSION_KEY}={STORAGE_VERSION}\n{MARKER_ROOTS_FINGERPRINT_KEY}={roots_fingerprint}\n")
                .as_bytes(),
        )?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    fn migrate_file_if_needed(path: &Path) -> io::Result<FileMigrationOutcome> {
        let mut read_file = OpenOptions::new().read(true).open(path)?;
        let file_len = read_file.metadata()?.len();
        if file_len < 8 {
            return Ok(FileMigrationOutcome::NotBPlusTree);
        }

        let mut header = [0u8; 8];
        read_file.read_exact(&mut header)?;
        if &header[0..4] != MAGIC {
            return Ok(FileMigrationOutcome::NotBPlusTree);
        }

        let version =
            u32::from_le_bytes(header[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?);
        if version == STORAGE_VERSION {
            return Ok(FileMigrationOutcome::AlreadyCurrent);
        }
        if version != LEGACY_STORAGE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported B+Tree storage version {version} in {} (expected {STORAGE_VERSION})",
                    path.display()
                ),
            ));
        }

        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        if let Err(err) = file.try_lock_exclusive() {
            if err.kind() == io::ErrorKind::WouldBlock {
                warn!("Skipping B+Tree migration for locked file {}: {}", path.display(), err);
                return Ok(FileMigrationOutcome::Locked);
            }
            return Err(err);
        }

        // Re-read header after lock to guard against concurrent updates between
        // the read-only probe and write phase.
        file.seek(SeekFrom::Start(0))?;
        let mut locked_header = [0u8; 8];
        file.read_exact(&mut locked_header)?;
        if &locked_header[0..4] != MAGIC {
            return Ok(FileMigrationOutcome::NotBPlusTree);
        }
        let locked_version = u32::from_le_bytes(
            locked_header[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        if locked_version == STORAGE_VERSION {
            return Ok(FileMigrationOutcome::AlreadyCurrent);
        }
        if locked_version != LEGACY_STORAGE_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported B+Tree storage version {locked_version} in {} (expected {STORAGE_VERSION})",
                    path.display()
                ),
            ));
        }

        let _flags_written = Self::normalize_metadata_flags(&mut file)?;
        file.sync_data()?;

        file.seek(SeekFrom::Start(4))?;
        file.write_all(&STORAGE_VERSION.to_le_bytes())?;
        file.flush()?;
        file.sync_data()?;
        Ok(FileMigrationOutcome::Migrated)
    }

    fn normalize_metadata_flags(file: &mut std::fs::File) -> io::Result<bool> {
        file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
        let mut metadata_len_raw = [0u8; 4];
        file.read_exact(&mut metadata_len_raw)?;
        let raw = u32::from_le_bytes(metadata_len_raw);

        let metadata_len = raw & HEADER_METADATA_LEN_MASK;
        if metadata_len > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
        }

        let mut normalized = metadata_len | HEADER_FLAG_HAS_METADATA_FLAGS;
        normalized &= !HEADER_FLAG_HAS_TOMBSTONES;

        if normalized == raw {
            return Ok(false);
        }

        file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
        file.write_all(&normalized.to_le_bytes())?;
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMigrationOutcome {
    NotBPlusTree,
    AlreadyCurrent,
    Locked,
    Migrated,
}

pub fn migrate_bplustree_databases(roots: &[PathBuf]) -> io::Result<BPlusTreeMigrationStats> {
    BPlusTreeStartupMigrator::new(roots.to_vec()).run()
}

pub fn bplustree_migration_marker_path(marker_dir: &Path) -> PathBuf { marker_dir.join(marker_file_name()) }

pub fn migrate_bplustree_databases_with_marker(
    roots: &[PathBuf],
    marker_dir: &Path,
) -> io::Result<BPlusTreeMigrationStats> {
    let marker_path = bplustree_migration_marker_path(marker_dir);
    BPlusTreeStartupMigrator::new_with_marker(roots.to_vec(), marker_path).run()
}

fn marker_file_name() -> String { format!("{MARKER_FILE_GUARD_PREFIX}{STORAGE_VERSION}") }

// ─── User DB schema migration ─────────────────────────────────────────────────
//
// The user database has gone through three serialization schemas (MessagePack,
// positional/sequence encoding via rmp_serde):
//
//   V1 (Deprecated) – original format, 13 fields, no epg_request_timeshift
//   V2              – 14 fields, added epg_request_timeshift
//   V3 (current)    – 15 fields, added priority
//
// On first startup after an upgrade the file is still in V1 or V2 format.
// `migrate_user_db_schema` detects this, converts every record in-place, and
// writes a merge-guard marker so that config-driven user merges cannot
// overwrite the freshly migrated data.

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredApiUserV1 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredApiUserV2 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
}

// V3 mirror — same layout as user_repository::StoredProxyUserCredentials.
// Defined here so the migration has no dependency on user_repository internals.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredApiUserV3 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
}

impl StoredApiUserV3 {
    fn from_v2(v2: &StoredApiUserV2) -> Self {
        Self {
            target: v2.target.clone(),
            username: v2.username.clone(),
            password: v2.password.clone(),
            token: v2.token.clone(),
            proxy: v2.proxy,
            server: v2.server.clone(),
            epg_timeshift: v2.epg_timeshift.clone(),
            epg_request_timeshift: v2.epg_request_timeshift.clone(),
            created_at: v2.created_at,
            exp_date: v2.exp_date,
            max_connections: v2.max_connections,
            status: v2.status,
            ui_enabled: v2.ui_enabled,
            comment: v2.comment.clone(),
            priority: None,
        }
    }

    fn from_v1(v1: &StoredApiUserV1) -> Self {
        Self {
            target: v1.target.clone(),
            username: v1.username.clone(),
            password: v1.password.clone(),
            token: v1.token.clone(),
            proxy: v1.proxy,
            server: v1.server.clone(),
            epg_timeshift: v1.epg_timeshift.clone(),
            epg_request_timeshift: None,
            created_at: v1.created_at,
            exp_date: v1.exp_date,
            max_connections: v1.max_connections,
            status: v1.status,
            ui_enabled: v1.ui_enabled,
            comment: v1.comment.clone(),
            priority: None,
        }
    }
}

fn create_user_db_merge_guard(merge_guard_path: &Path) -> io::Result<()> {
    if !merge_guard_path.exists() {
        std::fs::write(merge_guard_path, b"")?;
    }
    Ok(())
}

pub(crate) fn user_db_merge_guard_path(config_dir: &Path) -> PathBuf {
    config_dir.join(MARKER_FILE_API_USER_GUARD)
}

/// Migrates the user database file from V1 or V2 schema to V3 (current) in
/// place and creates a merge-guard file so config-driven merges are skipped
/// until the operator explicitly removes it.
///
/// Returns `true` when a migration was performed, `false` when the file was
/// already in V3 format or did not exist.
fn migrate_user_db_schema(db_path: &Path, merge_guard_path: &Path) -> io::Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }

    // Try legacy schemas first to preserve explicit upgrade behavior:
    // V1 -> V3, then V2 -> V3.
    if let Ok(tree) = BPlusTree::<String, StoredApiUserV1>::load(db_path) {
        let mut v3_tree: BPlusTree<String, StoredApiUserV3> = BPlusTree::new();
        for (key, v1) in &tree {
            v3_tree.insert(key.clone(), StoredApiUserV3::from_v1(v1));
        }
        v3_tree.store(db_path)?;
        create_user_db_merge_guard(merge_guard_path)?;
        return Ok(true);
    }

    if let Ok(tree) = BPlusTree::<String, StoredApiUserV2>::load(db_path) {
        let mut v3_tree: BPlusTree<String, StoredApiUserV3> = BPlusTree::new();
        for (key, v2) in &tree {
            v3_tree.insert(key.clone(), StoredApiUserV3::from_v2(v2));
        }
        v3_tree.store(db_path)?;
        create_user_db_merge_guard(merge_guard_path)?;
        return Ok(true);
    }

    // If legacy decoding failed, accept that DB may already be V3.
    // Do not rewrite or persist anything in that case.
    if BPlusTree::<String, StoredApiUserV3>::load(db_path).is_ok() {
        return Ok(false);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("User DB at '{}' exists but could not be read as V1, V2, or V3 format", db_path.display()),
    ))
}

// ─── Combined startup migration ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub struct AllStartupMigrationStats {
    pub bplustree: BPlusTreeMigrationStats,
    pub user_db_migrated: bool,
}

/// Runs all startup migrations in sequence:
/// 1. B+Tree storage-format migration (V1 → current binary format)
/// 2. User DB schema migration (V1/V2 → V3 `MessagePack` layout)
///
/// `config_dir` is the directory that contains `api_user.db` and the merge-guard
/// marker. `storage_dir` is used for the B+Tree migration marker.
fn run_all_startup_migrations(
    roots: &[PathBuf],
    storage_dir: &Path,
    config_dir: &Path,
) -> io::Result<AllStartupMigrationStats> {
    let marker_path = bplustree_migration_marker_path(storage_dir);
    let bplustree = BPlusTreeStartupMigrator::new_with_marker(roots.to_vec(), marker_path).run()?;

    let user_db_path = config_dir.join(storage_const::API_USER_DB_FILE);
    let merge_guard_path = user_db_merge_guard_path(config_dir);
    let user_db_migrated = migrate_user_db_schema(&user_db_path, &merge_guard_path)?;

    Ok(AllStartupMigrationStats { bplustree, user_db_migrated })
}


pub fn run_startup_migrations(config_paths: &ConfigPaths) {
    let config_file_path = Path::new(config_paths.config_file_path.as_str());
    if !config_file_path.exists() {
        return;
    }

    let config_dir = PathBuf::from(&config_paths.config_path);
    let storage_dir = if config_paths.storage_path.trim().is_empty() {
        config_dir.clone()
    } else {
        PathBuf::from(&config_paths.storage_path)
    };
    let mut roots: Vec<PathBuf> = vec![config_dir.clone()];
    if storage_dir != config_dir {
        roots.push(storage_dir.clone());
    }

    match run_all_startup_migrations(&roots, &storage_dir, &config_dir) {
        Ok(stats) => {
            if stats.bplustree.skipped_by_marker {
                info!("B+Tree startup migration skipped (marker already present)");
            } else if stats.bplustree.migrated_files > 0 {
                info!(
                    "B+Tree startup migration completed: migrated {} file(s) ({} B+Tree files checked, {} .db files scanned)",
                    stats.bplustree.migrated_files,
                    stats.bplustree.bplustree_files,
                    stats.bplustree.scanned_files
                );
            }
            if stats.user_db_migrated {
                info!("User DB schema migrated to V3");
            }
        }
        Err(err) => {
            crate::utils::exit!("Startup migration failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    fn test_roots_fingerprint(roots: &[PathBuf]) -> String {
        let resolved = BPlusTreeStartupMigrator::resolve_scan_roots(roots);
        BPlusTreeStartupMigrator::roots_fingerprint(&resolved)
    }

    #[test]
    fn startup_migrator_upgrades_legacy_bplustree_files() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("legacy.db");

        let mut file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path)?;
        let mut header = [0u8; 4096];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&LEGACY_STORAGE_VERSION.to_le_bytes());
        file.write_all(&header)?;
        file.flush()?;
        drop(file);

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.scanned_files, 1);
        assert_eq!(stats.bplustree_files, 1);
        assert_eq!(stats.migrated_files, 1);

        let mut check = OpenOptions::new().read(true).open(&db_path)?;
        let mut version_bytes = [0u8; 20];
        check.read_exact(&mut version_bytes)?;
        let version = u32::from_le_bytes(
            version_bytes[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        assert_eq!(version, STORAGE_VERSION);
        let metadata_len_raw = u32::from_le_bytes(
            version_bytes[16..20].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        assert_ne!(metadata_len_raw & HEADER_FLAG_HAS_METADATA_FLAGS, 0);
        assert_eq!(metadata_len_raw & HEADER_FLAG_HAS_TOMBSTONES, 0);

        Ok(())
    }

    #[test]
    fn startup_migrator_skips_non_bplustree_db_files() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("other.db");
        let mut file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path)?;
        file.write_all(b"NOT_BPLUSTREE_FILE")?;
        file.flush()?;
        drop(file);

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.scanned_files, 1);
        assert_eq!(stats.bplustree_files, 0);
        assert_eq!(stats.migrated_files, 0);

        Ok(())
    }

    #[test]
    fn startup_migrator_writes_marker_after_success() -> io::Result<()> {
        let temp = tempdir()?;
        let temp_other = tempdir()?;
        let db_path = temp.path().join("legacy.db");
        let db_path_other = temp_other.path().join("legacy_other.db");
        let mut file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path)?;
        let mut header = [0u8; 4096];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&LEGACY_STORAGE_VERSION.to_le_bytes());
        file.write_all(&header)?;
        file.flush()?;
        drop(file);

        let mut file_other =
            OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path_other)?;
        let mut header_other = [0u8; 4096];
        header_other[0..4].copy_from_slice(MAGIC);
        header_other[4..8].copy_from_slice(&LEGACY_STORAGE_VERSION.to_le_bytes());
        file_other.write_all(&header_other)?;
        file_other.flush()?;
        drop(file_other);

        let stats = migrate_bplustree_databases_with_marker(
            &[temp.path().to_path_buf(), temp_other.path().to_path_buf()],
            temp.path(),
        )?;
        assert_eq!(stats.migrated_files, 2);
        assert!(!stats.skipped_by_marker);

        let marker = bplustree_migration_marker_path(temp.path());
        assert!(marker.exists());
        assert!(marker.is_file());
        let marker_other = bplustree_migration_marker_path(temp_other.path());
        assert!(!marker_other.exists());

        Ok(())
    }

    #[test]
    fn startup_migrator_skips_when_marker_exists() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("legacy.db");
        let mut file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path)?;
        let mut header = [0u8; 4096];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&LEGACY_STORAGE_VERSION.to_le_bytes());
        file.write_all(&header)?;
        file.flush()?;
        drop(file);

        let marker = bplustree_migration_marker_path(temp.path());
        let roots = [temp.path().to_path_buf()];
        let fingerprint = test_roots_fingerprint(&roots);
        BPlusTreeStartupMigrator::write_migration_marker(&marker, &fingerprint)?;

        let stats = migrate_bplustree_databases_with_marker(&roots, temp.path())?;
        assert_eq!(stats.scanned_files, 0);
        assert_eq!(stats.bplustree_files, 0);
        assert_eq!(stats.migrated_files, 0);
        assert!(stats.skipped_by_marker);

        let mut check = OpenOptions::new().read(true).open(&db_path)?;
        let mut version_bytes = [0u8; 8];
        check.read_exact(&mut version_bytes)?;
        let version = u32::from_le_bytes(
            version_bytes[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        assert_eq!(version, LEGACY_STORAGE_VERSION);

        Ok(())
    }

    #[test]
    fn startup_migrator_removes_legacy_per_root_markers() -> io::Result<()> {
        let temp_root = tempdir()?;
        let temp_other = tempdir()?;
        let marker_dir = temp_root.path();
        let global_marker = bplustree_migration_marker_path(marker_dir);
        let roots = [marker_dir.to_path_buf(), temp_other.path().to_path_buf()];
        let fingerprint = test_roots_fingerprint(&roots);
        BPlusTreeStartupMigrator::write_migration_marker(&global_marker, &fingerprint)?;

        let legacy_per_root_marker = temp_other.path().join(format!("{MARKER_FILE_GUARD_PREFIX}{STORAGE_VERSION}"));
        BPlusTreeStartupMigrator::write_migration_marker(&legacy_per_root_marker, "legacy")?;
        assert!(legacy_per_root_marker.exists());

        let stats = migrate_bplustree_databases_with_marker(&roots, marker_dir)?;
        assert!(stats.skipped_by_marker);
        assert!(!legacy_per_root_marker.exists());
        assert!(global_marker.exists());

        Ok(())
    }

    #[test]
    fn startup_migrator_does_not_skip_when_marker_fingerprint_differs() -> io::Result<()> {
        let temp_a = tempdir()?;
        let temp_b = tempdir()?;
        let db_path = temp_a.path().join("legacy.db");

        let mut file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&db_path)?;
        let mut header = [0u8; 4096];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&LEGACY_STORAGE_VERSION.to_le_bytes());
        file.write_all(&header)?;
        file.flush()?;
        drop(file);

        let marker = bplustree_migration_marker_path(temp_a.path());
        let old_roots = [temp_a.path().to_path_buf()];
        let old_fingerprint = test_roots_fingerprint(&old_roots);
        BPlusTreeStartupMigrator::write_migration_marker(&marker, &old_fingerprint)?;

        let current_roots = [temp_a.path().to_path_buf(), temp_b.path().to_path_buf()];
        let stats = migrate_bplustree_databases_with_marker(&current_roots, temp_a.path())?;
        assert!(!stats.skipped_by_marker);
        assert_eq!(stats.migrated_files, 1);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v2_to_v3_creates_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v2_tree: BPlusTree<String, StoredApiUserV2> = BPlusTree::new();
        v2_tree.insert(
            "alice".to_string(),
            StoredApiUserV2 {
                target: "channels".to_string(),
                username: "alice".to_string(),
                password: "secret".to_string(),
                token: Some("token".to_string()),
                proxy: ProxyType::Reverse(None),
                server: Some("srv".to_string()),
                epg_timeshift: Some("1".to_string()),
                epg_request_timeshift: Some("2".to_string()),
                created_at: Some(1),
                exp_date: Some(2),
                max_connections: Some(3),
                status: Some(ProxyUserStatus::Active),
                ui_enabled: true,
                comment: Some("note".to_string()),
            },
        );
        let _ = v2_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(migrated);
        assert!(merge_guard_path.exists());

        let v3_tree = BPlusTree::<String, StoredApiUserV3>::load(&db_path)?;
        let user = v3_tree
            .query(&"alice".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "alice missing after migration"))?;
        assert_eq!(user.username, "alice");
        assert_eq!(user.epg_request_timeshift.as_deref(), Some("2"));
        assert_eq!(user.priority, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_v3_is_detected_without_writing_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v3_tree: BPlusTree<String, StoredApiUserV3> = BPlusTree::new();
        v3_tree.insert(
            "bob".to_string(),
            StoredApiUserV3 {
                target: "channels".to_string(),
                username: "bob".to_string(),
                password: "secret".to_string(),
                token: None,
                proxy: ProxyType::Reverse(None),
                server: None,
                epg_timeshift: None,
                epg_request_timeshift: None,
                created_at: None,
                exp_date: None,
                max_connections: Some(1),
                status: Some(ProxyUserStatus::Active),
                ui_enabled: true,
                comment: None,
                priority: Some(5),
            },
        );
        let _ = v3_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(!migrated);
        assert!(!merge_guard_path.exists());

        Ok(())
    }
}
