//! Explicit offline inspection and repair for historical User API records.
//!
//! Normal startup must never guess whether a compact positional string was a
//! `plan` or a `filter`. This module gives an operator a separate, auditable
//! path: verified field-named history wins; otherwise every ambiguous database
//! key needs an explicit assignment before a staged replacement is published.

mod commands;
use crate::{
    api_user_recovery::{classify_user_record, ApiUserRecovery, ApiUserRepository, StoredApiUserV7, UserRecordDecode},
    startup_migration::{
        create_user_db_merge_guard, load_user_records, load_user_records_read_only, user_db_merge_guard_path,
    },
    storage_const::API_USER_REPAIR_BACKUP_PREFIX,
    user_repository::StoredProxyUserCredentials,
};
pub use commands::{
    check_user_database, get_user_database, set_user_database, write_user_database_export, UserDbCheckReport,
    UserDbExport, UserDbExportUser,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use tuliprox_btree::{
    get_file_path_for_db_index, publish_staged_database, typed_migration, BPlusTree, BPlusTreeMetadata,
    BPlusTreeStagingArtifacts, RecoveryMaintenanceGuard, RecoveryOpenAction, RecoveryPaths, STORAGE_VERSION,
};

const RECOVERY_DIRECTORY: &str = "api_user_recovery";

pub const USER_DB_REPAIR_GUIDE: &str =
    "https://github.com/euzu/tuliprox/blob/develop/docs/src/operations-debugging.md#api-user-database-offline-repair";

/// Quote paths in the POSIX-shell command examples without interpreting their contents.
fn user_db_shell_path(path: &Path) -> String { format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'")) }

pub(crate) fn user_db_repair_error(db_path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "API user database {}: {error}\n\
             Stop all Tuliprox processes using this database and back up the complete configuration directory.\n\
             Export offline: tuliprox --user-db-get {} --output users.json\n\
             Resolve unassigned_value entries, then run --user-db-check with --input users.json before --user-db-set.\n\
             Run the CLI with a native binary or, for Docker installations, in a one-off container using the service's \
             image and config volume while the service is stopped.\n\
             Do not delete the user database or api_user_recovery, and do not guess plan/filter assignments.\n\
             Repair guide: {USER_DB_REPAIR_GUIDE}",
            serde_json::to_string(&db_path.to_string_lossy()).unwrap_or_default(),
            user_db_shell_path(db_path),
        ),
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UserDbOptionalField {
    Plan,
    Filter,
}

pub type UserDbRepairDecisions = BTreeMap<String, UserDbOptionalField>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UserDbRepairDecision {
    user_key: String,
    field: Option<UserDbOptionalField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<UserDbRepairDecisionSource>,
}

/// Read-only reference data for an ambiguous field assignment. Exported only
/// when an operator explicitly requests a decision template.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UserDbRepairDecisionSource {
    pub username: String,
    pub target: String,
    pub stored_value: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserDbHistoryInspection {
    Verified { database_revision: u64, recovery_revision: u64, records: u64 },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserDbInspectionReport {
    pub path: PathBuf,
    pub storage_version: u32,
    pub records: usize,
    pub live_records_inspected: bool,
    pub unambiguous_records: usize,
    pub ambiguous_user_keys: Vec<String>,
    pub ambiguous_user_sources: BTreeMap<String, UserDbRepairDecisionSource>,
    pub history: UserDbHistoryInspection,
}

/// User-facing next steps; keeps credentials and compact field values out of stdout.
pub fn user_db_repair_advice(report: &UserDbInspectionReport, template_path: Option<&Path>) -> String {
    let database = user_db_shell_path(&report.path);
    let next = if matches!(report.history, UserDbHistoryInspection::Verified { .. }) {
        format!(
            "Automatic Repair:   AVAILABLE from verified matching recovery history; do not supply manual decisions.\n\
             Next step: tuliprox --repair-user-db {database}"
        )
    } else if report.ambiguous_user_keys.is_empty() {
        format!(
            "Automatic Repair:   AVAILABLE from fully validated live records; fresh recovery history will be created.\n\
             Next step: tuliprox --repair-user-db {database}"
        )
    } else {
        let default_path = report.path.with_file_name("api_user-repair-decisions.json");
        let template = user_db_shell_path(template_path.unwrap_or(&default_path));
        format!(
            "Automatic Repair:   BLOCKED: field names were lost for the listed users.\n\
             Create a decision template:\n\
               tuliprox --inspect-user-db {database} --user-db-decision-template {template}\n\
             The template includes each user's username, target and stored value under source. Leave source unchanged \
             and set only field to plan or filter using a reliable field-named source for that \
             user's database state, such as a matching export or provisioning record. An api-proxy.yml file can be \
             stale or omit database-managed users; a plan name or valid filter syntax is not proof.\n\
             Without a reliable source, lossless reconstruction is impossible. Restore a complete matching backup, \
             or deliberately establish a new policy for the affected users; do not guess.\n\
             After resolving every decision:\n\
               tuliprox --repair-user-db {database} --user-db-decisions {template}"
        )
    };
    format!(
        "{next}\n\
         Stop every process using this database and back up the complete configuration directory before repair.\n\
         Docker: run these options in a one-off container with the service stopped (see guide).\n\
         Repair guide: {USER_DB_REPAIR_GUIDE}"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserDbRepairSource {
    VerifiedHistory,
    ExplicitLiveRecords,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserDbRepairReport {
    pub path: PathBuf,
    pub records: usize,
    pub source: UserDbRepairSource,
    pub backup_path: PathBuf,
}

/// Reads field choices for programmatic callers. File-backed CLI repairs use
/// `repair_user_database_with_decision_file` to retain and check source context.
pub fn load_user_db_repair_decisions(path: &Path) -> io::Result<UserDbRepairDecisions> {
    read_user_db_decision_file(path).map(|(decisions, _)| decisions)
}

fn read_user_db_decision_file(
    path: &Path,
) -> io::Result<(UserDbRepairDecisions, BTreeMap<String, UserDbRepairDecisionSource>)> {
    let encoded = fs::read(path)?;
    let entries: Vec<UserDbRepairDecision> = serde_json::from_slice(&encoded)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("invalid decision file: {error}")))?;
    let mut decisions = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for entry in entries {
        let field = entry.field.ok_or_else(|| {
            invalid(format!(
                "user key {} has an unresolved decision (field is null or missing); set field to plan or filter only \
                 after checking a reliable field-named source for this user's database state. Without one, a lossless \
                 reconstruction is impossible; restore a complete matching backup or deliberately establish a new policy",
                json_string(&entry.user_key)
            ))
        })?;
        if decisions.insert(entry.user_key.clone(), field).is_some() {
            return Err(invalid("decision file assigns the same user key more than once"));
        }
        if let Some(source) = entry.source {
            sources.insert(entry.user_key, source);
        }
    }
    Ok((decisions, sources))
}

/// Prefills a new decision file with the surviving user context and value.
/// The field assignment remains unresolved; existing files are never replaced.
pub fn write_user_db_decision_template(report: &UserDbInspectionReport, path: &Path) -> io::Result<()> {
    if matches!(report.history, UserDbHistoryInspection::Verified { .. }) {
        return Err(invalid("verified matching recovery history takes precedence; no decision template is needed"));
    }
    if report.ambiguous_user_keys.is_empty() {
        return Err(invalid("all live records are unambiguous; no decision template is needed"));
    }
    let entries: Vec<_> = report
        .ambiguous_user_keys
        .iter()
        .map(|key| {
            let source = report.ambiguous_user_sources.get(key).cloned().ok_or_else(|| {
                invalid("inspection lacks reference data for an ambiguous user; inspect the database again")
            })?;
            Ok(UserDbRepairDecision { user_key: key.clone(), field: None, source: Some(source) })
        })
        .collect::<io::Result<_>>()?;
    let mut encoded = serde_json::to_vec_pretty(&entries).map_err(io::Error::other)?;
    encoded.push(b'\n');
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                error.kind(),
                "decision template already exists; edit it or choose a new path; it was not overwritten",
            )
        } else {
            error
        }
    })?;
    file.write_all(&encoded)?;
    file.sync_all()
}

