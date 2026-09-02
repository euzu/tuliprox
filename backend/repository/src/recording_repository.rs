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
use shared::model::{
    recording::{RecordingOwner, RecordingVisibility},
    RecordingKind, RecordingMetadata, RecordingTaskState, UserId,
};
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
const DATABASE_FILE: &str = "recordings_library.db";
/// Directory holding recovery generations, relative to the recovery root.
const RECOVERY_DIR: &str = "recordings_library_recovery";

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
    /// What media this task refers to. Two tasks with the same identity are the
    /// same recording and share one physical file.
    #[serde(default)]
    pub media_identity: String,
}

/// Who a library entry belongs to.
///
/// This is the entry's own dimension, not the file's: the same physical
/// recording can be reachable from several principals at once.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "principal", rename_all = "snake_case")]
pub enum LibraryPrincipal {
    User { id: String },
    Shared,
}

impl LibraryPrincipal {
    /// Stable key for indexing and for the reference invariant.
    pub fn key(&self) -> String {
        match self {
            Self::User { id } => format!("user:{id}"),
            Self::Shared => "shared".to_owned(),
        }
    }
}

/// One physical recording: the file on disk and the work that produces it.
///
/// Carries no owner and no visibility. Those belong to the library entries
/// that reference it, because one file can be reachable from several.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedMaterialization {
    pub id: String,
    pub kind: RecordingKind,
    pub file_dir: PathBuf,
    pub file_path: PathBuf,
    pub filename: String,
    pub url: String,
    pub size: u64,
    pub total_size: Option<u64>,
    #[serde(default)]
    pub input_name: Option<String>,
    /// The highest priority among the attached entries; recomputed on attach
    /// and detach rather than owned by any one of them.
    #[serde(default)]
    pub priority: i8,
    /// What media this file holds. Every entry attached to it agreed on this.
    #[serde(default)]
    pub media_identity: String,
    /// Media identity and file state. The owner and visibility fields of this
    /// metadata are not authoritative here; the library entry owns them.
    pub media: RecordingMetadata,
    /// How many library entries reference this file. A physical delete is only
    /// legal at zero.
    #[serde(default)]
    pub reference_count: u32,
}

/// One user's link to a physical recording.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PersistedLibraryEntry {
    pub id: String,
    pub principal: LibraryPrincipal,
    pub materialization_id: String,
    /// Bytes charged to this principal's quota pool. Every attached entry is
    /// charged the whole logical size; the physical file is charged once.
    #[serde(default)]
    pub quota_bytes: u64,
    #[serde(default)]
    pub priority: i8,
    /// Where this entry is in its own lifecycle.
    ///
    /// Execution state is per entry, not per file: one entry can be running a
    /// transfer while another is queued behind it or has already been
    /// cancelled. Holding it on the shared materialization meant the last
    /// entry committed decided what every other entry looked like.
    #[serde(default)]
    pub state: RecordingTaskState,
    #[serde(default)]
    pub partition: RecordingPartition,
    #[serde(default)]
    pub finished: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub next_retry_at: Option<i64>,
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
    /// Sorts first, so a scan reaches the bookkeeping record before any record.
    Metadata,
    Materialization {
        id: String,
    },
    LibraryEntry {
        id: String,
    },
}

/// Values of the recording database.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum RecordingDbValue {
    Metadata(RecordingDbMetadata),
    Materialization(Box<PersistedMaterialization>),
    LibraryEntry(Box<PersistedLibraryEntry>),
}

fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

/// A key and value must describe the same kind of record. A metadata value
/// filed under a task key would be silently skipped by every reader.
fn check_pairing(key: &RecordingDbKey, value: &RecordingDbValue) -> io::Result<()> {
    match (key, value) {
        (RecordingDbKey::Metadata, RecordingDbValue::Metadata(_))
        | (RecordingDbKey::Materialization { .. }, RecordingDbValue::Materialization(_))
        | (RecordingDbKey::LibraryEntry { .. }, RecordingDbValue::LibraryEntry(_)) => Ok(()),
        _ => Err(invalid("recording record key and value describe different record kinds")),
    }
}

