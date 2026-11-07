//! Startup migration helpers for legacy repository files.
//!
//! The current B+Tree implementation writes storage format v3. This migrator
//! recognizes the repository's closed set of concrete key/value schemas and
//! rewrites legacy v1/v2 databases into verified v3 files before publishing
//! them atomically.

use super::{
    bplustree::{typed_migration, BPlusTree, BPlusTreeQuery, MAGIC, STORAGE_VERSION},
    MetadataRetryDbKey, MetadataRetryDbValue,
};
#[cfg(test)]
use crate::api_user_recovery::{StoredApiUserV2, StoredApiUserV3, StoredApiUserV4, StoredApiUserV5, StoredApiUserV6};
use crate::{
    api_user_recovery::{decode_user_record, ApiUserRecovery, ApiUserRepository, StoredApiUserV7},
    qos_snapshot_repository::{QosAggregationCheckpoint, QosSnapshotRecord},
    storage_const,
    target_id_mapping::VirtualIdRecord,
};
use log::{info, trace, warn};
use serde_json::Value;
// Only the migration tests build the legacy records now that the layouts
// themselves live in `api_user_recovery`.
#[cfg(test)]
use shared::model::{ClusterFlags, NetworkAccessDto, ProxyType, ProxyUserStatus};
use shared::model::{ConfigPaths, EpgChannel, M3uPlaylistItem, UUIDType, XtreamPlaylistItem};
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_btree::{publish_staged_database, BPlusTreeMetadata, BPlusTreeStagingArtifacts};

const LEGACY_STORAGE_VERSION: u32 = 1;
const MARKER_FILE_GUARD_PREFIX: &str = ".db_mergeto_v";
const MARKER_FILE_GUARD_PREFIX_LEGACY_ALT: &str = ".db_mergedto";
const MARKER_FILE_API_USER_GUARD: &str = ".userdb_mergeto_v7";
const MARKER_VERSION_KEY: &str = "migrated_to";
const MARKER_ROOTS_FINGERPRINT_KEY: &str = "roots_fingerprint";