/// Inspects the complete database and matching recovery history without
/// changing either one.
pub fn inspect_user_database(db_path: &Path) -> io::Result<UserDbInspectionReport> {
    let storage_version = user_storage_version(db_path)?;
    let root = database_root(db_path)?;
    let history = match ApiUserRepository::verify_history(db_path, root) {
        Ok(report) => UserDbHistoryInspection::Verified {
            database_revision: report.database_revision,
            recovery_revision: report.recovery_revision,
            records: report.live_records,
        },
        Err(error) => UserDbHistoryInspection::Unavailable { reason: error.to_string() },
    };
    let live_scan = (|| {
        let records = load_user_records_read_only(db_path, storage_version)?;
        let mut ambiguous_user_keys = Vec::new();
        let mut ambiguous_user_sources = BTreeMap::new();
        for (key, value) in &records {
            if let UserRecordDecode::Ambiguous { user, value: stored_value } = classify_user_record(value.clone())? {
                ambiguous_user_keys.push(key.clone());
                ambiguous_user_sources.insert(
                    key.clone(),
                    user_db_decision_source(records.get_metadata(), key, value, &user, stored_value)?,
                );
            }
        }
        Ok::<_, io::Error>((records.len(), ambiguous_user_keys, ambiguous_user_sources))
    })();
    let (total, ambiguous_user_keys, ambiguous_user_sources, live_records_inspected) = match live_scan {
        Ok((total, keys, sources)) => (total, keys, sources, true),
        Err(error) => match &history {
            UserDbHistoryInspection::Verified { records, .. } => (
                usize::try_from(*records).map_err(|_| invalid("recovery record count exceeds usize"))?,
                Vec::new(),
                BTreeMap::new(),
                false,
            ),
            UserDbHistoryInspection::Unavailable { .. } => return Err(error),
        },
    };
    Ok(UserDbInspectionReport {
        path: db_path.to_path_buf(),
        storage_version,
        records: total,
        live_records_inspected,
        unambiguous_records: total.saturating_sub(ambiguous_user_keys.len()),
        ambiguous_user_keys,
        ambiguous_user_sources,
        history,
    })
}