/// Version 1 of the split recording recovery schema.
///
/// Every field is encoded by name. When the record shape changes, raise
/// `CURRENT_VERSION` and add the step to `migrate_one`; existing histories
/// are then migrated forward on restore rather than rejected.
///
/// The name is distinct from the pre-split `recording` schema on purpose. That
/// shape stored one task per recording with the owner embedded, which cannot
/// be expressed as a one-to-one value migration into a materialization plus a
/// library entry. The queue does not migrate, so the old database and history
/// are left in place for inspection and a fresh pair is created beside them.
pub struct RecordingRecoverySchema;

impl RecoverySchema<RecordingDbKey, RecordingDbValue> for RecordingRecoverySchema {
    const NAME: &'static str = "recording_library";
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

/// The stored records, indexed for joining and for the reference invariant.
#[derive(Debug, Default)]
struct StoredRecords {
    queue_revision: u64,
    materializations: BTreeMap<String, PersistedMaterialization>,
    entries: BTreeMap<String, PersistedLibraryEntry>,
}

impl StoredRecords {
    fn attached_counts(&self) -> BTreeMap<&str, u32> {
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for entry in self.entries.values() {
            *counts.entry(entry.materialization_id.as_str()).or_default() += 1;
        }
        counts
    }

    fn recount_references(&mut self) {
        let counts: BTreeMap<String, u32> =
            self.attached_counts().into_iter().map(|(id, count)| (id.to_owned(), count)).collect();
        for (id, materialization) in &mut self.materializations {
            materialization.reference_count = counts.get(id).copied().unwrap_or(0);
        }
    }

    /// The strongest priority any attached entry asked for.
    ///
    /// Priority is per entry but scheduling is per file: one transfer serves
    /// every entry pointing at it. Taking any single entry's value lets a
    /// background request hold back somebody else's foreground one for the
    /// same media. Lower is stronger.
    fn recompute_effective_priorities(&mut self) {
        let mut strongest: BTreeMap<&str, i8> = BTreeMap::new();
        for entry in self.entries.values() {
            let slot = strongest.entry(entry.materialization_id.as_str()).or_insert(i8::MAX);
            *slot = (*slot).min(entry.priority);
        }
        let strongest: BTreeMap<String, i8> =
            strongest.into_iter().map(|(id, priority)| (id.to_owned(), priority)).collect();
        for (id, materialization) in &mut self.materializations {
            if let Some(priority) = strongest.get(id) {
                materialization.priority = *priority;
            }
        }
    }