#[derive(Debug, Clone, Copy, Default)]
pub struct BPlusTreeMigrationStats {
    pub scanned_files: usize,
    pub bplustree_files: usize,
    pub migrated_files: usize,
    pub marker_was_current: bool,
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
        let marker_path = self.migration_marker_path.as_deref().filter(|path| !Self::is_repair_archive_location(path));
        self.cleanup_legacy_root_markers(marker_path)?;
        let resolved_roots = Self::resolve_scan_roots(&self.roots);
        let roots_fingerprint = Self::roots_fingerprint(&resolved_roots);
        if let Some(marker_path) = marker_path {
            stats.marker_was_current = Self::marker_matches(marker_path, &roots_fingerprint)?;
        }
        for root in &resolved_roots {
            let files = Self::collect_db_files_for_root(root)?;
            for file in files {
                stats.scanned_files = stats.scanned_files.saturating_add(1);
                match Self::migrate_file_if_needed(&file, &resolved_roots)? {
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

        if let Some(marker_path) = marker_path {
            Self::write_migration_marker(marker_path, &roots_fingerprint)?;
        }

        Ok(stats)
    }

    fn cleanup_legacy_root_markers(&self, keep_marker_path: Option<&Path>) -> io::Result<()> {
        let marker_name = marker_file_name();
        let marker_name_alt = format!("{MARKER_FILE_GUARD_PREFIX_LEGACY_ALT}{STORAGE_VERSION}");
        let mut visited_roots: HashSet<PathBuf> = HashSet::new();

        for root in &self.roots {
            let Some(root) = Self::resolve_scan_root(root) else { continue };
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
            if let Some(root) = Self::resolve_scan_root(root) {
                resolved.push(root);
            }
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

    /// Reports whether any path component names a user DB repair archive.
    fn is_repair_archive_path(path: &Path) -> bool {
        path.components().any(|component| {
            component
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(storage_const::API_USER_REPAIR_BACKUP_PREFIX.as_bytes())
        })
    }

    /// Canonicalizes a scan root, skipping missing directories and repair archives before and after resolution.
    fn resolve_scan_root(root: &Path) -> Option<PathBuf> {
        if Self::is_repair_archive_path(root) || !root.is_dir() {
            return None;
        }
        let resolved = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        (!Self::is_repair_archive_path(&resolved)).then_some(resolved)
    }

    /// Like `is_repair_archive_path`, but also resolves the nearest existing ancestor so symlinks and not-yet-created paths are caught.
    fn is_repair_archive_location(path: &Path) -> bool {
        // Marker paths may not exist yet, or may reach an archive through a
        // symlink. Resolve the nearest existing ancestor before any marker I/O.
        Self::is_repair_archive_path(path)
            || path
                .ancestors()
                .find_map(|ancestor| std::fs::canonicalize(ancestor).ok())
                .is_some_and(|resolved| Self::is_repair_archive_path(&resolved))
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
                    if Self::is_repair_archive_path(&path) {
                        continue;
                    }
                    if visited.insert(path.clone()) {
                        queue.push_back(path);
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                if Self::is_abandoned_v3_temporary(&path) {
                    std::fs::remove_file(&path)?;
                    continue;
                }
                if path.extension().and_then(OsStr::to_str).is_some_and(|ext| ext.eq_ignore_ascii_case("db")) {
                    files.push(path);
                }
            }
        }

        Ok(files)
    }

    fn is_abandoned_v3_temporary(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            return false;
        };
        let Some(prefix) = name.strip_suffix(".v3.tmp") else {
            return false;
        };
        let Some((database_name, transaction)) = prefix.rsplit_once('.') else {
            return false;
        };
        Path::new(database_name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("db") || extension.eq_ignore_ascii_case("idx"))
            && uuid::Uuid::parse_str(transaction).is_ok()
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

    fn migrate_file_if_needed(path: &Path, roots: &[PathBuf]) -> io::Result<FileMigrationOutcome> {
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
            let _ = BPlusTreeQuery::<u8, u8>::try_new(path)?;
            return Ok(FileMigrationOutcome::AlreadyCurrent);
        }
        if version != LEGACY_STORAGE_VERSION && version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported B+Tree storage version {version} in {} (expected {STORAGE_VERSION})",
                    path.display()
                ),
            ));
        }

        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        if let Err(err) = file.try_lock() {
            if matches!(err, std::fs::TryLockError::WouldBlock) {
                warn!("Skipping B+Tree migration for locked file {}: {}", path.display(), err);
                return Ok(FileMigrationOutcome::Locked);
            }
            return Err(err.into());
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
        if locked_version != LEGACY_STORAGE_VERSION && locked_version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported B+Tree storage version {locked_version} in {} (expected {STORAGE_VERSION})",
                    path.display()
                ),
            ));
        }

        Self::migrate_typed_path(path, roots, locked_version).map_err(|error| {
            io::Error::new(error.kind(), format!("Failed to migrate B+Tree {}: {error}", path.display()))
        })?;
        Ok(FileMigrationOutcome::Migrated)
    }

    fn migrate_typed_path(path: &Path, roots: &[PathBuf], version: u32) -> io::Result<()> {
        use super::typed_migration::{migrate_v2_typed, migrate_v2_typed_with_index};

        let kind = Self::migration_kind(path, roots).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported typed B+Tree migration for {} (storage version {version})", path.display()),
            )
        })?;
        match kind {
            TypedMigration::MetadataRetry => migrate_v2_typed::<MetadataRetryDbKey, MetadataRetryDbValue>(path)?,
            TypedMigration::QosSnapshot => migrate_v2_typed::<String, QosSnapshotRecord>(path)?,
            TypedMigration::QosCheckpoint => migrate_v2_typed::<u8, QosAggregationCheckpoint>(path)?,
            TypedMigration::GeoIp => migrate_v2_typed::<u32, (u32, String)>(path)?,
            TypedMigration::IdMapping => migrate_v2_typed::<u32, VirtualIdRecord>(path)?,
            TypedMigration::UuidMapping => migrate_v2_typed::<UUIDType, u32>(path)?,
            TypedMigration::TargetM3u => {
                migrate_v2_typed_with_index::<u32, M3uPlaylistItem, u32, _>(path, |item| item.source_ordinal)?
            }
            TypedMigration::InputM3u => migrate_v2_typed::<Arc<str>, M3uPlaylistItem>(path)?,
            TypedMigration::Library => migrate_v2_typed::<UUIDType, XtreamPlaylistItem>(path)?,
            TypedMigration::TargetXtream => {
                migrate_v2_typed_with_index::<u32, XtreamPlaylistItem, u32, _>(path, |item| item.source_ordinal)?
            }
            TypedMigration::InputXtream => migrate_v2_typed::<u32, XtreamPlaylistItem>(path)?,
            TypedMigration::Epg => migrate_v2_typed::<Arc<str>, EpgChannel>(path)?,
        };
        Ok(())
    }

    fn migration_kind(path: &Path, roots: &[PathBuf]) -> Option<TypedMigration> {
        if let Some(relative) = roots.iter().find_map(|root| path.strip_prefix(root).ok()) {
            let components = relative.iter().filter_map(OsStr::to_str).collect::<Vec<_>>();
            match components.as_slice() {
                ["metadata_retry_state.db"] => return Some(TypedMigration::MetadataRetry),
                ["qos_snapshot.db"] => return Some(TypedMigration::QosSnapshot),
                ["qos_snapshot_meta.db"] => return Some(TypedMigration::QosCheckpoint),
                ["geoip.db"] => return Some(TypedMigration::GeoIp),
                [directory, "metadata_retry_state.db"] if directory.starts_with("input_") => {
                    return Some(TypedMigration::MetadataRetry);
                }
                [directory, name] if directory.starts_with("input_") && name.starts_with("m3u_") => {
                    return Some(TypedMigration::InputM3u);
                }
                [directory, name]
                    if directory.starts_with("input_")
                        && (name.starts_with("lib_") || name.starts_with("media_server_")) =>
                {
                    return Some(TypedMigration::Library);
                }
                [directory, name]
                    if directory.starts_with("input_") && matches!(*name, "live.db" | "video.db" | "series.db") =>
                {
                    return Some(TypedMigration::InputXtream);
                }
                [_, "id_mapping.db"] => return Some(TypedMigration::IdMapping),
                [_, "id_mapping.uuid.db"] => return Some(TypedMigration::UuidMapping),
                [_, "m3u", "m3u.db"] => return Some(TypedMigration::TargetM3u),
                [_, "m3u" | "xtream", "epg.db"] => return Some(TypedMigration::Epg),
                [_, "xtream", "live.db" | "video.db" | "series.db"] => return Some(TypedMigration::TargetXtream),
                _ => {}
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedMigration {
    MetadataRetry,
    QosSnapshot,
    QosCheckpoint,
    GeoIp,
    IdMapping,
    UuidMapping,
    TargetM3u,
    InputM3u,
    Library,
    TargetXtream,
    InputXtream,
    Epg,
}

impl TypedMigration {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().replace('_', "-").as_str() {
            "series" | "xtream" | "input-xtream" | "live" | "video" => Some(Self::InputXtream),
            "target-xtream" => Some(Self::TargetXtream),
            "m3u" | "input-m3u" => Some(Self::InputM3u),
            "target-m3u" => Some(Self::TargetM3u),
            "epg" => Some(Self::Epg),
            "id-mapping" => Some(Self::IdMapping),
            "uuid-mapping" => Some(Self::UuidMapping),
            "metadata-retry" => Some(Self::MetadataRetry),
            "qos" | "qos-snapshot" => Some(Self::QosSnapshot),
            "qos-meta" | "qos-checkpoint" => Some(Self::QosCheckpoint),
            "geoip" => Some(Self::GeoIp),
            "library" => Some(Self::Library),
            _ => None,
        }
    }

    #[must_use]
    pub fn infer_from_path(path: &Path) -> Option<Self> {
        let filename = path.file_name().and_then(OsStr::to_str)?;
        let parent_name = path.parent().and_then(|p| p.file_name()).and_then(OsStr::to_str).unwrap_or("");
        let path_str = path.to_str().unwrap_or("");

        match filename {
            "metadata_retry_state.db" => Some(Self::MetadataRetry),
            "qos_snapshot.db" => Some(Self::QosSnapshot),
            "qos_snapshot_meta.db" => Some(Self::QosCheckpoint),
            "geoip.db" => Some(Self::GeoIp),
            "id_mapping.uuid.db" => Some(Self::UuidMapping),
            "id_mapping.db" => Some(Self::IdMapping),
            "epg.db" => Some(Self::Epg),
            "m3u.db" => Some(Self::TargetM3u),
            name if name.starts_with("m3u_") => Some(Self::InputM3u),
            name if name.starts_with("lib_") || name.starts_with("media_server_") => Some(Self::Library),
            "series.db" | "live.db" | "video.db" => {
                if parent_name == "xtream" || path_str.contains("/target/xtream") {
                    Some(Self::TargetXtream)
                } else {
                    Some(Self::InputXtream)
                }
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::MetadataRetry => "metadata_retry",
            Self::QosSnapshot => "qos_snapshot",
            Self::QosCheckpoint => "qos_checkpoint",
            Self::GeoIp => "geoip",
            Self::IdMapping => "id_mapping",
            Self::UuidMapping => "uuid_mapping",
            Self::TargetM3u => "target_m3u",
            Self::InputM3u => "input_m3u",
            Self::Library => "library",
            Self::TargetXtream => "target_xtream",
            Self::InputXtream => "input_xtream",
            Self::Epg => "epg",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SingleMigrationReport {
    pub path: PathBuf,
    pub original_version: u32,
    pub target_version: u32,
    pub backup_path: Option<PathBuf>,
    pub live_entries: usize,
    pub corrupted_entries: usize,
    pub database_id: [u8; 16],
    pub generation: u64,
    pub already_current: bool,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct SingleInspectionReport {
    pub path: PathBuf,
    pub version: Option<u32>,
    pub file_size: u64,
    pub kind: String,
    pub live_entries: usize,
    pub corrupted_entries: usize,
    pub database_id: Option<[u8; 16]>,
    pub generation: Option<u64>,
}

fn lock_database_exclusively(path: &Path) -> io::Result<std::fs::File> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    if let Err(error) = file.try_lock() {
        if matches!(error, std::fs::TryLockError::WouldBlock) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("Database is locked: {}", path.display())));
        }
        return Err(error.into());
    }
    Ok(file)
}

pub fn migrate_single_database(
    path: &Path,
    explicit_kind: Option<&str>,
    backup: bool,
) -> io::Result<SingleMigrationReport> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !canonical.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database file does not exist: {}", path.display()),
        ));
    }

    let _lock_file = lock_database_exclusively(&canonical)?;

    let version = typed_migration::storage_version(&canonical)?;
    let Some(current_version) = version else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a valid B+Tree database (magic header 'BTRE' missing)", path.display()),
        ));
    };

    if current_version == STORAGE_VERSION {
        return Ok(SingleMigrationReport {
            path: canonical,
            original_version: current_version,
            target_version: STORAGE_VERSION,
            backup_path: None,
            live_entries: 0,
            corrupted_entries: 0,
            database_id: [0; 16],
            generation: 0,
            already_current: true,
            kind: "already_v3".to_string(),
        });
    }

    if current_version != LEGACY_STORAGE_VERSION && current_version != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported storage version {current_version} in {} (expected 1 or 2)", path.display()),
        ));
    }

    let kind = explicit_kind
        .and_then(TypedMigration::parse)
        .or_else(|| TypedMigration::infer_from_path(&canonical))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Could not determine database type for {}. Please specify --db-type (e.g. series, xtream, m3u, epg)",
                    path.display()
                ),
            )
        })?;

    let backup_path = if backup {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let backup_file = canonical.with_file_name(format!(
            "{}.bak.{timestamp}",
            canonical.file_name().and_then(OsStr::to_str).unwrap_or("database")
        ));
        std::fs::copy(&canonical, &backup_file)?;
        info!("Created backup of {} at {}", canonical.display(), backup_file.display());
        Some(backup_file)
    } else {
        None
    };

    let validation = match kind {
        TypedMigration::MetadataRetry => {
            typed_migration::migrate_v2_typed::<MetadataRetryDbKey, MetadataRetryDbValue>(&canonical)?
        }
        TypedMigration::QosSnapshot => typed_migration::migrate_v2_typed::<String, QosSnapshotRecord>(&canonical)?,
        TypedMigration::QosCheckpoint => typed_migration::migrate_v2_typed::<u8, QosAggregationCheckpoint>(&canonical)?,
        TypedMigration::GeoIp => typed_migration::migrate_v2_typed::<u32, (u32, String)>(&canonical)?,
        TypedMigration::IdMapping => typed_migration::migrate_v2_typed::<u32, VirtualIdRecord>(&canonical)?,
        TypedMigration::UuidMapping => typed_migration::migrate_v2_typed::<UUIDType, u32>(&canonical)?,
        TypedMigration::TargetM3u => {
            typed_migration::migrate_v2_typed_with_index::<u32, M3uPlaylistItem, u32, _>(&canonical, |item| {
                item.source_ordinal
            })?
        }
        TypedMigration::InputM3u => typed_migration::migrate_v2_typed::<Arc<str>, M3uPlaylistItem>(&canonical)?,
        TypedMigration::Library => typed_migration::migrate_v2_typed::<UUIDType, XtreamPlaylistItem>(&canonical)?,
        TypedMigration::TargetXtream => {
            typed_migration::migrate_v2_typed_with_index::<u32, XtreamPlaylistItem, u32, _>(&canonical, |item| {
                item.source_ordinal
            })?
        }
        TypedMigration::InputXtream => typed_migration::migrate_v2_typed::<u32, XtreamPlaylistItem>(&canonical)?,
        TypedMigration::Epg => typed_migration::migrate_v2_typed::<Arc<str>, EpgChannel>(&canonical)?,
    };

    Ok(SingleMigrationReport {
        path: canonical,
        original_version: current_version,
        target_version: STORAGE_VERSION,
        backup_path,
        live_entries: validation.entries,
        corrupted_entries: validation.corrupted_entries,
        database_id: validation.database_id,
        generation: validation.generation,
        already_current: false,
        kind: kind.label().to_string(),
    })
}

