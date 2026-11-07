//! Explicit offline repair for historical User API records.
//!
//! Normal startup must never guess whether a compact positional string was a
//! `plan` or a `filter`. This module gives an operator a separate, auditable
//! path: verified field-named history wins; otherwise an export names every
//! ambiguous value and requires an explicit assignment before a staged
//! replacement is published.

mod commands;
use crate::{
    api_user_recovery::{ApiUserRecovery, ApiUserRepository, StoredApiUserV7},
    startup_migration::{create_user_db_merge_guard, load_user_records, user_db_merge_guard_path},
    storage_const::API_USER_REPAIR_BACKUP_PREFIX,
    user_repository::StoredProxyUserCredentials,
};
pub use commands::{
    check_user_database, get_user_database, set_user_database, write_user_database_export, UserDbCheckReport,
    UserDbExport, UserDbExportUser,
};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
use tuliprox_btree::{
    get_file_path_for_db_index, publish_staged_database, typed_migration, BPlusTree, BPlusTreeStagingArtifacts,
    RecoveryMaintenanceGuard, RecoveryOpenAction, STORAGE_VERSION,
};

const RECOVERY_DIRECTORY: &str = "api_user_recovery";

pub const USER_DB_REPAIR_GUIDE: &str =
    "https://github.com/euzu/tuliprox/blob/develop/docs/src/operations-debugging.md#api-user-database-offline-repair";

/// Quote paths in the POSIX-shell command examples without interpreting their contents.
fn user_db_shell_path(path: &Path) -> String { format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'")) }

/// Appends the offline repair workflow to a user database error.
pub(crate) fn user_db_repair_error(db_path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "API user database {}: {error}\n\
             Stop all Tuliprox processes using this database and back up the complete configuration directory.\n\
             Export offline: tuliprox --user-db-get {} --user-db-output users.json\n\
             Resolve unassigned_value entries, then run --user-db-check with --user-db-input users.json before --user-db-set.\n\
             Run the CLI with a native binary or, for Docker installations, in a one-off container using the service's \
             image and config volume while the service is stopped.\n\
             Do not delete the user database or api_user_recovery, and do not guess plan/filter assignments.\n\
             Repair guide: {USER_DB_REPAIR_GUIDE}",
            serde_json::to_string(&db_path.to_string_lossy()).unwrap_or_default(),
            user_db_shell_path(db_path),
        ),
    )
}

/// Adds the repair workflow only to `InvalidData`; lock, permission and I/O errors pass through unchanged.
pub(crate) fn with_repair_hint_for_data_errors(db_path: &Path, error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::InvalidData {
        user_db_repair_error(db_path, &error)
    } else {
        error
    }
}

/// What an operator must do with a log the build cannot replay.
///
/// The V3 log is an undo log: database pages are written after the
/// before-images are synced, so without a commit record the database file can
/// hold a partially written transaction. Deleting the log would make that
/// state permanent.
fn keep_wal_advice(db_path: &Path) -> String {
    format!(
        "Do not delete '{wal}' or '{wal_temporary}': they hold the before-images needed to roll back a partially \
         written database. Back up the complete configuration directory, keep the database, write-ahead log and \
         recovery history together, and follow the repair guide.",
        wal = tuliprox_btree::wal_path(db_path).display(),
        wal_temporary = tuliprox_btree::wal_temporary_path(db_path).display(),
    )
}

/// How the operator can leave the state behind a pending write-ahead log when
/// nothing has been replayed yet.
fn pending_wal_remedy(db_path: &Path) -> String {
    format!(
        "A pending write-ahead log means the last shutdown was unclean.\n\
         Replay it under exclusive access: tuliprox --user-db-set {database}\n\
         If the log cannot be replayed: {}",
        keep_wal_advice(db_path),
        database = user_db_shell_path(db_path),
    )
}

/// Refusal for read-only commands while a write-ahead log is pending, naming the remedy.
fn pending_wal_error(db_path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "API user database {} has a pending write-ahead log; offline export and validation refuse to change it.\n{}",
            serde_json::to_string(&db_path.to_string_lossy()).unwrap_or_default(),
            pending_wal_remedy(db_path),
        ),
    )
}

/// Reports whether an active or temporary write-ahead log exists beside the database.
fn has_pending_wal(db_path: &Path) -> io::Result<bool> {
    Ok(tuliprox_btree::wal_path(db_path).try_exists()? || tuliprox_btree::wal_temporary_path(db_path).try_exists()?)
}

