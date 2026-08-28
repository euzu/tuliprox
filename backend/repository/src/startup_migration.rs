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
use crate::{
    qos_snapshot_repository::{QosAggregationCheckpoint, QosSnapshotRecord},
    storage_const,
    target_id_mapping::VirtualIdRecord,
};
use log::{info, trace, warn};
use shared::model::{
    ClusterFlags, ConfigPaths, EpgChannel, M3uPlaylistItem, NetworkAccessDto, ProxyType, ProxyUserStatus, UUIDType,
    XtreamPlaylistItem,
};
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

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
        self.cleanup_legacy_root_markers(self.migration_marker_path.as_deref())?;
        let resolved_roots = Self::resolve_scan_roots(&self.roots);
        let roots_fingerprint = Self::roots_fingerprint(&resolved_roots);
        if let Some(marker_path) = &self.migration_marker_path {
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
        let relative = roots.iter().find_map(|root| path.strip_prefix(root).ok())?;
        let components = relative.iter().filter_map(OsStr::to_str).collect::<Vec<_>>();
        match components.as_slice() {
            ["metadata_retry_state.db"] => Some(TypedMigration::MetadataRetry),
            ["qos_snapshot.db"] => Some(TypedMigration::QosSnapshot),
            ["qos_snapshot_meta.db"] => Some(TypedMigration::QosCheckpoint),
            ["geoip.db"] => Some(TypedMigration::GeoIp),
            [directory, "metadata_retry_state.db"] if directory.starts_with("input_") => {
                Some(TypedMigration::MetadataRetry)
            }
            [directory, name] if directory.starts_with("input_") && name.starts_with("m3u_") => {
                Some(TypedMigration::InputM3u)
            }
            [directory, name]
                if directory.starts_with("input_")
                    && (name.starts_with("lib_") || name.starts_with("media_server_")) =>
            {
                Some(TypedMigration::Library)
            }
            [directory, name]
                if directory.starts_with("input_") && matches!(*name, "live.db" | "video.db" | "series.db") =>
            {
                Some(TypedMigration::InputXtream)
            }
            [_, "id_mapping.db"] => Some(TypedMigration::IdMapping),
            [_, "id_mapping.uuid.db"] => Some(TypedMigration::UuidMapping),
            [_, "m3u", "m3u.db"] => Some(TypedMigration::TargetM3u),
            [_, "m3u" | "xtream", "epg.db"] => Some(TypedMigration::Epg),
            [_, "xtream", "live.db" | "video.db" | "series.db"] => Some(TypedMigration::TargetXtream),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypedMigration {
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

// V4 mirror — same layout as user_repository::StoredProxyUserCredentials.
// Defined here so the migration has no dependency on user_repository internals.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApiUserV4 {
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
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
}

impl StoredApiUserV4 {
    fn from_v3(v3: &StoredApiUserV3) -> Self {
        Self {
            target: v3.target.clone(),
            username: v3.username.clone(),
            password: v3.password.clone(),
            token: v3.token.clone(),
            proxy: v3.proxy,
            server: v3.server.clone(),
            epg_timeshift: v3.epg_timeshift.clone(),
            epg_request_timeshift: v3.epg_request_timeshift.clone(),
            created_at: v3.created_at,
            exp_date: v3.exp_date,
            max_connections: v3.max_connections,
            status: v3.status,
            ui_enabled: v3.ui_enabled,
            comment: v3.comment.clone(),
            priority: v3.priority,
            soft_connections: None,
            soft_priority: None,
        }
    }

    fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v3(&StoredApiUserV3::from_v2(v2)) }

    fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v3(&StoredApiUserV3::from_v1(v1)) }
}

// V5 mirror — same layout as the previous user_repository::StoredProxyUserCredentials.
// Defined here so the migration has no dependency on user_repository internals.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApiUserV5 {
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
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
}

impl StoredApiUserV5 {
    fn from_v4(v4: &StoredApiUserV4) -> Self {
        Self {
            target: v4.target.clone(),
            username: v4.username.clone(),
            password: v4.password.clone(),
            token: v4.token.clone(),
            proxy: v4.proxy,
            server: v4.server.clone(),
            epg_timeshift: v4.epg_timeshift.clone(),
            epg_request_timeshift: v4.epg_request_timeshift.clone(),
            created_at: v4.created_at,
            exp_date: v4.exp_date,
            max_connections: v4.max_connections,
            status: v4.status,
            output_clusters: ClusterFlags::all(),
            ui_enabled: v4.ui_enabled,
            comment: v4.comment.clone(),
            priority: v4.priority,
            soft_connections: v4.soft_connections,
            soft_priority: v4.soft_priority,
        }
    }

    fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v4(&StoredApiUserV4::from_v3(v3)) }

    fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v4(&StoredApiUserV4::from_v2(v2)) }

    fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v4(&StoredApiUserV4::from_v1(v1)) }
}

// V6 mirror — same layout as the previous user_repository::StoredProxyUserCredentials.
// Defined here so the migration has no dependency on user_repository internals.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApiUserV6 {
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
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
    pub network_access: Option<NetworkAccessDto>,
}

impl StoredApiUserV6 {
    fn from_v5(v5: &StoredApiUserV5) -> Self {
        Self {
            target: v5.target.clone(),
            username: v5.username.clone(),
            password: v5.password.clone(),
            token: v5.token.clone(),
            proxy: v5.proxy,
            server: v5.server.clone(),
            epg_timeshift: v5.epg_timeshift.clone(),
            epg_request_timeshift: v5.epg_request_timeshift.clone(),
            created_at: v5.created_at,
            exp_date: v5.exp_date,
            max_connections: v5.max_connections,
            status: v5.status,
            output_clusters: v5.output_clusters,
            ui_enabled: v5.ui_enabled,
            comment: v5.comment.clone(),
            priority: v5.priority,
            soft_connections: v5.soft_connections,
            soft_priority: v5.soft_priority,
            network_access: None,
        }
    }

    fn from_v4(v4: &StoredApiUserV4) -> Self { Self::from_v5(&StoredApiUserV5::from_v4(v4)) }

    fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v5(&StoredApiUserV5::from_v3(v3)) }

    fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v5(&StoredApiUserV5::from_v2(v2)) }

    fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v5(&StoredApiUserV5::from_v1(v1)) }
}

// V7 mirror — same layout as user_repository::StoredProxyUserCredentials.
// Defined here so the migration has no dependency on user_repository internals.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApiUserV7 {
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
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
    pub network_access: Option<NetworkAccessDto>,
    pub plan: Option<String>,
    pub filter: Option<String>,
}

impl StoredApiUserV7 {
    fn from_v6(v6: &StoredApiUserV6) -> Self {
        Self {
            target: v6.target.clone(),
            username: v6.username.clone(),
            password: v6.password.clone(),
            token: v6.token.clone(),
            proxy: v6.proxy,
            server: v6.server.clone(),
            epg_timeshift: v6.epg_timeshift.clone(),
            epg_request_timeshift: v6.epg_request_timeshift.clone(),
            created_at: v6.created_at,
            exp_date: v6.exp_date,
            max_connections: v6.max_connections,
            status: v6.status,
            output_clusters: v6.output_clusters,
            ui_enabled: v6.ui_enabled,
            comment: v6.comment.clone(),
            priority: v6.priority,
            soft_connections: v6.soft_connections,
            soft_priority: v6.soft_priority,
            network_access: v6.network_access.clone(),
            plan: None,
            filter: None,
        }
    }

    fn from_v5(v5: &StoredApiUserV5) -> Self { Self::from_v6(&StoredApiUserV6::from_v5(v5)) }

    fn from_v4(v4: &StoredApiUserV4) -> Self { Self::from_v6(&StoredApiUserV6::from_v4(v4)) }

    fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v6(&StoredApiUserV6::from_v3(v3)) }

    fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v6(&StoredApiUserV6::from_v2(v2)) }

    fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v6(&StoredApiUserV6::from_v1(v1)) }
}

fn create_user_db_merge_guard(merge_guard_path: &Path) -> io::Result<()> {
    if !merge_guard_path.exists() {
        std::fs::write(merge_guard_path, b"")?;
    }
    Ok(())
}

pub fn user_db_merge_guard_path(config_dir: &Path) -> PathBuf { config_dir.join(MARKER_FILE_API_USER_GUARD) }

fn migrate_legacy_user_schema<SourceV, Map>(db_path: &Path, merge_guard_path: &Path, map: Map) -> io::Result<bool>
where
    SourceV: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
    Map: FnMut(SourceV) -> StoredApiUserV7,
{
    if typed_migration::migrate_v2_typed_map::<String, SourceV, StoredApiUserV7, _>(db_path, map).is_err() {
        return Ok(false);
    }
    create_user_db_merge_guard(merge_guard_path)?;
    Ok(true)
}

fn migrate_current_user_schema<SourceV, Map>(db_path: &Path, merge_guard_path: &Path, map: Map) -> io::Result<bool>
where
    SourceV: for<'de> serde::Deserialize<'de>,
    Map: Fn(&SourceV) -> StoredApiUserV7,
{
    let Ok(tree) = BPlusTree::<String, SourceV>::load(db_path) else {
        return Ok(false);
    };
    let mut v7_tree = BPlusTree::new();
    for (key, user) in &tree {
        v7_tree.insert(key.clone(), map(user));
    }
    v7_tree.store(db_path)?;
    create_user_db_merge_guard(merge_guard_path)?;
    Ok(true)
}