fn count_v2_entries<K, V>(path: &Path) -> io::Result<(usize, usize)>
where
    K: Ord + serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
    V: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
{
    typed_migration::scan_v2_entries::<K, V>(path)
}

fn count_v3_entries<K, V>(path: &Path) -> io::Result<(usize, usize, [u8; 16], u64)>
where
    K: Ord + serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + 'static,
    V: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + 'static,
{
    let mut query = BPlusTreeQuery::<K, V>::try_new(path)?;
    let (id, gen) = query.snapshot_identity();
    let mut live = 0usize;
    let mut corrupt = 0usize;
    for item in query.iter() {
        match item {
            Ok(_) => live += 1,
            Err(_) => corrupt += 1,
        }
    }
    Ok((live, corrupt, id, gen))
}

pub fn inspect_single_database(path: &Path, explicit_kind: Option<&str>) -> io::Result<SingleInspectionReport> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !canonical.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database file does not exist: {}", path.display()),
        ));
    }
    let file_size = std::fs::metadata(&canonical)?.len();
    let version = typed_migration::storage_version(&canonical)?;
    let Some(ver) = version else {
        return Ok(SingleInspectionReport {
            path: canonical,
            version: None,
            file_size,
            kind: "not_a_bplustree".to_string(),
            live_entries: 0,
            corrupted_entries: 0,
            database_id: None,
            generation: None,
        });
    };

    let kind = explicit_kind.and_then(TypedMigration::parse).or_else(|| TypedMigration::infer_from_path(&canonical));

    let kind_label = kind.map_or("unknown", TypedMigration::label);

    if ver == LEGACY_STORAGE_VERSION || ver == 2 {
        if let Some(kind) = kind {
            let (live, corrupt) = match kind {
                TypedMigration::MetadataRetry => {
                    count_v2_entries::<MetadataRetryDbKey, MetadataRetryDbValue>(&canonical)?
                }
                TypedMigration::QosSnapshot => count_v2_entries::<String, QosSnapshotRecord>(&canonical)?,
                TypedMigration::QosCheckpoint => count_v2_entries::<u8, QosAggregationCheckpoint>(&canonical)?,
                TypedMigration::GeoIp => count_v2_entries::<u32, (u32, String)>(&canonical)?,
                TypedMigration::IdMapping => count_v2_entries::<u32, VirtualIdRecord>(&canonical)?,
                TypedMigration::UuidMapping => count_v2_entries::<UUIDType, u32>(&canonical)?,
                TypedMigration::TargetM3u => count_v2_entries::<u32, M3uPlaylistItem>(&canonical)?,
                TypedMigration::InputM3u => count_v2_entries::<Arc<str>, M3uPlaylistItem>(&canonical)?,
                TypedMigration::Library => count_v2_entries::<UUIDType, XtreamPlaylistItem>(&canonical)?,
                TypedMigration::TargetXtream | TypedMigration::InputXtream => {
                    count_v2_entries::<u32, XtreamPlaylistItem>(&canonical)?
                }
                TypedMigration::Epg => count_v2_entries::<Arc<str>, EpgChannel>(&canonical)?,
            };
            return Ok(SingleInspectionReport {
                path: canonical,
                version: Some(ver),
                file_size,
                kind: kind.label().to_string(),
                live_entries: live,
                corrupted_entries: corrupt,
                database_id: None,
                generation: None,
            });
        }
    } else if ver == STORAGE_VERSION {
        if let Some(kind) = kind {
            let (live, corrupt, id, gen) = match kind {
                TypedMigration::MetadataRetry => {
                    count_v3_entries::<MetadataRetryDbKey, MetadataRetryDbValue>(&canonical)?
                }
                TypedMigration::QosSnapshot => count_v3_entries::<String, QosSnapshotRecord>(&canonical)?,
                TypedMigration::QosCheckpoint => count_v3_entries::<u8, QosAggregationCheckpoint>(&canonical)?,
                TypedMigration::GeoIp => count_v3_entries::<u32, (u32, String)>(&canonical)?,
                TypedMigration::IdMapping => count_v3_entries::<u32, VirtualIdRecord>(&canonical)?,
                TypedMigration::UuidMapping => count_v3_entries::<UUIDType, u32>(&canonical)?,
                TypedMigration::TargetM3u => count_v3_entries::<u32, M3uPlaylistItem>(&canonical)?,
                TypedMigration::InputM3u => count_v3_entries::<Arc<str>, M3uPlaylistItem>(&canonical)?,
                TypedMigration::Library => count_v3_entries::<UUIDType, XtreamPlaylistItem>(&canonical)?,
                TypedMigration::TargetXtream | TypedMigration::InputXtream => {
                    count_v3_entries::<u32, XtreamPlaylistItem>(&canonical)?
                }
                TypedMigration::Epg => count_v3_entries::<Arc<str>, EpgChannel>(&canonical)?,
            };
            return Ok(SingleInspectionReport {
                path: canonical,
                version: Some(ver),
                file_size,
                kind: kind.label().to_string(),
                live_entries: live,
                corrupted_entries: corrupt,
                database_id: Some(id),
                generation: Some(gen),
            });
        }
    }

    Ok(SingleInspectionReport {
        path: canonical,
        version: Some(ver),
        file_size,
        kind: kind_label.to_string(),
        live_entries: 0,
        corrupted_entries: 0,
        database_id: None,
        generation: None,
    })
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

