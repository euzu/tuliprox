//! Crash-recoverable persistence for the DVR recording queue.
//!
//! The queue used to be a single JSON document rewritten in full on every
//! mutation. That could not survive a torn write, and a change to the record
//! shape had no upgrade path other than refusing to load. This module stores
//! one record per task in a B+Tree, and routes every write through
//! [`BPlusTreeRecoveryJournal`] so the database can be rebuilt from a
//! field-named history whose schema version is migrated forward on restore.
//!
//! The repository owns the persisted record shape. It deliberately knows
//! nothing about execution: the DVR decides what a task *means*, and hands
//! this module a set of records to commit atomically.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::model::{RecordingKind, RecordingMetadata, RecordingTaskState};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
use tuliprox_btree::{
    BPlusTreeRecoveryJournal, CheckpointOutcome, RecoveryBatch, RecoveryHealth, RecoveryOpenReport, RecoveryOperation,
    RecoveryPaths, RecoveryPolicy, RecoverySchema, RecoveryVerificationReport,
};

/// Name of the operational database inside the storage directory.
const DATABASE_FILE: &str = "recordings.db";
/// Directory holding recovery generations, relative to the recovery root.
const RECOVERY_DIR: &str = "recordings_recovery";

/// Which of the queue's four partitions a task currently belongs to.
///
/// Stored explicitly rather than derived from the state: the queue treats
/// `active` as a distinct slot, and two tasks in the same state can sit in
/// different partitions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPartition {
    #[default]
    Queued,
    Scheduled,
    Active,
    Finished,
}

/// The persisted shape of one recording task.
///
/// This is storage, not wire: it carries the resolved source URL and the
/// filesystem paths, neither of which may ever reach a client.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedRecordingTask {
    pub uuid: String,
    pub kind: RecordingKind,
    pub file_dir: PathBuf,
    pub file_path: PathBuf,
    pub filename: String,
    pub url: String,
    pub finished: bool,
    pub size: u64,
    pub total_size: Option<u64>,
    pub paused: bool,
    pub error: Option<String>,
    pub state: RecordingTaskState,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub priority: i8,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub next_retry_at: Option<i64>,
    pub recording: RecordingMetadata,
    #[serde(default)]
    pub partition: RecordingPartition,
}

/// Repository-level bookkeeping, stored under its own key.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RecordingDbMetadata {
    /// Mirrors the queue's own monotonic revision so a reopened queue resumes
    /// counting where it left off.
    pub queue_revision: u64,
}

/// Keys of the recording database.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "key", rename_all = "snake_case")]
pub enum RecordingDbKey {
    /// Sorts first, so a scan reaches the bookkeeping record before any task.
    Metadata,
    Task {
        uuid: String,
    },
}

/// Values of the recording database.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum RecordingDbValue {
    Metadata(RecordingDbMetadata),
    Task(Box<PersistedRecordingTask>),
}

fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

/// A key and value must describe the same kind of record. A metadata value
/// filed under a task key would be silently skipped by every reader.
fn check_pairing(key: &RecordingDbKey, value: &RecordingDbValue) -> io::Result<()> {
    match (key, value) {
        (RecordingDbKey::Metadata, RecordingDbValue::Metadata(_))
        | (RecordingDbKey::Task { .. }, RecordingDbValue::Task(_)) => Ok(()),
        _ => Err(invalid("recording record key and value describe different record kinds")),
    }
}

/// Version 1 of the recording recovery schema.
///
/// Every field is encoded by name. When the record shape changes, raise
/// `CURRENT_VERSION` and add the step to `migrate_one`; existing histories
/// are then migrated forward on restore rather than rejected.
pub struct RecordingRecoverySchema;

impl RecoverySchema<RecordingDbKey, RecordingDbValue> for RecordingRecoverySchema {
    const NAME: &'static str = "recording";
    const CURRENT_VERSION: u32 = 1;

