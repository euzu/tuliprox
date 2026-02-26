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
        if self.migration_marker_path.as_ref().is_some_and(|path| path.exists()) {
            stats.skipped_by_marker = true;
            return Ok(stats);
        }

        let mut visited_roots: HashSet<PathBuf> = HashSet::new();

        for root in &self.roots {
            if !root.exists() || !root.is_dir() {
                continue;
            }
            if !visited_roots.insert(root.clone()) {
                continue;
            }

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
            Self::write_migration_marker(marker_path)?;
        }

        Ok(stats)
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
                if keep_marker_path.is_some_and(|keep| keep == candidate.as_path()) {
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

    fn write_migration_marker(marker_path: &Path) -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(marker_path)?;
        file.write_all(format!("migrated_to={STORAGE_VERSION}\n").as_bytes())?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    fn migrate_file_if_needed(path: &Path) -> io::Result<FileMigrationOutcome> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock_exclusive()?;

        let file_len = file.metadata()?.len();
        if file_len < 8 {
            return Ok(FileMigrationOutcome::NotBPlusTree);
        }

        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        if &header[0..4] != MAGIC {
            return Ok(FileMigrationOutcome::NotBPlusTree);
        }

        let version =
            u32::from_le_bytes(header[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?);
        if version == STORAGE_VERSION {
            return Ok(FileMigrationOutcome::AlreadyCurrent);
        }
        if version == LEGACY_STORAGE_VERSION {
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&STORAGE_VERSION.to_le_bytes())?;
            let _flags_written = Self::normalize_metadata_flags(&mut file)?;
            file.flush()?;
            file.sync_data()?;
            return Ok(FileMigrationOutcome::Migrated);
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported B+Tree storage version {version} in {} (expected {STORAGE_VERSION})", path.display()),
        ))
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
        BPlusTreeStartupMigrator::write_migration_marker(&marker)?;

        let stats = migrate_bplustree_databases_with_marker(&[temp.path().to_path_buf()], temp.path())?;
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
        BPlusTreeStartupMigrator::write_migration_marker(&global_marker)?;

        let legacy_per_root_marker = temp_other.path().join(format!("{MARKER_FILE_PREFIX}{STORAGE_VERSION}"));
        BPlusTreeStartupMigrator::write_migration_marker(&legacy_per_root_marker)?;
        assert!(legacy_per_root_marker.exists());

        let stats = migrate_bplustree_databases_with_marker(
            &[marker_dir.to_path_buf(), temp_other.path().to_path_buf()],
            marker_dir,
        )?;
        assert!(stats.skipped_by_marker);
        assert!(!legacy_per_root_marker.exists());
        assert!(global_marker.exists());

        Ok(())
    }
}