/// Replays a valid pending log so the following read observes committed state.
///
/// Only commands that already hold the exclusive maintenance lease may call
/// this: replaying writes the database. The database, its log and the
/// recovery history are archived first, so the pre-replay state survives. A
/// log that cannot be replayed stays a hard error.
fn replay_pending_wal(db_path: &Path) -> io::Result<()> {
    if !has_pending_wal(db_path)? {
        return Ok(());
    }
    let archive = create_repair_archive(db_path, true)?;
    let storage_version = user_storage_version(db_path).map_err(|error| with_backup(&error, &archive))?;
    if let Err(error) = load_user_records(db_path, storage_version) {
        // The remedy only applies while a log is actually still pending; a
        // duplicate key or an undecodable record needs its own repair.
        let message = if has_pending_wal(db_path)? {
            format!("{error}\nThe log could not be replayed. {}", keep_wal_advice(db_path))
        } else {
            error.to_string()
        };
        return Err(with_backup(&io::Error::new(error.kind(), message), &archive));
    }
    // A legacy V1/V2 container never writes this log, so the file cannot
    // belong to it and the scan above leaves it in place. Say so instead of
    // repeating the replay advice on the next read.
    if has_pending_wal(db_path)? {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "API user database {} did not consume its write-ahead log in this format; a V1/V2 database never \
                 writes this log, so it does not belong to this file. Back up the complete configuration directory, \
                 then move '{}' and '{}' out of the configuration directory.\n\
                 Repair guide: {USER_DB_REPAIR_GUIDE}",
                serde_json::to_string(&db_path.to_string_lossy()).unwrap_or_default(),
                tuliprox_btree::wal_path(db_path).display(),
                tuliprox_btree::wal_temporary_path(db_path).display(),
            ),
        );
        return Err(with_backup(&error, &archive));
    }
    Ok(())
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
    /// Why existing recovery history was not used for this repair; it was archived and replaced.
    pub recovery_history_error: Option<String>,
}

/// Archives the database and history, then rebuilds the database from verified history under the held lease.
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
            recovery_history_error: None,
        })
    })();
    result.map_err(|error: io::Error| with_backup(&error, &backup))
}

/// Stages resolved live records with fresh recovery history, verifies both, archives the source and publishes them.
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
        recovery_history_error: None,
    })
}

/// Swaps in the staged history and database, moving the previous history into the archive and rolling back on failure.
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

/// Confirms the published database loads as V7 and agrees with its recovery history.
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

/// Copies the database, its companion files, the merge guard and optionally the recovery history into a new archive directory.
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

/// Database file plus sorted index and write-ahead log companions.
fn related_database_files(db_path: &Path) -> [PathBuf; 4] {
    [
        db_path.to_path_buf(),
        get_file_path_for_db_index(db_path),
        tuliprox_btree::wal_path(db_path),
        tuliprox_btree::wal_temporary_path(db_path),
    ]
}

/// Recursively copies a directory of regular files, refusing symlinks and special files.
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

/// Fails when `path` itself is a symbolic link.
fn reject_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("user database repair refuses symbolic links"));
    }
    Ok(())
}

/// Removes staging artifacts, logging instead of failing because the repair outcome is already decided.
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

/// Reads the B+Tree storage version, accepting only V1, V2 and the current version.
fn user_storage_version(db_path: &Path) -> io::Result<u32> {
    let version = typed_migration::storage_version(db_path)?.ok_or_else(|| invalid("user database is not a B+Tree"))?;
    if !matches!(version, 1 | 2) && version != STORAGE_VERSION {
        return Err(invalid("unsupported user B+Tree storage version"));
    }
    Ok(version)
}

/// Returns the directory that holds the database and its recovery history.
fn database_root(db_path: &Path) -> io::Result<&Path> {
    db_path.parent().ok_or_else(|| invalid("user database has no parent directory"))
}

/// Quotes a user key as JSON so control characters cannot alter terminal output.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid-user-key>\"".to_string())
}

/// Appends the archive path to an error so the operator can find the pre-repair state.
fn with_backup(error: &io::Error, backup: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{error}; automatic repair archive: {}", backup.display()))
}

/// Builds an `InvalidData` error.
fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_hint_only_for_invalid_data() {
        let db_path = Path::new("/test/path/api_user.db");
        let data_err = io::Error::new(io::ErrorKind::InvalidData, "corrupted records");
        let wrapped = with_repair_hint_for_data_errors(db_path, data_err);
        assert_eq!(wrapped.kind(), io::ErrorKind::InvalidData);
        let msg = wrapped.to_string();
        assert!(msg.contains("corrupted records"), "{msg}");
        assert!(msg.contains(USER_DB_REPAIR_GUIDE), "{msg}");
        assert!(msg.contains("--user-db-output"), "{msg}");
        assert!(msg.contains("--user-db-input"), "{msg}");

        let would_block_err = io::Error::new(io::ErrorKind::WouldBlock, "lock acquisition would block");
        let unchanged = with_repair_hint_for_data_errors(db_path, would_block_err);
        assert_eq!(unchanged.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(unchanged.to_string(), "lock acquisition would block");
    }
}
