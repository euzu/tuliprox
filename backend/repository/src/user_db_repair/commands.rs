//! Read-only exports and validation, followed by an exclusive all-or-nothing repair.
use super::{
    database_root, invalid, rebuild_from_live_records, rebuild_from_verified_history, reject_symlink,
    related_database_files, replay_pending_wal, user_storage_version, UserDbRepairReport, RECOVERY_DIRECTORY,
};
use crate::{
    api_user_recovery::{classify_user_record, ApiUserRepository, StoredApiUserV7, UserRecordDecode},
    startup_migration::load_user_records_read_only,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
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
    /// Ambiguous records in the database whose `plan`/`filter` field is unknown.
    pub unassigned_records: usize,
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
    reserved.extend([sidecar_lock_path(db_path), super::user_db_merge_guard_path(root)]);
    let paths = RecoveryPaths { database: db_path.to_path_buf(), directory: root.join(RECOVERY_DIRECTORY) };
    reserved.push(tuliprox_btree::maintenance_lease_path(&paths)?);
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
    let mut file = options.open(output).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                error.kind(),
                format!(
                    "export file {} already exists; choose a new path or remove it first; it was not overwritten",
                    output.display()
                ),
            )
        } else {
            error
        }
    })?;
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
        unassigned_records: snapshot.unassigned.len(),
        recovery_verified: snapshot.verified_history_records.is_some(),
    })
}

/// Validates and publishes an entire repair while holding exclusive access.
///
/// Without an input document this repairs from verified recovery history, or
/// from fully decoded live records. A separate earlier check is never trusted
/// in place of this validation.
pub fn set_user_database(db_path: &Path, input: Option<&Path>) -> io::Result<UserDbRepairReport> {
    fs::metadata(db_path)?;
    let root = database_root(db_path)?;
    let maintenance = RecoveryMaintenanceGuard::try_acquire(&RecoveryPaths {
        database: db_path.to_path_buf(),
        directory: root.join(RECOVERY_DIRECTORY),
    })?;
    // Parse the input before anything is written, so an unreadable document
    // never costs the pending log.
    let document = input.map(read_document).transpose()?;
    // The lease makes this the only writer, so a pending log can be replayed
    // instead of blocking the repair that is meant to resolve it.
    replay_pending_wal(db_path)?;
    let snapshot = read_snapshot(db_path)?;
    let users = validate_document(&snapshot, document.as_ref())?;
    if let Some(records) = snapshot.verified_history_records {
        rebuild_from_verified_history(db_path, root, records, &maintenance)
    } else {
        rebuild_from_live_records(db_path, root, &users)
    }
}

