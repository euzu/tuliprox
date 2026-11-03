//! Read-only exports and validation, followed by an exclusive all-or-nothing repair.
use super::{
    classify_user_record, database_root, invalid, load_user_records_read_only, rebuild_from_live_records,
    rebuild_from_verified_history, reject_symlink, related_database_files, user_storage_version, ApiUserRepository,
    StoredApiUserV7, UserDbRepairReport, UserRecordDecode, RECOVERY_DIRECTORY,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use tuliprox_btree::{sidecar_lock_path, RecoveryMaintenanceGuard, RecoveryPaths};

const EXPORT_VERSION: u32 = 1;

/// A repair document contains only the fields needed to identify users and
/// resolve their optional strings. Authentication and other fields stay in the DB.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserDbExport {
    pub format_version: u32,
    pub source_fingerprint: String,
    pub users: Vec<UserDbExportUser>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserDbExportUser {
    pub user_key: String,
    pub username: String,
    pub target: String,
    pub plan: Option<String>,
    pub filter: Option<String>,
    pub unassigned_value: Option<String>,
}

#[derive(Debug)]
pub struct UserDbCheckReport {
    pub records: usize,
    pub resolved_assignments: usize,
    pub recovery_verified: bool,
}

struct Snapshot {
    export: UserDbExport,
    users: BTreeMap<String, StoredApiUserV7>,
    unassigned: BTreeMap<String, String>,
    verified_history_records: Option<u64>,
}

/// Reads all repair fields, or a selected user, without opening a mutable journal.
pub fn get_user_database(db_path: &Path, user_key: Option<&str>) -> io::Result<UserDbExport> {
    let mut export = read_snapshot(db_path)?.export;
    if let Some(key) = user_key {
        export.users.retain(|user| user.user_key == key);
        if export.users.is_empty() {
            return Err(invalid("the requested user key does not exist"));
        }
    }
    Ok(export)
}

/// Writes a new private export file. No existing file or database artifact may
/// be overwritten, and exports cannot be placed inside the recovery directory.
pub fn write_user_database_export(db_path: &Path, export: &UserDbExport, output: &Path) -> io::Result<()> {
    let destination = output_identity(output)?;
    let root = database_root(db_path)?;
    let recovery = output_identity(&root.join(RECOVERY_DIRECTORY))?;
    let mut reserved = related_database_files(db_path).to_vec();
    reserved.extend([
        sidecar_lock_path(db_path),
        super::user_db_merge_guard_path(root),
        root.join(".api_user_recovery.maintenance.lock"),
    ]);
    if destination.starts_with(recovery)
        || reserved.iter().any(|path| output_identity(path).is_ok_and(|identity| identity == destination))
    {
        return Err(invalid("output must be outside the database artifacts and recovery directory"));
    }
    let mut encoded = serde_json::to_vec_pretty(export).map_err(io::Error::other)?;
    encoded.push(b'\n');
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output)?;
    file.write_all(&encoded)?;
    file.sync_all()
}

/// Validates the whole source and optional input without replaying a WAL,
/// creating a backup, or changing database or recovery files.
pub fn check_user_database(db_path: &Path, input: Option<&Path>) -> io::Result<UserDbCheckReport> {
    let snapshot = read_snapshot(db_path)?;
    let document = input.map(read_document).transpose()?;
    let users = validate_document(&snapshot, document.as_ref())?;
    Ok(UserDbCheckReport {
        records: users.len(),
        resolved_assignments: snapshot.unassigned.len(),
        recovery_verified: snapshot.verified_history_records.is_some(),
    })
}

/// Validates and publishes an entire repair while holding exclusive access.
/// A separate earlier check is never trusted in place of this validation.
pub fn set_user_database(db_path: &Path, input: &Path) -> io::Result<UserDbRepairReport> {
    fs::metadata(db_path)?;
    let root = database_root(db_path)?;
    let maintenance = RecoveryMaintenanceGuard::try_acquire(&RecoveryPaths {
        database: db_path.to_path_buf(),
        directory: root.join(RECOVERY_DIRECTORY),
    })?;
    let snapshot = read_snapshot(db_path)?;
    let document = read_document(input)?;
    let users = validate_document(&snapshot, Some(&document))?;
    if let Some(records) = snapshot.verified_history_records {
        rebuild_from_verified_history(db_path, root, records, &maintenance)
    } else {
        rebuild_from_live_records(db_path, root, &users)
    }
}