//
// The user database has gone through seven serialization schemas (MessagePack,
// positional/sequence encoding via rmp_serde):
//
//   V1 (Deprecated) – original format, 13 fields, no epg_request_timeshift
//   V2              – 14 fields, added epg_request_timeshift
//   V3              – 15 fields, added priority
//   V4              – 17 fields, added soft_connections and soft_priority
//   V5              – 18 fields, added output_clusters
//   V6              – 19 fields, added network_access
//   V7 (current)    – 21 fields, added plan and filter
//
// On first startup after an upgrade the file is still in an older format.
// `migrate_user_db_schema` detects this, converts every record in-place, and
// writes a merge-guard marker so that config-driven user merges cannot
// overwrite the freshly migrated data.

/// Creates the empty merge-guard marker that stops config-driven user merges after a migration.
pub(crate) fn create_user_db_merge_guard(merge_guard_path: &Path) -> io::Result<()> {
    if !merge_guard_path.exists() {
        std::fs::write(merge_guard_path, b"")?;
    }
    Ok(())
}

pub fn user_db_merge_guard_path(config_dir: &Path) -> PathBuf { config_dir.join(MARKER_FILE_API_USER_GUARD) }

/// How a full-record scan treats a pending write-ahead log.
#[derive(Clone, Copy)]
enum UserRecordScan {
    /// Replays the log; only the process that owns the write lease may use this.
    Replaying,
    /// Refuses to open, so a caller that promises read-only access keeps it.
    Refusing,
}

/// Reads the complete source without silently overwriting duplicate user keys.
pub(crate) fn load_user_records(db_path: &Path, storage_version: u32) -> io::Result<BPlusTree<String, Value>> {
    scan_user_records(db_path, storage_version, UserRecordScan::Replaying)
}

/// Read-only variant for offline inspection. A pending V3 WAL is reported
/// instead of being replayed as a side effect of opening the database.
pub(crate) fn load_user_records_read_only(
    db_path: &Path,
    storage_version: u32,
) -> io::Result<BPlusTree<String, Value>> {
    scan_user_records(db_path, storage_version, UserRecordScan::Refusing)
}

/// Loads every user record as raw values, rejecting duplicate keys; `scan` decides whether a pending WAL is replayed.
fn scan_user_records(
    db_path: &Path,
    storage_version: u32,
    scan: UserRecordScan,
) -> io::Result<BPlusTree<String, Value>> {
    if storage_version != STORAGE_VERSION {
        return typed_migration::load_legacy_typed(db_path);
    }
    let mut query = match scan {
        UserRecordScan::Replaying => BPlusTreeQuery::<String, Value>::try_new(db_path)?,
        UserRecordScan::Refusing => BPlusTreeQuery::<String, Value>::try_new_read_only(db_path)?,
    };
    let mut records = BPlusTree::new();
    records.set_metadata(query.metadata().clone());
    for entry in query.iter() {
        let (key, value) = entry?;
        if records.query(&key).is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "user database contains duplicate keys"));
        }
        records.insert(key, value);
    }
    Ok(records)
}

/// Publishes only a fully decoded and verified set of migrated users.
fn store_migrated_users(db_path: &Path, users: &mut BPlusTree<String, StoredApiUserV7>) -> io::Result<()> {
    let name = db_path.file_name().ok_or_else(|| io::Error::other("user database has no file name"))?;
    let staging = db_path.with_file_name(format!("{}.{}.v3.tmp", name.to_string_lossy(), uuid::Uuid::new_v4()));
    let artifacts = BPlusTreeStagingArtifacts::new(db_path, &staging)?;
    let result = (|| {
        users.store(&staging)?;
        let verified = BPlusTree::<String, StoredApiUserV7>::load(&staging)?;
        if verified.len() != users.len() || verified.get_metadata() != users.get_metadata() {
            return Err(io::Error::other("migrated user count or database metadata did not verify"));
        }
        for (key, expected) in &*users {
            let actual = verified.query(key).ok_or_else(|| io::Error::other("migrated user is missing"))?;
            if ApiUserRecovery::from(actual) != ApiUserRecovery::from(expected) {
                return Err(io::Error::other("migrated user fields did not verify"));
            }
        }
        publish_staged_database::<String, StoredApiUserV7>(&staging, db_path)
    })();
    if let Err(error) = artifacts.remove_owned_staging_artifacts() {
        warn!("Could not remove user migration staging artifacts: {error}");
    }
    result
}