fn user_db_decision_source(
    metadata: &BPlusTreeMetadata,
    key: &str,
    record: &serde_json::Value,
    user: &StoredApiUserV7,
    stored_value: String,
) -> io::Result<UserDbRepairDecisionSource> {
    // Bind the choice to the exact record and, for tracked databases, its
    // database identity and revision. The digest never exposes credentials.
    let encoded = match metadata {
        BPlusTreeMetadata::Empty => rmp_serde::to_vec(&("empty", key, record)),
        BPlusTreeMetadata::TargetIdMapping(id) => rmp_serde::to_vec(&("target", id, key, record)),
        BPlusTreeMetadata::Recovery(identity) => rmp_serde::to_vec(&(
            "recovery",
            identity.database_id,
            identity.schema_fingerprint,
            identity.schema_version,
            identity.applied_revision,
            key,
            record,
        )),
    }
    .map_err(io::Error::other)?;
    Ok(UserDbRepairDecisionSource {
        username: user.username.clone(),
        target: user.target.clone(),
        stored_value,
        fingerprint: blake3::hash(&encoded).to_hex().to_string(),
    })
}

/// Repairs a User API database while Tuliprox is stopped.
///
/// A matching verified history is always preferred. Without one, all live
/// records must decode and every compact single string must have an explicit
/// `plan`/`filter` decision. The source database and previous history are
/// archived before a staged, fully verified replacement is published.
pub fn repair_user_database(db_path: &Path, decisions: &UserDbRepairDecisions) -> io::Result<UserDbRepairReport> {
    repair_user_database_inner(db_path, decisions, &BTreeMap::new())
}