fn read_snapshot(db_path: &Path) -> io::Result<Snapshot> {
    let storage_version = user_storage_version(db_path)?;
    let fingerprint = source_fingerprint(db_path)?;
    let mut users = BTreeMap::new();
    let mut unassigned = BTreeMap::new();
    let verified_history_records = if let Ok((report, records)) =
        ApiUserRepository::read_verified_history(db_path, database_root(db_path)?)
    {
        for (key, record) in records {
            let bytes = rmp_serde::to_vec(&record).map_err(io::Error::other)?;
            let user = rmp_serde::from_slice(&bytes)
                .map_err(|_| invalid("verified recovery record cannot be represented in the current user schema"))?;
            users.insert(key, user);
        }
        Some(report.live_records)
    } else {
        let records = load_user_records_read_only(db_path, storage_version)?;
        for (key, record) in &records {
            let user = match classify_user_record(record.clone())? {
                UserRecordDecode::Complete(user) => user,
                UserRecordDecode::Ambiguous { user, value } => {
                    unassigned.insert(key.clone(), value);
                    user
                }
            };
            users.insert(key.clone(), user);
        }
        None
    };
    if fingerprint != source_fingerprint(db_path)? {
        return Err(invalid("the source changed while reading; stop every process using this database and retry"));
    }
    let export = UserDbExport {
        format_version: EXPORT_VERSION,
        source_fingerprint: fingerprint,
        users: users
            .iter()
            .map(|(key, user)| UserDbExportUser {
                user_key: key.clone(),
                username: user.username.clone(),
                target: user.target.clone(),
                plan: user.plan.clone(),
                filter: user.filter.clone(),
                unassigned_value: unassigned.get(key).cloned(),
            })
            .collect(),
    };
    Ok(Snapshot { export, users, unassigned, verified_history_records })
}

fn read_document(path: &Path) -> io::Result<UserDbExport> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| invalid("invalid user DB input file; use an export from --user-db-get"))
}

fn validate_document(
    snapshot: &Snapshot,
    document: Option<&UserDbExport>,
) -> io::Result<BTreeMap<String, StoredApiUserV7>> {
    let Some(document) = document else {
        if !snapshot.unassigned.is_empty() {
            return Err(invalid(
                "unassigned values require an edited export; run --user-db-get with --output users.json",
            ));
        }
        return Ok(snapshot.users.clone());
    };
    if document.format_version != EXPORT_VERSION {
        return Err(invalid("unsupported user DB export format version"));
    }
    if document.source_fingerprint != snapshot.export.source_fingerprint {
        return Err(invalid("the export does not match the current database or recovery history; generate a fresh export with --user-db-get"));
    }
    if document.users.len() != snapshot.users.len() {
        return Err(invalid("input must contain every user exactly once; partial exports cannot be imported"));
    }
    let mut resolved = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for entry in &document.users {
        if !seen.insert(&entry.user_key) {
            return Err(invalid("input contains a duplicate user key"));
        }
        let original =
            snapshot.users.get(&entry.user_key).ok_or_else(|| invalid("input contains an unknown user key"))?;
        if entry.username != original.username || entry.target != original.target {
            return Err(invalid("user_key, username and target are read-only reference fields"));
        }
        let mut user = original.clone();
        if let Some(value) = snapshot.unassigned.get(&entry.user_key) {
            if entry.unassigned_value.is_some() {
                return Err(invalid(format!(
                    "user key {} still has an unassigned_value; assign its unchanged value to plan or filter and set unassigned_value to null",
                    super::json_string(&entry.user_key)
                )));
            }
            match (entry.plan.as_deref(), entry.filter.as_deref()) {
                (Some(plan), None) if plan == value => user.plan.clone_from(&entry.plan),
                (None, Some(filter)) if filter == value => user.filter.clone_from(&entry.filter),
                _ => return Err(invalid("assign the unchanged unassigned value to exactly one field: plan or filter")),
            }
        } else if entry.unassigned_value.is_some() || entry.plan != original.plan || entry.filter != original.filter {
            return Err(invalid(
                "known plan/filter fields must remain unchanged; only unassigned values require repair",
            ));
        }
        resolved.insert(entry.user_key.clone(), user);
    }
    Ok(resolved)
}

fn output_identity(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| invalid("output has no file name"))?;
    Ok(fs::canonicalize(parent)?.join(name))
}

fn source_fingerprint(db_path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("tuliprox user DB repair export v1");
    hash_file(&mut hasher, b"database", db_path)?;
    let recovery = database_root(db_path)?.join(RECOVERY_DIRECTORY);
    let has_recovery = match fs::symlink_metadata(&recovery) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if has_recovery {
        let mut files = Vec::new();
        collect_history_files(&recovery, &mut files)?;
        files.sort();
        for file in files {
            let relative = file.strip_prefix(&recovery).map_err(io::Error::other)?;
            let label = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            hash_file(&mut hasher, label.as_bytes(), &file)?;
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_history_files(directory: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    reject_symlink(directory)?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("._") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_history_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        } else {
            return Err(invalid("recovery history contains a symbolic link or unsupported file type"));
        }
    }
    Ok(())
}

fn hash_file(hasher: &mut blake3::Hasher, label: &[u8], path: &Path) -> io::Result<()> {
    reject_symlink(path)?;
    hasher.update(&u64::try_from(label.len()).map_err(io::Error::other)?.to_le_bytes());
    hasher.update(label);
    let mut file = fs::File::open(path)?;
    hasher.update(&file.metadata()?.len().to_le_bytes());
    let mut buffer = vec![0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..count]);
    }
}