    /// A file whose count disagrees with the links pointing at it would either
    /// be deleted while still reachable, or kept forever after its last
    /// reference went away. Neither is recoverable by guessing, so a
    /// disagreement fails the read.
    fn check_reference_invariant(&self) -> io::Result<()> {
        let counts = self.attached_counts();
        for (id, materialization) in &self.materializations {
            let attached = counts.get(id.as_str()).copied().unwrap_or(0);
            if attached != materialization.reference_count {
                return Err(invalid(format!(
                    "materialization reference count is {} but {attached} library entries reference it",
                    materialization.reference_count
                )));
            }
        }
        for entry in self.entries.values() {
            if !self.materializations.contains_key(&entry.materialization_id) {
                return Err(invalid("library entry references a materialization that does not exist"));
            }
        }
        Ok(())
    }
}

/// The materialization id a task's file is stored under.
///
/// Derived from the media identity, so two requests for the same thing land on
/// one file. Distinct from the entry id, which is what a user addresses.
/// Falls back to the task's own uuid when no identity was supplied, which
/// keeps such a task on a file of its own rather than colliding with others.
pub fn materialization_id_for(task: &PersistedRecordingTask) -> String {
    if task.media_identity.is_empty() {
        return format!("mat-uuid:{}", task.uuid);
    }
    format!("mat-{}", task.media_identity)
}

/// Split a task into the file it produces and the link that owns it.
fn split(task: &PersistedRecordingTask) -> (PersistedMaterialization, PersistedLibraryEntry) {
    let materialization_id = materialization_id_for(task);
    let principal = match task.recording.visibility {
        RecordingVisibility::Shared => LibraryPrincipal::Shared,
        RecordingVisibility::Private => LibraryPrincipal::User { id: task.recording.owner_id().0.clone() },
    };
    let materialization = PersistedMaterialization {
        id: materialization_id.clone(),
        kind: task.kind,
        file_dir: task.file_dir.clone(),
        file_path: task.file_path.clone(),
        filename: task.filename.clone(),
        url: task.url.clone(),
        size: task.size,
        total_size: task.total_size,
        input_name: task.input_name.clone(),
        priority: task.priority,
        media_identity: task.media_identity.clone(),
        media: task.recording.clone(),
        reference_count: 1,
    };
    let entry = PersistedLibraryEntry {
        id: task.uuid.clone(),
        principal,
        materialization_id,
        quota_bytes: task.recording.reserved_bytes,
        priority: task.priority,
        state: task.state,
        partition: task.partition,
        finished: task.finished,
        paused: task.paused,
        error: task.error.clone(),
        retry_attempts: task.retry_attempts,
        next_retry_at: task.next_retry_at,
    };
    (materialization, entry)
}

/// Rejoin a file and one of its links into the task view the DVR works in.
///
/// The link is authoritative for owner and visibility; the file is
/// authoritative for everything physical.
fn join(materialization: &PersistedMaterialization, entry: &PersistedLibraryEntry) -> PersistedRecordingTask {
    let mut media = materialization.media.clone();
    match &entry.principal {
        LibraryPrincipal::Shared => media.visibility = RecordingVisibility::Shared,
        LibraryPrincipal::User { id } => {
            media.visibility = RecordingVisibility::Private;
            media.owner = RecordingOwner::User(UserId::from(id.as_str()));
        }
    }
    media.reserved_bytes = entry.quota_bytes;
    PersistedRecordingTask {
        uuid: entry.id.clone(),
        kind: materialization.kind,
        file_dir: materialization.file_dir.clone(),
        file_path: materialization.file_path.clone(),
        filename: materialization.filename.clone(),
        url: materialization.url.clone(),
        finished: entry.finished,
        size: materialization.size,
        total_size: materialization.total_size,
        paused: entry.paused,
        error: entry.error.clone(),
        state: entry.state,
        input_name: materialization.input_name.clone(),
        priority: entry.priority,
        retry_attempts: entry.retry_attempts,
        next_retry_at: entry.next_retry_at,
        recording: media,
        partition: entry.partition,
        media_identity: materialization.media_identity.clone(),
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

    /// Reads every committed library entry joined to the file it references,
    /// plus the queue revision.
    ///
    /// The caller still works in whole tasks. Splitting them is the
    /// repository's job precisely so the reference count has one owner.
    pub fn load(&mut self) -> io::Result<RecordingRepositorySnapshot> {
        let stored = self.read_records()?;
        let mut snapshot = RecordingRepositorySnapshot { queue_revision: stored.queue_revision, tasks: Vec::new() };
        for entry in stored.entries.values() {
            let materialization = stored
                .materializations
                .get(&entry.materialization_id)
                .ok_or_else(|| invalid("library entry references a materialization that does not exist"))?;
            snapshot.tasks.push(join(materialization, entry));
        }
        snapshot.tasks.sort_by(|left, right| left.uuid.cmp(&right.uuid));
        Ok(snapshot)
    }

    /// Every stored record, indexed for joining and for invariant checks.
    fn read_records(&mut self) -> io::Result<StoredRecords> {
        let mut stored = StoredRecords::default();
        for (key, value) in self.journal.entries()? {
            check_pairing(&key, &value)?;
            match (key, value) {
                (_, RecordingDbValue::Metadata(metadata)) => stored.queue_revision = metadata.queue_revision,
                (RecordingDbKey::Materialization { id }, RecordingDbValue::Materialization(materialization)) => {
                    if id != materialization.id {
                        return Err(invalid("materialization is filed under a different id than it carries"));
                    }
                    let _ = stored.materializations.insert(id, *materialization);
                }
                (RecordingDbKey::LibraryEntry { id }, RecordingDbValue::LibraryEntry(entry)) => {
                    if id != entry.id {
                        return Err(invalid("library entry is filed under a different id than it carries"));
                    }
                    let _ = stored.entries.insert(id, *entry);
                }
                _ => return Err(invalid("recording record key and value describe different record kinds")),
            }
        }
        stored.check_reference_invariant()?;
        // Derived from the links, so a stale stored value is recomputed rather
        // than rejected. A wrong reference count risks deleting a file someone
        // still holds; a wrong priority only schedules badly.
        stored.recompute_effective_priorities();
        Ok(stored)
    }

    /// Replaces the committed set in one atomic batch.
    ///
    /// The queue mutates by rebuilding a whole candidate set, so the
    /// repository diffs that against what is stored and commits the
    /// difference. Committing the diff rather than the whole set keeps the
    /// recovery journal proportional to what actually changed.
    pub fn commit(&mut self, queue_revision: u64, tasks: &[PersistedRecordingTask]) -> io::Result<()> {
        let mut incoming = StoredRecords::default();
        for task in tasks {
            let (materialization, entry) = split(task);
            if incoming.entries.insert(entry.id.clone(), entry).is_some() {
                return Err(invalid("recording batch contains two tasks with the same uuid"));
            }
            // Two requests for the same media converge on one file; the first
            // one to arrive defines its physical state.
            let _ = incoming.materializations.entry(materialization.id.clone()).or_insert(materialization);
        }
        incoming.recount_references();
        incoming.recompute_effective_priorities();
        incoming.check_reference_invariant()?;

        let existing = self.read_records()?;
        let mut operations = Vec::new();
        for id in existing.entries.keys() {
            if !incoming.entries.contains_key(id) {
                operations.push(RecoveryOperation::Delete(RecordingDbKey::LibraryEntry { id: id.clone() }));
            }
        }
        for id in existing.materializations.keys() {
            if !incoming.materializations.contains_key(id) {
                operations.push(RecoveryOperation::Delete(RecordingDbKey::Materialization { id: id.clone() }));
            }
        }
        for (id, materialization) in &incoming.materializations {
            if existing.materializations.get(id) == Some(materialization) {
                continue;
            }
            operations.push(RecoveryOperation::Upsert(
                RecordingDbKey::Materialization { id: id.clone() },
                RecordingDbValue::Materialization(Box::new(materialization.clone())),
            ));
        }
        for (id, entry) in &incoming.entries {
            if existing.entries.get(id) == Some(entry) {
                continue;
            }
            operations.push(RecoveryOperation::Upsert(
                RecordingDbKey::LibraryEntry { id: id.clone() },
                RecordingDbValue::LibraryEntry(Box::new(entry.clone())),
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
        LibraryPrincipal, PersistedRecordingTask, RecordingDbKey, RecordingDbValue, RecordingPartition,
        RecordingRepository, RecordingVisibility,
    };
    use shared::model::{
        recording::{RecordingOwner, RecordingSource},
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
            media_identity: String::new(),
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
        // Each task is now a file plus a link, so two tasks are four records
        // plus the metadata one.
        assert_eq!(report.live_records, 5);
        Ok(())
    }

    #[test]
    fn a_task_round_trips_through_the_split() {
        // The DVR still works in whole tasks; splitting and rejoining them is
        // the repository's job, so the join must be lossless.
        let mut original = task("a");
        original.recording.reserved_bytes = 4_096;
        original.priority = -3;
        let (materialization, entry) = super::split(&original);
        assert_eq!(super::join(&materialization, &entry), original);
    }

    #[test]
    fn the_file_carries_no_owner_and_the_link_does() {
        let mut private = task("a");
        private.recording.visibility = RecordingVisibility::Private;
        let (_, entry) = super::split(&private);
        assert_eq!(entry.principal, LibraryPrincipal::User { id: "web:alice".to_owned() });

        let mut shared = task("b");
        shared.recording.visibility = RecordingVisibility::Shared;
        let (_, shared_entry) = super::split(&shared);
        assert_eq!(shared_entry.principal, LibraryPrincipal::Shared);
    }

    #[test]
    fn an_entry_id_and_a_materialization_id_are_different_namespaces() {
        // Once one file serves several users the two stop being in step, so
        // they must not be interchangeable even while the mapping is 1:1.
        let (materialization, entry) = super::split(&task("a"));
        assert_eq!(entry.id, "a");
        assert_ne!(materialization.id, entry.id);
        assert_eq!(entry.materialization_id, materialization.id);
    }

    #[test]
    fn a_committed_file_records_its_reference_count() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task("a"), task("b")])?;
        let stored = repository.read_records()?;
        assert_eq!(stored.materializations.len(), 2);
        assert_eq!(stored.entries.len(), 2);
        for materialization in stored.materializations.values() {
            assert_eq!(materialization.reference_count, 1);
        }
        Ok(())
    }

    #[test]
    fn a_reference_count_that_disagrees_with_the_links_fails_the_read() {
        // A file deleted while still reachable, or kept forever after its last
        // reference went away, cannot be resolved by guessing.
        let (mut materialization, entry) = super::split(&task("a"));
        materialization.reference_count = 3;
        let mut stored = super::StoredRecords::default();
        let _ = stored.materializations.insert(materialization.id.clone(), materialization);
        let _ = stored.entries.insert(entry.id.clone(), entry);
        assert!(stored.check_reference_invariant().is_err());

        stored.recount_references();
        assert!(stored.check_reference_invariant().is_ok());
    }

    #[test]
    fn a_link_to_a_missing_file_fails_the_read() {
        let (_, entry) = super::split(&task("a"));
        let mut stored = super::StoredRecords::default();
        let _ = stored.entries.insert(entry.id.clone(), entry);
        assert!(stored.check_reference_invariant().is_err());
    }

    /// The same media, requested by a different principal.
    fn task_for(uuid: &str, owner: &str, identity: &str) -> PersistedRecordingTask {
        let mut built = task(uuid);
        built.media_identity = identity.to_owned();
        built.recording.owner = RecordingOwner::User(UserId::from(owner));
        built
    }

    #[test]
    fn two_principals_asking_for_the_same_media_share_one_file() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(
            1,
            &[task_for("alice-entry", "web:alice", "film-42"), task_for("bob-entry", "web:bob", "film-42")],
        )?;

        let stored = repository.read_records()?;
        assert_eq!(stored.materializations.len(), 1, "one physical file");
        assert_eq!(stored.entries.len(), 2, "one link each");
        let materialization = stored.materializations.values().next().expect("a file");
        assert_eq!(materialization.reference_count, 2);
        Ok(())
    }

    #[test]
    fn each_principal_still_sees_their_own_entry() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(
            1,
            &[task_for("alice-entry", "web:alice", "film-42"), task_for("bob-entry", "web:bob", "film-42")],
        )?;

        let tasks = repository.load()?.tasks;
        assert_eq!(tasks.len(), 2, "a shared file is still two library entries");
        let owners: Vec<String> = tasks.iter().map(|task| task.recording.owner_id().0.clone()).collect();
        assert_eq!(owners, vec!["web:alice".to_owned(), "web:bob".to_owned()]);
        // Both point at the same bytes.
        assert_eq!(tasks[0].file_path, tasks[1].file_path);
        Ok(())
    }

    #[test]
    fn different_media_never_share_a_file() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task_for("a", "web:alice", "film-42"), task_for("b", "web:alice", "film-99")])?;
        assert_eq!(repository.read_records()?.materializations.len(), 2);
        Ok(())
    }

    #[test]
    fn detaching_one_principal_leaves_the_file_for_the_other() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(
            1,
            &[task_for("alice-entry", "web:alice", "film-42"), task_for("bob-entry", "web:bob", "film-42")],
        )?;
        // Alice removes hers; Bob's link and the file must both survive.
        repository.commit(2, &[task_for("bob-entry", "web:bob", "film-42")])?;

        let stored = repository.read_records()?;
        assert_eq!(stored.entries.len(), 1);
        assert_eq!(stored.materializations.len(), 1, "the file outlives the first reference");
        assert_eq!(stored.materializations.values().next().expect("a file").reference_count, 1);
        Ok(())
    }

    #[test]
    fn the_last_detach_removes_the_file() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task_for("alice-entry", "web:alice", "film-42")])?;
        repository.commit(2, &[])?;

        let stored = repository.read_records()?;
        assert_eq!(stored.entries.len(), 0);
        assert_eq!(stored.materializations.len(), 0, "nothing references it, so it is gone");
        Ok(())
    }

    #[test]
    fn a_task_without_an_identity_keeps_a_file_to_itself() -> io::Result<()> {
        // Falling back to the uuid is what stops two unidentified tasks from
        // silently colliding on one file.
        let fixture = Fixture::new()?;
        let mut repository = fixture.open()?;
        repository.commit(1, &[task("a"), task("b")])?;
        assert_eq!(repository.read_records()?.materializations.len(), 2);
        Ok(())
    }

    #[test]
    fn a_mismatched_key_and_value_is_rejected() {
        let (materialization, entry) = super::split(&task("a"));
        assert!(super::check_pairing(
            &RecordingDbKey::Metadata,
            &RecordingDbValue::Materialization(Box::new(materialization.clone())),
        )
        .is_err());
        assert!(super::check_pairing(
            &RecordingDbKey::Materialization { id: materialization.id.clone() },
            &RecordingDbValue::LibraryEntry(Box::new(entry.clone())),
        )
        .is_err());
        assert!(super::check_pairing(
            &RecordingDbKey::Materialization { id: materialization.id.clone() },
            &RecordingDbValue::Materialization(Box::new(materialization)),
        )
        .is_ok());
        assert!(super::check_pairing(
            &RecordingDbKey::LibraryEntry { id: entry.id.clone() },
            &RecordingDbValue::LibraryEntry(Box::new(entry)),
        )
        .is_ok());
    }

    #[test]
    fn two_entries_on_one_file_keep_their_own_states() -> io::Result<()> {
        // Execution state is per entry. Alice is running the transfer; Bob is
        // queued behind it on the same media. Holding state on the shared
        // materialization meant whichever entry was written last decided what
        // both of them looked like on the next load -- so Bob being queued
        // silently rewrote Alice as queued too, or the reverse.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (mut repository, _opened) = RecordingRepository::open(dir.path(), dir.path())?;

        let mut running = task("alice");
        running.recording.owner = RecordingOwner::User(UserId::from("web:alice"));
        running.state = RecordingTaskState::Running;
        running.partition = RecordingPartition::Active;
        running.finished = false;
        let mut queued = task("bob");
        queued.recording.owner = RecordingOwner::User(UserId::from("web:bob"));
        queued.state = RecordingTaskState::Queued;
        queued.partition = RecordingPartition::Queued;
        queued.finished = false;
        // Same media: one file, two links.
        running.media_identity = "programme-42".to_string();
        queued.media_identity = "programme-42".to_string();

        repository.commit(1, &[running, queued])?;
        let stored = repository.read_records()?;
        assert_eq!(stored.materializations.len(), 1, "one physical file");
        assert_eq!(stored.entries.len(), 2, "two links");

        let state_of = |id: &str| stored.entries.get(id).expect("entry").state;
        assert_eq!(state_of("alice"), RecordingTaskState::Running);
        assert_eq!(state_of("bob"), RecordingTaskState::Queued);
        Ok(())
    }

    /// Two entries on one file, at the given priorities. Lower is stronger.
    fn shared_media_at(priorities: [(&str, i8); 2]) -> Vec<PersistedRecordingTask> {
        priorities
            .into_iter()
            .map(|(user, priority)| {
                let mut task = task(user);
                task.recording.owner = RecordingOwner::User(UserId::from(format!("web:{user}").as_str()));
                task.priority = priority;
                task.media_identity = "programme-42".to_string();
                task
            })
            .collect()
    }

    #[test]
    fn a_file_runs_at_the_strongest_priority_any_entry_asked_for() -> io::Result<()> {
        // One transfer serves both entries. If it took the background value,
        // Bob's foreground request would wait behind Alice's background one for
        // bytes they both want.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (mut repository, _opened) = RecordingRepository::open(dir.path(), dir.path())?;
        repository.commit(1, &shared_media_at([("alice", 5), ("bob", -3)]))?;

        let stored = repository.read_records()?;
        let file = stored.materializations.values().next().expect("one file");
        assert_eq!(file.priority, -3, "the file runs at the strongest priority attached to it");
        Ok(())
    }

    #[test]
    fn losing_the_strongest_entry_relaxes_the_file() -> io::Result<()> {
        // The plan's case: remove the former maximum and the file must fall
        // back to what is still attached, not keep a priority nobody holds.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let (mut repository, _opened) = RecordingRepository::open(dir.path(), dir.path())?;
        let tasks = shared_media_at([("alice", 5), ("bob", -3)]);
        repository.commit(1, &tasks)?;

        let alice_only: Vec<_> = tasks.into_iter().filter(|task| task.uuid == "alice").collect();
        repository.commit(2, &alice_only)?;

        let stored = repository.read_records()?;
        let file = stored.materializations.values().next().expect("one file");
        assert_eq!(file.priority, 5, "with Bob gone the file is background work again");
        Ok(())
    }

    #[test]
    fn a_stale_stored_priority_is_recomputed_rather_than_trusted() {
        // Unlike the reference count, a wrong priority cannot lose data, so it
        // is corrected from the links instead of failing the read.
        let mut stored = super::StoredRecords::default();
        let (mut materialization, mut entry) = super::split(&task("a"));
        entry.priority = -4;
        materialization.priority = 99;
        let _ = stored.materializations.insert(materialization.id.clone(), materialization);
        let _ = stored.entries.insert(entry.id.clone(), entry);

        stored.recompute_effective_priorities();

        assert_eq!(stored.materializations.values().next().expect("a file").priority, -4);
    }

    #[test]
    fn records_are_encoded_by_field_name() -> io::Result<()> {
        // The recovery history must not depend on field order: that is the
        // whole reason it exists beside the positional B+Tree encoding.
        let (materialization, entry) = super::split(&task("a"));
        let encoded = serde_json::to_string(&RecordingDbValue::Materialization(Box::new(materialization)))?;
        // The file's own facts.
        for field in ["\"id\"", "\"kind\"", "\"file_path\"", "\"media\"", "\"reference_count\""] {
            assert!(encoded.contains(field), "{field} is missing from {encoded}");
        }
        let encoded = serde_json::to_string(&RecordingDbValue::LibraryEntry(Box::new(entry)))?;
        // The link's own facts, including where this entry is in its lifecycle.
        for field in
            ["\"id\"", "\"principal\"", "\"materialization_id\"", "\"quota_bytes\"", "\"state\"", "\"partition\""]
        {
            assert!(encoded.contains(field), "{field} is missing from {encoded}");
        }
        Ok(())
    }
}