/// Imports the edited template and validates its reference data under the same
/// exclusive lease that protects the repair. Older field-only lists are valid.
pub fn repair_user_database_with_decision_file(db_path: &Path, path: &Path) -> io::Result<UserDbRepairReport> {
    let (decisions, sources) = read_user_db_decision_file(path)?;
    repair_user_database_inner(db_path, &decisions, &sources)
}

fn repair_user_database_inner(
    db_path: &Path,
    decisions: &UserDbRepairDecisions,
    sources: &BTreeMap<String, UserDbRepairDecisionSource>,
) -> io::Result<UserDbRepairReport> {
    let root = database_root(db_path)?;
    fs::metadata(db_path)?;
    // Fence both existing journal handles and new opens before taking a
    // snapshot or touching recovery. Hold through final verification/guard.
    let maintenance = RecoveryMaintenanceGuard::try_acquire(&RecoveryPaths {
        database: db_path.to_path_buf(),
        directory: root.join(RECOVERY_DIRECTORY),
    })?;
    let inspection = inspect_user_database(db_path)?;
    if let UserDbHistoryInspection::Verified { records, .. } = inspection.history {
        if !decisions.is_empty() {
            return Err(invalid(
                "a verified matching recovery history is available; remove manual decisions because history takes precedence",
            ));
        }
        return rebuild_from_verified_history(db_path, root, records, &maintenance);
    }

    let storage_version = user_storage_version(db_path)?;
    let records = load_user_records(db_path, storage_version)?;
    let users = resolve_live_records(&records, decisions, sources)?;
    rebuild_from_live_records(db_path, root, &users)
}

fn rebuild_from_verified_history(
    db_path: &Path,
    root: &Path,
    records: u64,
    maintenance: &RecoveryMaintenanceGuard,
) -> io::Result<UserDbRepairReport> {
    let backup = create_repair_archive(db_path, true)?;
    let result = (|| {
        let report = ApiUserRepository::rebuild_with_maintenance(db_path, root, maintenance)?;
        if report.action != RecoveryOpenAction::Rebuilt {
            return Err(invalid("verified history did not rebuild the user database"));
        }
        verify_published_database(db_path, records)?;
        create_user_db_merge_guard(&user_db_merge_guard_path(root))?;
        Ok(UserDbRepairReport {
            path: db_path.to_path_buf(),
            records: usize::try_from(records).map_err(|_| invalid("recovery record count exceeds usize"))?,
            source: UserDbRepairSource::VerifiedHistory,
            backup_path: backup.clone(),
        })
    })();
    result.map_err(|error: io::Error| with_backup(&error, &backup))
}

fn resolve_live_records(
    records: &BPlusTree<String, serde_json::Value>,
    decisions: &UserDbRepairDecisions,
    sources: &BTreeMap<String, UserDbRepairDecisionSource>,
) -> io::Result<BTreeMap<String, StoredApiUserV7>> {
    let mut users = BTreeMap::new();
    let mut resolved_keys = BTreeSet::new();
    for (key, record) in records {
        let user = match classify_user_record(record.clone())? {
            UserRecordDecode::Complete(user) => user,
            UserRecordDecode::Ambiguous { mut user, value } => {
                if let Some(expected) = sources.get(key) {
                    let actual = user_db_decision_source(records.get_metadata(), key, record, &user, value.clone())?;
                    if &actual != expected {
                        return Err(invalid(format!(
                            "reference data for user key {} does not match the current database; inspect again, \
                             generate a new decision template and edit only field; the source database has not been replaced",
                            json_string(key)
                        )));
                    }
                }
                let field = decisions.get(key).ok_or_else(|| {
                    invalid(format!(
                        "user key {} has an ambiguous compact string; add an explicit plan/filter decision",
                        json_string(key)
                    ))
                })?;
                match field {
                    UserDbOptionalField::Plan => user.plan = Some(value),
                    UserDbOptionalField::Filter => user.filter = Some(value),
                }
                resolved_keys.insert(key.clone());
                user
            }
        };
        users.insert(key.clone(), user);
    }
    if let Some(unused) = decisions.keys().find(|key| !resolved_keys.contains(*key)) {
        return Err(invalid(format!(
            "decision for user key {} is unused; decisions are accepted only for reported ambiguous records",
            json_string(unused)
        )));
    }
    Ok(users)
}

