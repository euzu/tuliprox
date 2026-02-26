use super::bplustree::{MAGIC, STORAGE_VERSION};
use fs2::FileExt as _;
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
const MARKER_FILE_PREFIX: &str = ".db_mergeto";
const LEGACY_MARKER_FILE_PREFIX_ALT: &str = ".db_mergedto";
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
pub struct BPlusTreeStartupMigrator {
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
                    FileMigrationOutcome::AlreadyCurrent => {
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

    fn resolve_scan_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut resolved: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for root in roots {
            if !root.exists() || !root.is_dir() {
                continue;
            }
            let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            let key = resolved_root.to_string_lossy().into_owned();
            if seen.insert(key) {
                resolved.push(resolved_root);
            }
        }

        resolved
    }

    fn roots_fingerprint(roots: &[PathBuf]) -> String {
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut canonical_entries: Vec<String> = roots.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        canonical_entries.sort_unstable();
        canonical_entries.dedup();

        let mut hash = FNV_OFFSET_BASIS;
        for entry in &canonical_entries {
            for byte in entry.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(FNV_PRIME);
        }

        format!("{hash:016x}")
    }

    fn cleanup_legacy_root_markers(&self, keep_marker_path: Option<&Path>) -> io::Result<()> {
        let marker_name = marker_file_name();
        let marker_name_alt = marker_file_name_alt();
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
        file.try_lock_exclusive()?;

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

        let has_flags = (raw & HEADER_FLAG_HAS_METADATA_FLAGS) != 0;
        if has_flags {
            return Ok(false);
        }

        let mut normalized = metadata_len | HEADER_FLAG_HAS_METADATA_FLAGS;
        normalized &= !HEADER_FLAG_HAS_TOMBSTONES;

        file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
        file.write_all(&normalized.to_le_bytes())?;
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileMigrationOutcome {
    NotBPlusTree,
    AlreadyCurrent,
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

fn marker_file_name() -> String { format!("{MARKER_FILE_PREFIX}{STORAGE_VERSION}") }

fn marker_file_name_alt() -> String { format!("{LEGACY_MARKER_FILE_PREFIX_ALT}{STORAGE_VERSION}") }

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

        let legacy_per_root_marker = temp_other.path().join(format!("{MARKER_FILE_PREFIX}{STORAGE_VERSION}"));
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
}