    fn encode_key(&self, key: &RecordingDbKey) -> io::Result<Value> {
        serde_json::to_value(key).map_err(|error| invalid(error.to_string()))
    }

    fn migrate_key_one(&self, from: u32, _key: Value) -> io::Result<Value> {
        Err(invalid(format!("no recording key migration from version {from}")))
    }

    fn decode_current_key(&self, key: Value) -> io::Result<RecordingDbKey> {
        serde_json::from_value(key).map_err(|error| invalid(error.to_string()))
    }

    fn encode_current(&self, value: &RecordingDbValue) -> io::Result<Value> {
        serde_json::to_value(value).map_err(|error| invalid(error.to_string()))
    }

    fn migrate_one(&self, from: u32, _value: Value) -> io::Result<Value> {
        Err(invalid(format!("no recording value migration from version {from}")))
    }

    fn decode_current(&self, value: Value) -> io::Result<RecordingDbValue> {
        serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
    }
}

type Journal = BPlusTreeRecoveryJournal<RecordingDbKey, RecordingDbValue, RecordingRecoverySchema>;

/// The complete committed content of the repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordingRepositorySnapshot {
    pub queue_revision: u64,
    pub tasks: Vec<PersistedRecordingTask>,
}

/// The recoverable recording store.
pub struct RecordingRepository {
    journal: Journal,
}

impl RecordingRepository {
    /// Opens the repository, creating it when neither a database nor a
    /// recovery history exists, and rebuilding it when the history is ahead.
    ///
    /// `recovery_root` is where recovery generations live. Pointing it at a
    /// different filesystem than `storage_dir` is what makes the history
    /// survive the loss of the database volume; the caller decides, and
    /// [`RecordingRepository::health`] reports what was chosen.
    pub fn open(storage_dir: &Path, recovery_root: &Path) -> io::Result<(Self, RecoveryOpenReport)> {
        let paths =
            RecoveryPaths { database: storage_dir.join(DATABASE_FILE), directory: recovery_root.join(RECOVERY_DIR) };
        let (journal, report) = Journal::open(paths, RecordingRecoverySchema, RecoveryPolicy::default())?;
        Ok((Self { journal }, report))
    }

    /// Reads every committed task, plus the queue revision.
    pub fn load(&mut self) -> io::Result<RecordingRepositorySnapshot> {
        let mut snapshot = RecordingRepositorySnapshot::default();
        for (key, value) in self.journal.entries()? {
            check_pairing(&key, &value)?;
            match value {
                RecordingDbValue::Metadata(metadata) => snapshot.queue_revision = metadata.queue_revision,
                RecordingDbValue::Task(task) => {
                    let RecordingDbKey::Task { uuid } = &key else {
                        return Err(invalid("task value stored under a non-task key"));
                    };
                    if *uuid != task.uuid {
                        return Err(invalid("recording task is filed under a different uuid than it carries"));
                    }
                    snapshot.tasks.push(*task);
                }
            }
        }
        Ok(snapshot)
    }

    /// Replaces the committed task set in one atomic batch.
    ///
    /// The queue mutates by rebuilding a whole candidate set, so the
    /// repository diffs that against what is stored and commits the
    /// difference. Committing the diff rather than the whole set keeps the
    /// recovery journal proportional to what actually changed.
    pub fn commit(&mut self, queue_revision: u64, tasks: &[PersistedRecordingTask]) -> io::Result<()> {
        let mut incoming = BTreeMap::new();
        for task in tasks {
            if incoming.insert(task.uuid.clone(), task).is_some() {
                return Err(invalid("recording batch contains two tasks with the same uuid"));
            }
        }

        let existing = self.load()?;
        let mut operations = Vec::new();
        for stored in &existing.tasks {
            if !incoming.contains_key(&stored.uuid) {
                operations.push(RecoveryOperation::Delete(RecordingDbKey::Task { uuid: stored.uuid.clone() }));
            }
        }
        let stored_by_uuid: BTreeMap<&str, &PersistedRecordingTask> =
            existing.tasks.iter().map(|task| (task.uuid.as_str(), task)).collect();
        for (uuid, task) in incoming {
            if stored_by_uuid.get(uuid.as_str()).is_some_and(|stored| *stored == task) {
                continue;
            }
            operations.push(RecoveryOperation::Upsert(
                RecordingDbKey::Task { uuid },
                RecordingDbValue::Task(Box::new(task.clone())),
            ));
        }
        if existing.queue_revision != queue_revision || operations.is_empty() {
            operations.push(RecoveryOperation::Upsert(
                RecordingDbKey::Metadata,
                RecordingDbValue::Metadata(RecordingDbMetadata { queue_revision }),
            ));
        }

        let _ = self.journal.apply_batch(RecoveryBatch::new(operations))?;
        let _ = self.journal.checkpoint_if_needed()?;
        Ok(())
    }

    /// Confirms the database and its recovery history agree, and that every
    /// stored task is filed under the uuid it carries.
    pub fn verify(&mut self) -> io::Result<RecoveryVerificationReport> {
        let _ = self.load()?;
        self.journal.verify()
    }

    pub fn checkpoint_if_needed(&mut self) -> io::Result<CheckpointOutcome> { self.journal.checkpoint_if_needed() }

    pub fn health(&self) -> RecoveryHealth { self.journal.health() }
}

#[cfg(test)]
mod tests {
    use super::{
        PersistedRecordingTask, RecordingDbKey, RecordingDbMetadata, RecordingDbValue, RecordingPartition,
        RecordingRepository,
    };
    use shared::model::{
        recording::{RecordingOwner, RecordingSource, RecordingVisibility},
        RecordingKind, RecordingMetadata, RecordingTaskState, UserId,
    };
    use std::{io, path::PathBuf};
    use tempfile::TempDir;
    use tuliprox_btree::{RecoveryOpenAction, RecoveryRepositoryState};

    fn metadata() -> RecordingMetadata {
        RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("target", "42", "input"),
            "Film".to_owned(),
        )
    }

    fn task(uuid: &str) -> PersistedRecordingTask {
        PersistedRecordingTask {
            uuid: uuid.to_owned(),
            kind: RecordingKind::Vod,
            file_dir: PathBuf::from("/rec"),
            file_path: PathBuf::from("/rec/film.mp4"),
            filename: "film.mp4".to_owned(),
            url: "http://provider.invalid/film.mp4".to_owned(),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: RecordingTaskState::Queued,
            input_name: Some("input".to_owned()),
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: metadata(),
            partition: RecordingPartition::Queued,
        }
    }

    struct Fixture {
        _root: TempDir,
        storage: PathBuf,
        recovery: PathBuf,
    }

    impl Fixture {
        fn new() -> io::Result<Self> {
            let root = TempDir::new()?;
            let storage = root.path().join("storage");
            let recovery = root.path().join("recovery");
            std::fs::create_dir_all(&storage)?;
            std::fs::create_dir_all(&recovery)?;
            Ok(Self { _root: root, storage, recovery })
        }

        fn open(&self) -> io::Result<RecordingRepository> {
            RecordingRepository::open(&self.storage, &self.recovery).map(|(repository, _)| repository)
        }
    }

    #[test]
    fn a_fresh_repository_is_created_empty() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let (mut repository, report) = RecordingRepository::open(&fixture.storage, &fixture.recovery)?;
        assert_eq!(report.action, RecoveryOpenAction::Created);
        let snapshot = repository.load()?;
        assert_eq!(snapshot.queue_revision, 0);
        assert_eq!(snapshot.tasks.len(), 0);
        Ok(())
    }

    #[test]
    fn committed_tasks_survive_a_reopen() -> io::Result<()> {
        let fixture = Fixture::new()?;
        {
            let mut repository = fixture.open()?;
            repository.commit(7, &[task("a"), task("b")])?;
        }
        let mut repository = fixture.open()?;
        let snapshot = repository.load()?;
        assert_eq!(snapshot.queue_revision, 7);
        assert_eq!(snapshot.tasks.iter().map(|t| t.uuid.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
        Ok(())
    }

    #[test]
    fn commit_replaces_the_whole_task_set() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task("a"), task("b")])?;

        let mut changed = task("b");
        changed.state = RecordingTaskState::Running;
        repository.commit(2, &[changed, task("c")])?;

        let snapshot = repository.load()?;
        assert_eq!(snapshot.queue_revision, 2);
        assert_eq!(snapshot.tasks.iter().map(|t| t.uuid.clone()).collect::<Vec<_>>(), vec!["b", "c"]);
        assert_eq!(snapshot.tasks[0].state, RecordingTaskState::Running);
        Ok(())
    }

    #[test]
    fn commit_rejects_a_duplicate_uuid() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        assert!(repository.commit(1, &[task("a"), task("a")]).is_err());
        Ok(())
    }

    #[test]
    fn an_empty_commit_still_advances_the_revision() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[])?;
        repository.commit(2, &[])?;
        assert_eq!(repository.load()?.queue_revision, 2);
        Ok(())
    }

    #[test]
    fn a_deleted_database_is_rebuilt_from_the_recovery_history() -> io::Result<()> {
        let fixture = Fixture::new()?;
        {
            let mut repository = fixture.open()?;
            repository.commit(3, &[task("a"), task("b")])?;
        }
        std::fs::remove_file(fixture.storage.join(super::DATABASE_FILE))?;

        let (mut repository, report) = RecordingRepository::open(&fixture.storage, &fixture.recovery)?;
        assert_eq!(report.action, RecoveryOpenAction::Rebuilt);
        let snapshot = repository.load()?;
        assert_eq!(snapshot.queue_revision, 3);
        assert_eq!(snapshot.tasks.len(), 2);
        assert_eq!(repository.health().state, RecoveryRepositoryState::Healthy);
        Ok(())
    }

    #[test]
    fn a_destroyed_recovery_history_fails_closed() -> io::Result<()> {
        let fixture = Fixture::new()?;
        {
            let mut repository = fixture.open()?;
            repository.commit(1, &[task("a")])?;
        }
        std::fs::remove_dir_all(fixture.recovery.join(super::RECOVERY_DIR))?;
        // The database is ahead of every surviving history, which is exactly
        // the case that must not be resolved by guessing.
        assert!(RecordingRepository::open(&fixture.storage, &fixture.recovery).is_err());
        Ok(())
    }

    #[test]
    fn verify_reports_agreement_between_database_and_history() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task("a"), task("b")])?;
        let report = repository.verify()?;
        assert_eq!(report.database_revision, report.recovery_revision);
        // Two tasks plus the metadata record.
        assert_eq!(report.live_records, 3);
        Ok(())
    }

    #[test]
    fn a_mismatched_key_and_value_is_rejected() {
        assert!(super::check_pairing(&RecordingDbKey::Metadata, &RecordingDbValue::Task(Box::new(task("a")))).is_err());
        assert!(super::check_pairing(
            &RecordingDbKey::Task { uuid: "a".to_owned() },
            &RecordingDbValue::Metadata(RecordingDbMetadata::default()),
        )
        .is_err());
        assert!(super::check_pairing(
            &RecordingDbKey::Task { uuid: "a".to_owned() },
            &RecordingDbValue::Task(Box::new(task("a"))),
        )
        .is_ok());
    }

    #[test]
    fn records_are_encoded_by_field_name() -> io::Result<()> {
        // The recovery history must not depend on field order: that is the
        // whole reason it exists beside the positional B+Tree encoding.
        let encoded = serde_json::to_string(&RecordingDbValue::Task(Box::new(task("a"))))?;
        for field in ["\"uuid\"", "\"kind\"", "\"state\"", "\"partition\"", "\"recording\""] {
            assert!(encoded.contains(field), "{field} is missing from {encoded}");
        }
        Ok(())
    }
}