fn rebuild_from_live_records(
    db_path: &Path,
    root: &Path,
    users: &BTreeMap<String, StoredApiUserV7>,
) -> io::Result<UserDbRepairReport> {
    let token = uuid::Uuid::new_v4();
    let name = db_path.file_name().ok_or_else(|| invalid("user database has no file name"))?;
    let staging_db = root.join(format!(".{}.repair-{token}.tmp", name.to_string_lossy()));
    let staging_root = root.join(format!(".api-user-repair-{token}"));
    let staging_history = staging_root.join(RECOVERY_DIRECTORY);
    let artifacts = BPlusTreeStagingArtifacts::new(db_path, &staging_db)?;

    let staged = (|| {
        let mut tree = BPlusTree::<String, StoredProxyUserCredentials>::new();
        let mut expected = BTreeMap::new();
        for (key, user) in users {
            let normalized = ApiUserRecovery::from(user);
            tree.insert(key.clone(), StoredProxyUserCredentials::from(&normalized));
            expected.insert(key.clone(), normalized);
        }
        tree.store(&staging_db)?;
        let (mut repository, report) = ApiUserRepository::open(&staging_db, &staging_root)?;
        if report.action != RecoveryOpenAction::Adopted {
            return Err(invalid("staged user database was not adopted into fresh recovery history"));
        }
        let actual = repository.load()?;
        if actual.len() != expected.len() {
            return Err(invalid("staged user database record count did not verify"));
        }
        for (key, user) in actual {
            if expected.get(&key) != Some(&ApiUserRecovery::from(&user)) {
                return Err(invalid("staged user database fields did not verify"));
            }
        }
        let verification = ApiUserRepository::verify_history(&staging_db, &staging_root)?;
        if verification.live_records
            != u64::try_from(expected.len()).map_err(|_| invalid("record count exceeds u64"))?
        {
            return Err(invalid("fresh recovery history record count did not verify"));
        }
        let strict = BPlusTree::<String, StoredApiUserV7>::load(&staging_db)?;
        if strict.len() != expected.len() {
            return Err(invalid("staged user database is not a complete V7 database"));
        }
        Ok(())
    })();
    if let Err(error) = staged {
        cleanup_staging(&artifacts, &staging_root);
        return Err(error);
    }

    let backup = match create_repair_archive(db_path, false) {
        Ok(path) => path,
        Err(error) => {
            cleanup_staging(&artifacts, &staging_root);
            return Err(error);
        }
    };
    let published = publish_fresh_recovery(db_path, &staging_db, root, &staging_history, &backup);
    cleanup_staging(&artifacts, &staging_root);
    if let Err(error) = published {
        return Err(with_backup(&error, &backup));
    }
    let count = u64::try_from(users.len()).map_err(|_| invalid("record count exceeds u64"))?;
    let verified = (|| {
        verify_published_database(db_path, count)?;
        create_user_db_merge_guard(&user_db_merge_guard_path(root))
    })();
    if let Err(error) = verified {
        return Err(with_backup(&error, &backup));
    }
    Ok(UserDbRepairReport {
        path: db_path.to_path_buf(),
        records: users.len(),
        source: UserDbRepairSource::ExplicitLiveRecords,
        backup_path: backup,
    })
}

fn publish_fresh_recovery(
    db_path: &Path,
    staging_db: &Path,
    root: &Path,
    staging_history: &Path,
    backup: &Path,
) -> io::Result<()> {
    let recovery = root.join(RECOVERY_DIRECTORY);
    let archived_recovery = backup.join(RECOVERY_DIRECTORY);
    let had_recovery = recovery.exists();
    if had_recovery {
        reject_symlink(&recovery)?;
        fs::rename(&recovery, &archived_recovery)?;
    }
    if let Err(error) = fs::rename(staging_history, &recovery) {
        if had_recovery {
            let _ = fs::rename(&archived_recovery, &recovery);
        }
        return Err(error);
    }
    if let Err(error) = publish_staged_database::<String, StoredProxyUserCredentials>(staging_db, db_path) {
        let staged_restore = staging_history.to_path_buf();
        let _ = fs::rename(&recovery, &staged_restore);
        if had_recovery {
            if let Err(rollback) = fs::rename(&archived_recovery, &recovery) {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "database publication failed ({error}) and recovery rollback failed ({rollback}); restore the archive manually"
                    ),
                ));
            }
        }
        return Err(error);
    }
    Ok(())
}