/// Migrates the user database file from V1-V6 schema to V7 (current) in
/// place and creates a merge-guard file so config-driven merges are skipped
/// until the operator explicitly removes it.
///
/// Returns `true` when a migration was performed, `false` when the file was
/// already in V7 format or did not exist.
fn migrate_user_db_schema(db_path: &Path, merge_guard_path: &Path) -> io::Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }

    let storage_version = typed_migration::storage_version(db_path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "user database is not a B+Tree"))?;
    if storage_version <= 2 {
        if typed_migration::migrate_v2_typed::<String, StoredApiUserV7>(db_path).is_ok() {
            return Ok(false);
        }
        if migrate_legacy_user_schema::<StoredApiUserV6, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v6(&user)
        })? {
            return Ok(true);
        }
        if migrate_legacy_user_schema::<StoredApiUserV5, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v5(&user)
        })? {
            return Ok(true);
        }
        if migrate_legacy_user_schema::<StoredApiUserV4, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v4(&user)
        })? {
            return Ok(true);
        }
        if migrate_legacy_user_schema::<StoredApiUserV3, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v3(&user)
        })? {
            return Ok(true);
        }
        if migrate_legacy_user_schema::<StoredApiUserV2, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v2(&user)
        })? {
            return Ok(true);
        }
        if migrate_legacy_user_schema::<StoredApiUserV1, _>(db_path, merge_guard_path, |user| {
            StoredApiUserV7::from_v1(&user)
        })? {
            return Ok(true);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Legacy user DB at '{}' could not be read as V1, V2, V3, V4, V5, V6, or V7", db_path.display()),
        ));
    }
    if storage_version != STORAGE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Unsupported user B+Tree storage version {storage_version} in {}", db_path.display()),
        ));
    }

    if BPlusTree::<String, StoredApiUserV7>::load(db_path).is_ok() {
        return Ok(false);
    }
    if migrate_current_user_schema::<StoredApiUserV6, _>(db_path, merge_guard_path, StoredApiUserV7::from_v6)? {
        return Ok(true);
    }
    if migrate_current_user_schema::<StoredApiUserV5, _>(db_path, merge_guard_path, StoredApiUserV7::from_v5)? {
        return Ok(true);
    }
    if migrate_current_user_schema::<StoredApiUserV4, _>(db_path, merge_guard_path, StoredApiUserV7::from_v4)? {
        return Ok(true);
    }
    if migrate_current_user_schema::<StoredApiUserV3, _>(db_path, merge_guard_path, StoredApiUserV7::from_v3)? {
        return Ok(true);
    }
    if migrate_current_user_schema::<StoredApiUserV2, _>(db_path, merge_guard_path, StoredApiUserV7::from_v2)? {
        return Ok(true);
    }
    if migrate_current_user_schema::<StoredApiUserV1, _>(db_path, merge_guard_path, StoredApiUserV7::from_v1)? {
        return Ok(true);
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "User DB at '{}' exists but could not be read as V1, V2, V3, V4, V5, V6, or V7 format",
            db_path.display()
        ),
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AllStartupMigrationStats {
    pub bplustree: BPlusTreeMigrationStats,
    pub user_db_migrated: bool,
}

/// Runs all startup migrations in sequence:
/// 1. B+Tree storage-format migration (V1 -> current binary format)
/// 2. User DB schema migration (V1-V6 -> V7 `MessagePack` layout)
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
    let user_db_migrated = migrate_user_db_schema(&user_db_path, &merge_guard_path)?;
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
                info!("User DB schema migrated to V6");
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
    use shared::model::{EpgProgramme, LiveStreamProperties};
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
    fn epg_programme_accepts_legacy_messagepack_without_catchup_id_field() {
        let encoded = to_vec(&LegacyEpgProgramme {
            start: 100,
            stop: 200,
            title: Some("Programme".to_string()),
            desc: Some("Description".to_string()),
        })
        .expect("legacy programme should encode");

        let decoded: EpgProgramme = from_slice(&encoded).expect("legacy programme should decode");
        assert_eq!(decoded.start, 100);
        assert_eq!(decoded.stop, 200);
        assert_eq!(decoded.title.as_deref(), Some("Programme"));
        assert_eq!(decoded.desc.as_deref(), Some("Description"));
        assert!(decoded.catchup_id.is_none());
        assert!(decoded.categories.is_empty());
        assert!(!decoded.is_live);
        assert!(!decoded.is_new);
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
        assert_eq!(BPlusTreeStartupMigrator::migration_kind(&temp.path().join("input_news/vod.db"), &roots), None);
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
}