/// Normalizes historical and compact user records to the full V7 layout.
///
/// Tracked databases are repaired only from history matching their identity and
/// revision. Untracked records are decoded individually, without skipping any
/// invalid or ambiguous entry. Publication happens after all records verify.
/// Returns whether the user value schema changed; a storage-only upgrade still
/// writes V3 but does not create the user merge guard.
pub(crate) fn migrate_user_db_schema(db_path: &Path, merge_guard_path: &Path) -> io::Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }
    let root = db_path.parent().ok_or_else(|| io::Error::other("user database has no parent directory"))?;
    let _use_guard = tuliprox_btree::RecoveryUseGuard::try_acquire(&tuliprox_btree::RecoveryPaths {
        database: db_path.to_path_buf(),
        directory: root.join("api_user_recovery"),
    })?;
    let storage_version = typed_migration::storage_version(db_path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "user database is not a B+Tree"))?;
    if !matches!(storage_version, 1 | 2) && storage_version != STORAGE_VERSION {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported user B+Tree storage version"));
    }
    if storage_version == STORAGE_VERSION {
        if BPlusTree::<String, StoredApiUserV7>::load(db_path).is_ok() {
            return Ok(false);
        }
        let tracked = {
            let query = BPlusTreeQuery::<String, Value>::try_new(db_path)?;
            matches!(query.metadata(), BPlusTreeMetadata::Recovery(_))
        };
        if tracked {
            let root = db_path.parent().ok_or_else(|| io::Error::other("user database has no parent directory"))?;
            ApiUserRepository::rebuild_from_history(db_path, root)?;
            create_user_db_merge_guard(merge_guard_path)?;
            return Ok(true);
        }
    }

    let source = load_user_records(db_path, storage_version)?;
    let mut users = BPlusTree::new();
    users.set_metadata(source.get_metadata().clone());
    let mut schema_changed = false;
    for (key, value) in &source {
        schema_changed |= value.as_array().is_none_or(|fields| fields.len() != 21);
        let user = decode_user_record(value.clone()).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("user key {}: {error}", serde_json::to_string(key).unwrap_or_default()),
            )
        })?;
        users.insert(key.clone(), user);
    }
    store_migrated_users(db_path, &mut users)?;
    if schema_changed {
        create_user_db_merge_guard(merge_guard_path)?;
    }
    Ok(schema_changed)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AllStartupMigrationStats {
    pub bplustree: BPlusTreeMigrationStats,
    pub user_db_migrated: bool,
}