fn verify_published_database(db_path: &Path, expected_records: u64) -> io::Result<()> {
    let strict = BPlusTree::<String, StoredApiUserV7>::load(db_path)?;
    if u64::try_from(strict.len()).map_err(|_| invalid("record count exceeds u64"))? != expected_records {
        return Err(invalid("published user database record count did not verify"));
    }
    let root = database_root(db_path)?;
    let history = ApiUserRepository::verify_history(db_path, root)?;
    if history.live_records != expected_records || history.database_revision != history.recovery_revision {
        return Err(invalid("published user database and recovery history do not agree"));
    }
    Ok(())
}

fn create_repair_archive(db_path: &Path, include_recovery: bool) -> io::Result<PathBuf> {
    let root = database_root(db_path)?;
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let archive = root.join(format!("{API_USER_REPAIR_BACKUP_PREFIX}{timestamp}_{}", uuid::Uuid::new_v4()));
    fs::create_dir(&archive)?;
    for source in related_database_files(db_path) {
        if source.exists() {
            reject_symlink(&source)?;
            let name = source.file_name().ok_or_else(|| invalid("database artifact has no file name"))?;
            fs::copy(&source, archive.join(name))?;
        }
    }
    let guard = user_db_merge_guard_path(root);
    if guard.exists() {
        reject_symlink(&guard)?;
        let name = guard.file_name().ok_or_else(|| invalid("merge guard has no file name"))?;
        fs::copy(&guard, archive.join(name))?;
    }
    if include_recovery {
        let recovery = root.join(RECOVERY_DIRECTORY);
        if recovery.exists() {
            copy_directory(&recovery, &archive.join(RECOVERY_DIRECTORY))?;
        }
    }
    Ok(archive)
}

fn related_database_files(db_path: &Path) -> [PathBuf; 4] {
    [
        db_path.to_path_buf(),
        get_file_path_for_db_index(db_path),
        append_suffix(db_path, ".wal"),
        append_suffix(db_path, ".wal.tmp"),
    ]
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map_or_else(OsString::new, OsString::from);
    name.push(suffix);
    path.with_file_name(name)
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    reject_symlink(source)?;
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(invalid("recovery archive refuses symbolic links"));
        }
        if file_type.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(source_path, destination_path)?;
        } else {
            return Err(invalid("recovery archive contains an unsupported file type"));
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("user database repair refuses symbolic links"));
    }
    Ok(())
}

fn cleanup_staging(artifacts: &BPlusTreeStagingArtifacts, staging_root: &Path) {
    if let Err(error) = artifacts.remove_owned_staging_artifacts() {
        log::warn!("Could not remove user repair staging artifacts: {error}");
    }
    if let Err(error) = fs::remove_dir_all(staging_root) {
        if error.kind() != io::ErrorKind::NotFound {
            log::warn!("Could not remove user repair staging directory: {error}");
        }
    }
}

fn user_storage_version(db_path: &Path) -> io::Result<u32> {
    let version = typed_migration::storage_version(db_path)?.ok_or_else(|| invalid("user database is not a B+Tree"))?;
    if !matches!(version, 1 | 2) && version != STORAGE_VERSION {
        return Err(invalid("unsupported user B+Tree storage version"));
    }
    Ok(version)
}

fn database_root(db_path: &Path) -> io::Result<&Path> {
    db_path.parent().ok_or_else(|| invalid("user database has no parent directory"))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid-user-key>\"".to_string())
}

fn with_backup(error: &io::Error, backup: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{error}; automatic repair archive: {}", backup.display()))
}

fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }
