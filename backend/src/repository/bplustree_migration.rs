use super::bplustree::{MAGIC, STORAGE_VERSION};
use fs2::FileExt as _;
use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const LEGACY_STORAGE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default)]
pub struct BPlusTreeMigrationStats {
    pub scanned_files: usize,
    pub bplustree_files: usize,
    pub migrated_files: usize,
}

#[derive(Debug)]
pub struct BPlusTreeStartupMigrator {
    roots: Vec<PathBuf>,
}

impl BPlusTreeStartupMigrator {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn run(&self) -> io::Result<BPlusTreeMigrationStats> {
        let files = self.collect_db_files()?;
        let mut stats = BPlusTreeMigrationStats::default();

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

        Ok(stats)
    }

    fn collect_db_files(&self) -> io::Result<Vec<PathBuf>> {
        let mut files: Vec<PathBuf> = Vec::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();

        for root in &self.roots {
            if !root.exists() || !root.is_dir() {
                continue;
            }
            if visited.insert(root.clone()) {
                queue.push_back(root.clone());
            }
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
                if path
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
                {
                    files.push(path);
                }
            }
        }

        Ok(files)
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

        let version = u32::from_le_bytes(
            header[4..8]
                .try_into()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        if version == STORAGE_VERSION {
            return Ok(FileMigrationOutcome::AlreadyCurrent);
        }
        if version == LEGACY_STORAGE_VERSION {
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&STORAGE_VERSION.to_le_bytes())?;
            file.flush()?;
            file.sync_data()?;
            return Ok(FileMigrationOutcome::Migrated);
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Unsupported B+Tree storage version {version} in {} (expected {STORAGE_VERSION})",
                path.display()
            ),
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn startup_migrator_upgrades_legacy_bplustree_files() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("legacy.db");

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&db_path)?;
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
        let mut version_bytes = [0u8; 8];
        check.read_exact(&mut version_bytes)?;
        let version = u32::from_le_bytes(
            version_bytes[4..8]
                .try_into()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        assert_eq!(version, STORAGE_VERSION);

        Ok(())
    }

    #[test]
    fn startup_migrator_skips_non_bplustree_db_files() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("other.db");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&db_path)?;
        file.write_all(b"NOT_BPLUSTREE_FILE")?;
        file.flush()?;
        drop(file);

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.scanned_files, 1);
        assert_eq!(stats.bplustree_files, 0);
        assert_eq!(stats.migrated_files, 0);

        Ok(())
    }
}