fn read_snapshot(db_path: &Path) -> io::Result<Snapshot> {
    // A pending log means the last shutdown was unclean; reading around it
    // would report stale records, so refuse and name the way forward.
    if super::has_pending_wal(db_path)? {
        return Err(super::pending_wal_error(db_path));
    }
    let storage_version = user_storage_version(db_path)?;
    let fingerprint = source_fingerprint(db_path)?;
    let mut users = BTreeMap::new();
    let mut unassigned = BTreeMap::new();
    let verified_history_records =
        if let Ok((report, records)) = ApiUserRepository::read_verified_history(db_path, database_root(db_path)?) {
            for (key, record) in records {
                users.insert(key, StoredApiUserV7::from(&record));
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
    // Re-check the source fingerprint to detect any concurrent writer during the read.
    let fingerprint_after = source_fingerprint(db_path)?;
    if fingerprint != fingerprint_after {
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
                "unassigned values require an edited export; run --user-db-get with --user-db-output users.json",
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
    io::copy(&mut file, hasher)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api_user_recovery::test_fixtures::{compact_user, CompactUserRecord},
        user_db_repair::UserDbRepairSource,
    };
    use tempfile::tempdir;
    use tuliprox_btree::BPlusTree;

    fn write_compact_database(path: &Path, users: &[(&str, Option<&str>, Option<&str>)]) -> io::Result<()> {
        let mut tree = BPlusTree::<String, CompactUserRecord>::new();
        for (username, plan, filter) in users {
            tree.insert((*username).to_string(), compact_user(username, *plan, *filter));
        }
        tree.store(path)?;
        Ok(())
    }

    fn record_fields(path: &Path, key: &str) -> io::Result<Vec<serde_json::Value>> {
        let mut query = tuliprox_btree::BPlusTreeQuery::<String, serde_json::Value>::try_new(path)?;
        let record = query
            .iter()
            .find_map(|entry| match entry {
                Ok((stored_key, value)) if stored_key == key => Some(Ok(value)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .transpose()?
            .ok_or_else(|| io::Error::other("stored user is missing"))?;
        record.as_array().cloned().ok_or_else(|| io::Error::other("stored user is not a positional array"))
    }

    /// Resolves the ambiguous value the way an operator would: the value moves
    /// unchanged into `plan` and `unassigned_value` becomes null.
    fn export_with_resolved_plan(db_path: &Path) -> io::Result<UserDbExport> {
        let mut export = get_user_database(db_path, None)?;
        for user in &mut export.users {
            if let Some(value) = user.unassigned_value.take() {
                user.plan = Some(value);
            }
        }
        Ok(export)
    }

    fn write_document(path: &Path, export: &UserDbExport) -> io::Result<()> {
        let mut encoded = serde_json::to_vec_pretty(export).map_err(io::Error::other)?;
        encoded.push(b'\n');
        fs::write(path, encoded)
    }

    #[test]
    fn export_names_the_ambiguous_compact_value() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("alice", Some("basic"), None)])?;

        let export = get_user_database(&db_path, None)?;
        assert_eq!(export.users.len(), 1);
        assert_eq!(export.users[0].user_key, "alice");
        assert_eq!(export.users[0].unassigned_value.as_deref(), Some("basic"));
        assert_eq!(export.users[0].plan, None);
        assert_eq!(export.users[0].filter, None);
        Ok(())
    }

    #[test]
    fn export_leaves_a_complete_record_untouched() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("carol", Some("premium"), Some("Group ~ \"^DE.*\""))])?;

        let export = get_user_database(&db_path, None)?;
        assert_eq!(export.users[0].plan.as_deref(), Some("premium"));
        assert_eq!(export.users[0].filter.as_deref(), Some("Group ~ \"^DE.*\""));
        assert_eq!(export.users[0].unassigned_value, None);
        Ok(())
    }

    #[test]
    fn check_rejects_an_unedited_export() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("alice", Some("basic"), None)])?;
        let export_path = dir.path().join("users.json");
        write_user_database_export(&db_path, &get_user_database(&db_path, None)?, &export_path)?;

        let error = check_user_database(&db_path, Some(&export_path)).expect_err("an unedited export must be refused");
        assert!(error.to_string().contains("unassigned_value"), "{error}");
        Ok(())
    }

    #[test]
    fn check_leaves_database_and_recovery_untouched() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let before = fs::read(&db_path)?;

        let report = check_user_database(&db_path, None)?;
        assert_eq!(report.records, 1);
        assert_eq!(report.unassigned_records, 0);
        assert!(!report.recovery_verified);
        assert_eq!(fs::read(&db_path)?, before);
        assert!(!dir.path().join(RECOVERY_DIRECTORY).exists());
        Ok(())
    }

    #[test]
    fn set_publishes_complete_records_with_recovery_history() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("alice", Some("basic"), None), ("bob", None, None)])?;
        let document_path = dir.path().join("users.json");
        write_document(&document_path, &export_with_resolved_plan(&db_path)?)?;

        assert!(matches!(
            check_user_database(&db_path, Some(&document_path)),
            Ok(UserDbCheckReport { unassigned_records: 1, .. })
        ));
        let report = set_user_database(&db_path, Some(&document_path))?;
        assert_eq!(report.records, 2);
        assert_eq!(report.source, UserDbRepairSource::ExplicitLiveRecords);
        assert!(report.backup_path.is_dir(), "repair archive must exist");
        assert!(dir.path().join(RECOVERY_DIRECTORY).is_dir(), "recovery history must exist");

        let fields = record_fields(&db_path, "alice")?;
        assert_eq!(fields.len(), 21, "published records keep every optional slot");
        assert_eq!(fields[19], serde_json::json!("basic"));
        assert_eq!(fields[20], serde_json::Value::Null);
        assert_eq!(record_fields(&db_path, "bob")?.len(), 21);

        let verified = check_user_database(&db_path, None)?;
        assert!(verified.recovery_verified, "fresh history must verify");
        assert_eq!(verified.unassigned_records, 0);
        Ok(())
    }

    #[test]
    fn set_without_input_repairs_an_unambiguous_database() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;

        let report = set_user_database(&db_path, None)?;
        assert_eq!(report.records, 1);
        assert_eq!(report.source, UserDbRepairSource::ExplicitLiveRecords);
        assert_eq!(record_fields(&db_path, "bob")?.len(), 21);
        Ok(())
    }

    #[test]
    fn set_refuses_a_database_that_still_needs_a_decision() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("alice", Some("basic"), None)])?;
        let before = fs::read(&db_path)?;

        let error = set_user_database(&db_path, None).expect_err("an ambiguous record must block the repair");
        assert!(error.to_string().contains("--user-db-get"), "{error}");
        assert_eq!(fs::read(&db_path)?, before, "a refused repair must not touch the database");
        Ok(())
    }

    #[test]
    fn export_refuses_to_overwrite_an_existing_file() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let output = dir.path().join("users.json");
        fs::write(&output, b"keep me")?;

        let error =
            write_user_database_export(&db_path, &get_user_database(&db_path, None)?, &output).expect_err("refused");
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(fs::read(&output)?, b"keep me");
        Ok(())
    }

    #[test]
    fn export_refuses_the_recovery_directory() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let recovery = dir.path().join(RECOVERY_DIRECTORY);
        fs::create_dir_all(&recovery)?;

        let error = write_user_database_export(&db_path, &get_user_database(&db_path, None)?, &recovery.join("u.json"))
            .expect_err("the recovery directory is reserved");
        assert!(error.to_string().contains("outside the database artifacts"), "{error}");
        Ok(())
    }

    #[test]
    fn check_rejects_an_export_from_a_different_database_state() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let export_path = dir.path().join("users.json");
        write_user_database_export(&db_path, &get_user_database(&db_path, None)?, &export_path)?;

        write_compact_database(&db_path, &[("bob", None, None), ("dave", None, None)])?;

        let error = check_user_database(&db_path, Some(&export_path)).expect_err("a stale export must be refused");
        assert!(error.to_string().contains("does not match"), "{error}");
        Ok(())
    }

    #[test]
    fn a_pending_log_is_reported_with_a_remedy() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        fs::write(tuliprox_btree::wal_path(&db_path), b"unfinished")?;

        let error = get_user_database(&db_path, None).expect_err("a pending log must be reported");
        let text = error.to_string();
        assert!(text.contains("write-ahead log"), "{text}");
        assert!(text.contains("--user-db-set"), "{text}");
        assert!(text.contains(".wal.tmp"), "{text}");
        Ok(())
    }

    #[test]
    fn set_reports_an_unreplayable_log_without_touching_the_database() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let before = fs::read(&db_path)?;
        fs::write(tuliprox_btree::wal_path(&db_path), b"not a write-ahead log")?;

        let error = set_user_database(&db_path, None).expect_err("an unreadable log must stop the repair");
        let text = error.to_string();
        assert!(text.contains("could not be replayed"), "{text}");
        assert!(text.contains("Do not delete"), "an undo log must never be discarded: {text}");
        assert!(text.contains("automatic repair archive"), "{text}");
        assert!(
            !text.contains("Replay it under exclusive access"),
            "the failed replay must not be suggested again: {text}"
        );
        assert_eq!(fs::read(&db_path)?, before);
        Ok(())
    }

    /// A committed log left by an interrupted shutdown must be replayed by the
    /// repair, not treated as an unmovable obstacle.
    #[test]
    fn set_replays_a_committed_log_and_publishes_the_repair() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        tuliprox_btree::test_support::leave_committed_test_wal(&db_path)?;
        let wal = tuliprox_btree::wal_path(&db_path);
        assert!(wal.exists(), "the fixture must leave a pending log");

        let pending_log = fs::read(&wal)?;

        let report = set_user_database(&db_path, None)?;
        assert_eq!(report.records, 1);
        assert!(!wal.exists(), "the replay must consume the log");
        let archived_log = fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("api_user.db.wal"))
            .find(|path| path.is_file())
            .ok_or_else(|| io::Error::other("the pre-replay archive must keep the pending log"))?;
        assert_eq!(fs::read(archived_log)?, pending_log);
        assert_eq!(record_fields(&db_path, "bob")?.len(), 21);
        assert!(check_user_database(&db_path, None)?.recovery_verified);
        Ok(())
    }

    #[test]
    fn set_archives_the_original_database_before_replacing_it() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let before = fs::read(&db_path)?;

        let report = set_user_database(&db_path, None)?;
        let archived_db = report.backup_path.join("api_user.db");
        assert_eq!(fs::read(&archived_db)?, before, "the archive must hold the pre-repair database");
        assert_ne!(fs::read(&db_path)?, before, "the database must have been rewritten");
        Ok(())
    }

    /// A legacy container is read through the legacy reader and published in
    /// the current layout, so an old file needs no separate migration step.
    #[test]
    fn set_publishes_a_legacy_container_in_the_current_layout() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        let mut legacy = tuliprox_btree::v2::BPlusTree::<String, CompactUserRecord>::new();
        legacy.insert("alice".to_string(), compact_user("alice", None, None));
        legacy.store(&db_path)?;
        assert_eq!(tuliprox_btree::typed_migration::storage_version(&db_path)?, Some(2));

        let report = set_user_database(&db_path, None)?;
        assert_eq!(report.records, 1);
        assert_eq!(tuliprox_btree::typed_migration::storage_version(&db_path)?, Some(3));
        assert_eq!(record_fields(&db_path, "alice")?.len(), 21);
        assert!(check_user_database(&db_path, None)?.recovery_verified);
        Ok(())
    }

    /// A legacy container has no write-ahead log, so a stray one cannot be
    /// consumed. The repair must say that instead of repeating the replay hint.
    #[test]
    fn set_reports_a_log_a_legacy_container_cannot_consume() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        let mut legacy = tuliprox_btree::v2::BPlusTree::<String, CompactUserRecord>::new();
        legacy.insert("bob".to_string(), compact_user("bob", None, None));
        legacy.store(&db_path)?;
        let before = fs::read(&db_path)?;
        fs::write(tuliprox_btree::wal_path(&db_path), b"unfinished")?;

        let error = set_user_database(&db_path, None).expect_err("a legacy container cannot consume the log");
        let text = error.to_string();
        assert!(text.contains("did not consume its write-ahead log"), "{text}");
        assert!(text.contains(".wal.tmp"), "{text}");
        assert!(!text.contains("Replay it under exclusive access"), "{text}");
        assert_eq!(fs::read(&db_path)?, before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn export_is_written_with_owner_only_permissions() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir()?;
        let db_path = dir.path().join("api_user.db");
        write_compact_database(&db_path, &[("bob", None, None)])?;
        let output = dir.path().join("users.json");

        write_user_database_export(&db_path, &get_user_database(&db_path, None)?, &output)?;
        let mode = fs::metadata(&output)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an export must not be readable by other users");
        Ok(())
    }
}