/// Runs all startup migrations in sequence:
/// 1. User DB normalization/recovery into the full V7 layout and current storage format
/// 2. Storage-format migration for the remaining B+Tree databases
///
/// `config_dir` is the directory that contains `api_user.db` and the merge-guard
/// marker. `storage_dir` is used for the B+Tree migration marker.
fn run_all_startup_migrations(
    roots: &[PathBuf],
    storage_dir: &Path,
    config_dir: &Path,
) -> io::Result<AllStartupMigrationStats> {
    let user_db_path = config_dir.join(storage_const::API_USER_DB_FILE);
    let merge_guard_path = user_db_merge_guard_path(config_dir);
    let user_db_migrated = migrate_user_db_schema(&user_db_path, &merge_guard_path)
        .map_err(|error| crate::user_db_repair::user_db_repair_error(&user_db_path, &error))?;
    let marker_path = bplustree_migration_marker_path(storage_dir);
    let bplustree = BPlusTreeStartupMigrator::new_with_marker(roots.to_vec(), marker_path).run()?;

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
            if stats.bplustree.marker_was_current {
                trace!("B+Tree startup migration marker was current; database headers were still scanned");
            }
            if stats.bplustree.migrated_files > 0 {
                info!(
                    "B+Tree startup migration completed: migrated {} file(s) ({} B+Tree files checked, {} .db files scanned)",
                    stats.bplustree.migrated_files,
                    stats.bplustree.bplustree_files,
                    stats.bplustree.scanned_files
                );
            }
            if stats.user_db_migrated {
                info!("User DB schema migrated to V7");
            }
        }
        Err(err) => {
            tuliprox_core::utils::exit!("Startup migration failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmp_serde::{from_slice, to_vec};
    use serde::Serialize;
    use shared::model::{EpgCategory, EpgProgramme, LiveStreamProperties};
    use std::io::Read;
    use tempfile::tempdir;

    #[derive(Serialize)]
    struct LegacyLiveStreamProperties {
        name: String,
        category_id: u32,
        stream_id: u32,
        stream_icon: String,
        direct_source: String,
        custom_sid: Option<String>,
        added: Option<String>,
        stream_type: Option<String>,
        epg_channel_id: Option<String>,
        tv_archive: Option<i32>,
        tv_archive_duration: Option<i32>,
        is_adult: i32,
        video: Option<String>,
        audio: Option<String>,
        last_probed_timestamp: Option<i64>,
        last_success_timestamp: Option<i64>,
    }

    #[derive(Serialize)]
    struct LegacyEpgProgramme {
        start: i64,
        stop: i64,
        title: Option<String>,
        desc: Option<String>,
        catchup_id: Option<String>,
        categories: Vec<EpgCategory>,
        is_live: bool,
        is_new: bool,
        previously_shown: bool,
    }

    #[test]
    fn live_stream_properties_accept_legacy_messagepack_without_catchup_field() {
        let encoded = to_vec(&LegacyLiveStreamProperties {
            name: "Channel 7".to_string(),
            category_id: 0,
            stream_id: 7,
            stream_icon: String::new(),
            direct_source: String::new(),
            custom_sid: None,
            added: None,
            stream_type: None,
            epg_channel_id: Some("channel.7".to_string()),
            tv_archive: Some(1),
            tv_archive_duration: Some(7),
            is_adult: 0,
            video: None,
            audio: None,
            last_probed_timestamp: Some(100),
            last_success_timestamp: Some(200),
        })
        .expect("legacy live properties should encode");

        let decoded: LiveStreamProperties = from_slice(&encoded).expect("legacy live properties should decode");
        assert_eq!(decoded.stream_id, 7);
        assert_eq!(decoded.name.as_ref(), "Channel 7");
        assert_eq!(decoded.tv_archive, Some(1));
        assert_eq!(decoded.tv_archive_duration, Some(7));
        assert!(decoded.catchup.is_none());
        assert_eq!(decoded.bitrate, 0);
    }

    #[test]
    fn epg_programme_accepts_complete_legacy_messagepack_without_icon_field() {
        let encoded = to_vec(&LegacyEpgProgramme {
            start: 100,
            stop: 200,
            title: Some("Programme".to_string()),
            desc: Some("Description".to_string()),
            catchup_id: Some("catchup-42".to_string()),
            categories: vec![EpgCategory { value: "Sports".into(), lang: Some("en".into()) }],
            is_live: true,
            is_new: true,
            previously_shown: true,
        })
        .expect("legacy programme should encode");

        let decoded: EpgProgramme = from_slice(&encoded).expect("legacy programme should decode");
        assert_eq!(decoded.start, 100);
        assert_eq!(decoded.stop, 200);
        assert_eq!(decoded.title.as_deref(), Some("Programme"));
        assert_eq!(decoded.desc.as_deref(), Some("Description"));
        assert!(decoded.icon.is_none());
        assert_eq!(decoded.catchup_id.as_deref(), Some("catchup-42"));
        assert_eq!(decoded.categories, vec![EpgCategory { value: "Sports".into(), lang: Some("en".into()) }]);
        assert!(decoded.is_live);
        assert!(decoded.is_new);
        assert!(decoded.previously_shown);
    }

    fn test_roots_fingerprint(roots: &[PathBuf]) -> String {
        let resolved = BPlusTreeStartupMigrator::resolve_scan_roots(roots);
        BPlusTreeStartupMigrator::roots_fingerprint(&resolved)
    }

    fn write_legacy_geoip(root: &Path, version: u32) -> io::Result<PathBuf> {
        let path = root.join("geoip.db");
        let mut tree = crate::bplustree::v2::BPlusTree::new();
        tree.insert(1u32, (2u32, String::from("DE")));
        tree.store(&path)?;
        if version == LEGACY_STORAGE_VERSION {
            let mut file = OpenOptions::new().write(true).open(&path)?;
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&version.to_le_bytes())?;
            file.sync_all()?;
        }
        Ok(path)
    }

    #[test]
    fn startup_migrator_upgrades_legacy_bplustree_files() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = write_legacy_geoip(temp.path(), LEGACY_STORAGE_VERSION)?;

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
        let query = BPlusTree::<u32, (u32, String)>::load(&db_path)?;
        assert_eq!(query.query(&1), Some(&(2, String::from("DE"))));

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
    fn startup_migrator_rejects_an_unknown_valid_legacy_tree_without_changing_it() -> io::Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("backup.db");
        let mut tree = crate::bplustree::v2::BPlusTree::new();
        tree.insert(1u32, String::from("preserve"));
        tree.store(&path)?;
        let before = std::fs::read(&path)?;

        assert!(migrate_bplustree_databases(&[temp.path().to_path_buf()]).is_err());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn startup_migrator_reports_a_locked_legacy_tree_without_modifying_it() -> io::Result<()> {
        let temp = tempdir()?;
        let path = write_legacy_geoip(temp.path(), 2)?;
        let before = std::fs::read(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.lock()?;

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.bplustree_files, 1);
        assert_eq!(stats.migrated_files, 0);
        assert_eq!(std::fs::read(&path)?, before);
        file.unlock()?;
        Ok(())
    }

    #[test]
    fn startup_migrator_only_removes_exact_abandoned_v3_temporary_names() -> io::Result<()> {
        let temp = tempdir()?;
        let abandoned = temp.path().join(format!("geoip.db.{}.v3.tmp", uuid::Uuid::new_v4()));
        let unrelated = temp.path().join("geoip.db.not-a-uuid.v3.tmp");
        std::fs::write(&abandoned, b"partial")?;
        std::fs::write(&unrelated, b"keep")?;

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.scanned_files, 0);
        assert!(!abandoned.exists());
        assert!(unrelated.exists());
        Ok(())
    }

    #[test]
    fn typed_registry_matches_only_the_persisted_path_families() -> io::Result<()> {
        let temp = tempdir()?;
        let roots = [temp.path().to_path_buf()];
        let cases = [
            ("input_news/metadata_retry_state.db", TypedMigration::MetadataRetry),
            ("qos_snapshot.db", TypedMigration::QosSnapshot),
            ("qos_snapshot_meta.db", TypedMigration::QosCheckpoint),
            ("geoip.db", TypedMigration::GeoIp),
            ("target/id_mapping.db", TypedMigration::IdMapping),
            ("target/id_mapping.uuid.db", TypedMigration::UuidMapping),
            ("target/m3u/m3u.db", TypedMigration::TargetM3u),
            ("input_news/m3u_news.db", TypedMigration::InputM3u),
            ("input_news/lib_news.db", TypedMigration::Library),
            ("input_news/media_server_news.db", TypedMigration::Library),
            ("target/xtream/live.db", TypedMigration::TargetXtream),
            ("target/xtream/video.db", TypedMigration::TargetXtream),
            ("input_news/live.db", TypedMigration::InputXtream),
            ("input_news/video.db", TypedMigration::InputXtream),
            ("target/m3u/epg.db", TypedMigration::Epg),
            ("target/xtream/epg.db", TypedMigration::Epg),
        ];
        for (relative, expected) in cases {
            assert_eq!(BPlusTreeStartupMigrator::migration_kind(&temp.path().join(relative), &roots), Some(expected));
        }
        assert_eq!(BPlusTreeStartupMigrator::migration_kind(&temp.path().join("backup/tree.db"), &roots), None);
        assert_eq!(BPlusTreeStartupMigrator::migration_kind(&temp.path().join("backup/series.db"), &roots), None);
        assert_eq!(BPlusTreeStartupMigrator::migration_kind(&temp.path().join("input_news/vod.db"), &roots), None);
        Ok(())
    }

    #[test]
    fn single_database_migration_rejects_a_locked_file() -> io::Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("series.db");
        let mut tree = crate::bplustree::v2::BPlusTree::<u32, XtreamPlaylistItem>::new();
        tree.store(&path)?;
        let before = std::fs::read(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.lock()?;

        let error = migrate_single_database(&path, Some("series"), false)
            .expect_err("an exclusively locked database must not be migrated");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&path)?, before);
        file.unlock()?;
        Ok(())
    }

    #[test]
    fn single_database_inspection_reports_v1_entries() -> io::Result<()> {
        let temp = tempdir()?;
        let path = write_legacy_geoip(temp.path(), LEGACY_STORAGE_VERSION)?;
        let before = std::fs::read(&path)?;

        let report = inspect_single_database(&path, Some("geoip"))?;
        assert_eq!(report.version, Some(LEGACY_STORAGE_VERSION));
        assert_eq!(report.live_entries, 1);
        assert_eq!(report.corrupted_entries, 0);
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn startup_migrator_exercises_every_typed_registry_family() -> io::Result<()> {
        let temp = tempdir()?;
        let mut paths = Vec::new();
        macro_rules! empty_v2 {
            ($relative:literal, $key:ty, $value:ty) => {{
                let path = temp.path().join($relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut tree = crate::bplustree::v2::BPlusTree::<$key, $value>::new();
                tree.store(&path)?;
                paths.push(path);
            }};
        }
        empty_v2!("input_news/metadata_retry_state.db", MetadataRetryDbKey, MetadataRetryDbValue);
        empty_v2!("qos_snapshot.db", String, QosSnapshotRecord);
        empty_v2!("qos_snapshot_meta.db", u8, QosAggregationCheckpoint);
        empty_v2!("geoip.db", u32, (u32, String));
        empty_v2!("target/id_mapping.db", u32, VirtualIdRecord);
        empty_v2!("target/id_mapping.uuid.db", UUIDType, u32);
        empty_v2!("target/m3u/m3u.db", u32, M3uPlaylistItem);
        empty_v2!("input_news/m3u_news.db", Arc<str>, M3uPlaylistItem);
        empty_v2!("input_news/lib_news.db", UUIDType, XtreamPlaylistItem);
        empty_v2!("input_news/media_server_news.db", UUIDType, XtreamPlaylistItem);
        empty_v2!("target/xtream/live.db", u32, XtreamPlaylistItem);
        empty_v2!("target/xtream/video.db", u32, XtreamPlaylistItem);
        empty_v2!("target/xtream/series.db", u32, XtreamPlaylistItem);
        empty_v2!("input_news/live.db", u32, XtreamPlaylistItem);
        empty_v2!("input_news/video.db", u32, XtreamPlaylistItem);
        empty_v2!("input_news/series.db", u32, XtreamPlaylistItem);
        empty_v2!("target/m3u/epg.db", Arc<str>, EpgChannel);
        empty_v2!("target/xtream/epg.db", Arc<str>, EpgChannel);

        let stats = migrate_bplustree_databases(&[temp.path().to_path_buf()])?;
        assert_eq!(stats.migrated_files, paths.len());
        for path in paths {
            assert_eq!(crate::bplustree::typed_migration::storage_version(&path)?, Some(STORAGE_VERSION));
        }
        assert!(temp.path().join("target/m3u/m3u.idx").exists());
        assert!(temp.path().join("target/xtream/live.idx").exists());
        Ok(())
    }

    #[test]
    fn startup_migrator_writes_marker_after_success() -> io::Result<()> {
        let temp = tempdir()?;
        let temp_other = tempdir()?;
        let _ = write_legacy_geoip(temp.path(), LEGACY_STORAGE_VERSION)?;
        let _ = write_legacy_geoip(temp_other.path(), LEGACY_STORAGE_VERSION)?;

        let stats = migrate_bplustree_databases_with_marker(
            &[temp.path().to_path_buf(), temp_other.path().to_path_buf()],
            temp.path(),
        )?;
        assert_eq!(stats.migrated_files, 2);
        assert!(!stats.marker_was_current);

        let marker = bplustree_migration_marker_path(temp.path());
        assert!(marker.exists());
        assert!(marker.is_file());
        let marker_other = bplustree_migration_marker_path(temp_other.path());
        assert!(!marker_other.exists());

        Ok(())
    }

    #[test]
    fn startup_migrator_scans_when_marker_exists() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = write_legacy_geoip(temp.path(), LEGACY_STORAGE_VERSION)?;

        let marker = bplustree_migration_marker_path(temp.path());
        let roots = [temp.path().to_path_buf()];
        let fingerprint = test_roots_fingerprint(&roots);
        BPlusTreeStartupMigrator::write_migration_marker(&marker, &fingerprint)?;

        let stats = migrate_bplustree_databases_with_marker(&roots, temp.path())?;
        assert_eq!(stats.scanned_files, 1);
        assert_eq!(stats.bplustree_files, 1);
        assert_eq!(stats.migrated_files, 1);
        assert!(stats.marker_was_current);

        let mut check = OpenOptions::new().read(true).open(&db_path)?;
        let mut version_bytes = [0u8; 8];
        check.read_exact(&mut version_bytes)?;
        let version = u32::from_le_bytes(
            version_bytes[4..8].try_into().map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
        );
        assert_eq!(version, STORAGE_VERSION);

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
        assert!(stats.marker_was_current);
        assert!(!legacy_per_root_marker.exists());
        assert!(global_marker.exists());

        Ok(())
    }

    #[test]
    fn startup_migrator_does_not_skip_when_marker_fingerprint_differs() -> io::Result<()> {
        let temp_a = tempdir()?;
        let temp_b = tempdir()?;
        let _ = write_legacy_geoip(temp_a.path(), LEGACY_STORAGE_VERSION)?;

        let marker = bplustree_migration_marker_path(temp_a.path());
        let old_roots = [temp_a.path().to_path_buf()];
        let old_fingerprint = test_roots_fingerprint(&old_roots);
        BPlusTreeStartupMigrator::write_migration_marker(&marker, &old_fingerprint)?;

        let current_roots = [temp_a.path().to_path_buf(), temp_b.path().to_path_buf()];
        let stats = migrate_bplustree_databases_with_marker(&current_roots, temp_a.path())?;
        assert!(!stats.marker_was_current);
        assert_eq!(stats.migrated_files, 1);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v2_to_v7_creates_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v2_tree: crate::bplustree::v2::BPlusTree<String, StoredApiUserV2> =
            crate::bplustree::v2::BPlusTree::new();
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

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"alice".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "alice missing after migration"))?;
        assert_eq!(user.username, "alice");
        assert_eq!(user.epg_request_timeshift.as_deref(), Some("2"));
        assert_eq!(user.output_clusters, ClusterFlags::all());
        assert_eq!(user.priority, None);
        assert_eq!(user.soft_connections, None);
        assert_eq!(user.soft_priority, None);
        assert_eq!(user.network_access, None);
        assert_eq!(user.plan, None);
        assert_eq!(user.filter, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v3_to_v7_creates_merge_guard() -> io::Result<()> {
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
        assert!(migrated);
        assert!(merge_guard_path.exists());

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"bob".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bob missing after migration"))?;
        assert_eq!(user.output_clusters, ClusterFlags::all());
        assert_eq!(user.priority, Some(5));
        assert_eq!(user.soft_connections, None);
        assert_eq!(user.soft_priority, None);
        assert_eq!(user.network_access, None);
        assert_eq!(user.plan, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v4_to_v7_creates_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v4_tree: BPlusTree<String, StoredApiUserV4> = BPlusTree::new();
        v4_tree.insert(
            "carol".to_string(),
            StoredApiUserV4 {
                target: "channels".to_string(),
                username: "carol".to_string(),
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
                soft_connections: Some(2),
                soft_priority: Some(-4),
            },
        );
        let _ = v4_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(migrated);
        assert!(merge_guard_path.exists());

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"carol".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "carol missing after migration"))?;
        assert_eq!(user.output_clusters, ClusterFlags::all());
        assert_eq!(user.soft_connections, Some(2));
        assert_eq!(user.soft_priority, Some(-4));
        assert_eq!(user.network_access, None);
        assert_eq!(user.plan, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v5_to_v7_creates_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v5_tree: BPlusTree<String, StoredApiUserV5> = BPlusTree::new();
        v5_tree.insert(
            "dave".to_string(),
            StoredApiUserV5 {
                target: "channels".to_string(),
                username: "dave".to_string(),
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
                output_clusters: ClusterFlags::Live | ClusterFlags::Vod,
                ui_enabled: true,
                comment: None,
                priority: Some(5),
                soft_connections: Some(2),
                soft_priority: Some(-4),
            },
        );
        let _ = v5_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(migrated);
        assert!(merge_guard_path.exists());

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"dave".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "dave missing after v7 migration"))?;
        assert_eq!(user.output_clusters, ClusterFlags::Live | ClusterFlags::Vod);
        assert_eq!(user.priority, Some(5));
        assert_eq!(user.soft_connections, Some(2));
        assert_eq!(user.soft_priority, Some(-4));
        assert_eq!(user.network_access, None);
        assert_eq!(user.plan, None);
        assert_eq!(user.filter, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_migration_v6_to_v7_creates_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v6_tree: crate::bplustree::v2::BPlusTree<String, StoredApiUserV6> =
            crate::bplustree::v2::BPlusTree::new();
        v6_tree.insert(
            "erin".to_string(),
            StoredApiUserV6 {
                target: "channels".to_string(),
                username: "erin".to_string(),
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
                output_clusters: ClusterFlags::Live | ClusterFlags::Vod,
                ui_enabled: true,
                comment: None,
                priority: Some(5),
                soft_connections: Some(2),
                soft_priority: Some(-4),
                network_access: Some(NetworkAccessDto {
                    allowed_countries: Some(vec!["DE".to_string()]),
                    allowed_networks: Some(vec!["192.168.0.0/16".to_string()]),
                }),
            },
        );
        let _ = v6_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(migrated);
        assert!(merge_guard_path.exists());

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"erin".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "erin missing after v7 migration"))?;
        assert_eq!(user.output_clusters, ClusterFlags::Live | ClusterFlags::Vod);
        assert_eq!(user.priority, Some(5));
        assert_eq!(user.soft_connections, Some(2));
        assert_eq!(user.soft_priority, Some(-4));
        assert_eq!(
            user.network_access.as_ref().and_then(|value| value.allowed_countries.as_ref()),
            Some(&vec!["DE".to_string()])
        );
        assert_eq!(user.plan, None);
        assert_eq!(user.filter, None);

        Ok(())
    }

    #[test]
    fn user_db_schema_v7_is_detected_without_writing_merge_guard() -> io::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(temp.path());

        let mut v7_tree: BPlusTree<String, StoredApiUserV7> = BPlusTree::new();
        v7_tree.insert(
            "frank".to_string(),
            StoredApiUserV7 {
                target: "channels".to_string(),
                username: "frank".to_string(),
                password: "secret".to_string(),
                token: None,
                proxy: ProxyType::Reverse(None),
                server: None,
                epg_timeshift: None,
                epg_request_timeshift: None,
                created_at: None,
                exp_date: None,
                max_connections: None,
                status: Some(ProxyUserStatus::Active),
                output_clusters: ClusterFlags::all(),
                ui_enabled: true,
                comment: None,
                priority: None,
                soft_connections: None,
                soft_priority: None,
                network_access: None,
                plan: Some("basic".to_string()),
                filter: Some(r#"Group ~ "^DE.*""#.to_string()),
            },
        );
        let _ = v7_tree.store(&db_path)?;
        assert!(!merge_guard_path.exists());

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(!migrated);
        assert!(!merge_guard_path.exists());

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"frank".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "frank missing after v7 detection"))?;
        assert_eq!(user.plan.as_deref(), Some("basic"));
        assert_eq!(user.filter.as_deref(), Some(r#"Group ~ "^DE.*""#));

        Ok(())
    }

    #[test]
    fn repair_archive_directories_are_not_scanned() -> io::Result<()> {
        let temp = tempdir()?;
        let root = temp.path().to_path_buf();
        let archive_dir = root.join("api_user_repair_backup_test");
        std::fs::create_dir_all(&archive_dir)?;
        let series_db = archive_dir.join("series.db");
        let mut tree = crate::bplustree::v2::BPlusTree::<u32, XtreamPlaylistItem>::new();
        tree.store(&series_db)?;
        assert_eq!(crate::bplustree::typed_migration::storage_version(&series_db)?, Some(2));

        let marker = root.join("migration.marker");
        let stats = BPlusTreeStartupMigrator::new_with_marker(vec![root], marker).run()?;
        assert_eq!(stats.migrated_files, 0);
        assert_eq!(crate::bplustree::typed_migration::storage_version(&series_db)?, Some(2));
        Ok(())
    }

    #[test]
    fn repair_archive_scan_root_is_ignored() -> io::Result<()> {
        let temp = tempdir()?;
        let archive_dir = temp.path().join("api_user_repair_backup_test");
        std::fs::create_dir_all(&archive_dir)?;
        let series_db = archive_dir.join("series.db");
        let mut tree = crate::bplustree::v2::BPlusTree::<u32, XtreamPlaylistItem>::new();
        tree.store(&series_db)?;

        let roots = vec![archive_dir.clone()];
        let resolved = BPlusTreeStartupMigrator::resolve_scan_roots(&roots);
        assert!(resolved.is_empty(), "repair archive scan root must be ignored");

        let marker = temp.path().join("migration.marker");
        let stats = BPlusTreeStartupMigrator::new_with_marker(roots, marker).run()?;
        assert_eq!(stats.migrated_files, 0);
        assert_eq!(crate::bplustree::typed_migration::storage_version(&series_db)?, Some(2));
        Ok(())
    }

    #[test]
    fn repair_archive_marker_location_is_not_written() -> io::Result<()> {
        let temp = tempdir()?;
        let archive_dir = temp.path().join("api_user_repair_backup_x");
        std::fs::create_dir_all(&archive_dir)?;
        let marker = archive_dir.join("migration.marker");

        let stats = BPlusTreeStartupMigrator::new_with_marker(vec![temp.path().to_path_buf()], marker.clone()).run()?;
        assert_eq!(stats.migrated_files, 0);
        assert!(!marker.exists(), "marker inside a repair archive directory must not be written");
        Ok(())
    }

    use crate::api_user_recovery::test_fixtures::{compact_user, CompactUserRecord};

    fn create_tracked_compact_fixture(dir: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let db_path = dir.join(storage_const::API_USER_DB_FILE);
        let merge_guard_path = user_db_merge_guard_path(dir);

        let (mut repository, _report) = ApiUserRepository::open(&db_path, dir)?;
        let cred = crate::user_repository::StoredProxyUserCredentials {
            target: "channels".to_string(),
            username: "alice".to_string(),
            password: "not-a-real-password".to_string(),
            token: None,
            proxy: ProxyType::Reverse(None),
            server: None,
            epg_timeshift: None,
            epg_request_timeshift: None,
            created_at: None,
            exp_date: None,
            max_connections: None,
            status: Some(ProxyUserStatus::Active),
            output_clusters: ClusterFlags::all(),
            ui_enabled: true,
            comment: None,
            priority: None,
            soft_connections: None,
            soft_priority: None,
            network_access: None,
            plan: Some("premium".to_string()),
            filter: Some(r#"Group ~ "^DE.*""#.to_string()),
        };
        repository.commit(vec![("alice".to_string(), cred)])?;
        drop(repository);

        let metadata = {
            let query = BPlusTreeQuery::<String, Value>::try_new(&db_path)?;
            query.metadata().clone()
        };
        let mut compact_tree = BPlusTree::<String, CompactUserRecord>::new();
        compact_tree.set_metadata(metadata);
        compact_tree.insert("alice".to_string(), compact_user("alice", Some("premium"), None));
        compact_tree.store(&db_path)?;

        Ok((db_path, merge_guard_path))
    }

    #[test]
    fn migrate_user_db_schema_tracked_compact_with_matching_history() -> io::Result<()> {
        let temp = tempdir()?;
        let (db_path, merge_guard_path) = create_tracked_compact_fixture(temp.path())?;

        assert!(!merge_guard_path.exists());
        assert!(BPlusTree::<String, StoredApiUserV7>::load(&db_path).is_err(), "compact records must fail V7 load");

        let migrated = migrate_user_db_schema(&db_path, &merge_guard_path)?;
        assert!(migrated, "migration should return Ok(true)");
        assert!(merge_guard_path.exists(), "merge guard must exist");

        let v7_tree = BPlusTree::<String, StoredApiUserV7>::load(&db_path)?;
        let user = v7_tree
            .query(&"alice".to_string())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "alice missing after rebuild"))?;
        assert_eq!(user.plan.as_deref(), Some("premium"));
        assert_eq!(user.filter.as_deref(), Some(r#"Group ~ "^DE.*""#));

        let mut query = BPlusTreeQuery::<String, Value>::try_new(&db_path)?;
        let mut count = 0;
        for entry in query.iter() {
            let (_, val) = entry?;
            let arr = val.as_array().ok_or_else(|| io::Error::other("stored record is not an array"))?;
            assert_eq!(arr.len(), 21, "all records must have 21 fields");
            count += 1;
        }
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    fn migrate_user_db_schema_tracked_compact_without_recovery_dir_fails_and_leaves_db_unchanged() -> io::Result<()> {
        let temp = tempdir()?;
        let (db_path, merge_guard_path) = create_tracked_compact_fixture(temp.path())?;

        std::fs::remove_dir_all(temp.path().join("api_user_recovery"))?;
        let before_bytes = std::fs::read(&db_path)?;

        let err = migrate_user_db_schema(&db_path, &merge_guard_path)
            .expect_err("migration must fail when recovery directory is removed");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err_msg = err.to_string();
        assert!(err_msg.contains("no verifiable recovery generation was found"), "unexpected error message: {err_msg}");
        assert!(!err_msg.contains("not-a-real-password"), "error message leaked password: {err_msg}");
        assert_eq!(std::fs::read(&db_path)?, before_bytes, "database file bytes must be unchanged");
        assert!(!merge_guard_path.exists(), "merge guard must not exist");

        Ok(())
    }
}
