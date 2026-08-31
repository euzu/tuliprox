//! A recoverable wrapper around the operational B+Tree.
//!
//! The B+Tree's own write-ahead log makes a single commit atomic. It cannot
//! rebuild a database whose value type has changed, because its cells hold
//! positional `MessagePack`. This wrapper adds a second, independent history of
//! field-named JSON records so a database can be reconstructed from any schema
//! version the application still knows how to migrate.

mod format;
mod generation;
mod schema;
#[cfg(test)]
mod tests;

use crate::{
    publish_staged_database,
    v3::{BPlusTree, BPlusTreeMetadata, BPlusTreeQuery, BPlusTreeUpdate, FlushPolicy, RecoveryIdentity},
};
use format::{
    chain, encode_frame, invalid_data, read_frames, DecodedFrame, JsonOperation, RecordBody, RecordEnvelope,
    GENESIS_HASH, MAX_OPERATIONS,
};
use generation::{
    prune_generations, read_checkpoint_entries, read_current, scan_generations, scan_journal, write_current,
    CheckpointWriter, GenerationPaths, GenerationScan, RETAINED_GENERATIONS,
};
pub use schema::RecoverySchema;
use schema::{migrate_key_to_current, migrate_value_to_current, schema_fingerprint};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The two directories a recoverable database occupies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryPaths {
    /// The operational B+Tree file.
    pub database: PathBuf,
    /// The directory that holds recovery generations. Placing it on a distinct
    /// filesystem is what makes recovery survive the loss of the database
    /// volume; the wrapper reports the placement but never enforces it.
    pub directory: PathBuf,
}

/// One key/value mutation in a logical batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOperation<K, V> {
    Upsert(K, V),
    Delete(K),
}

impl<K, V> RecoveryOperation<K, V> {
    fn key(&self) -> &K {
        match self {
            Self::Upsert(key, _) | Self::Delete(key) => key,
        }
    }
}

/// A set of mutations that commit together or not at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryBatch<K, V> {
    pub operations: Vec<RecoveryOperation<K, V>>,
}

impl<K, V> RecoveryBatch<K, V> {
    pub fn new(operations: Vec<RecoveryOperation<K, V>>) -> Self { Self { operations } }
}

/// When a checkpoint replaces the accumulated journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub max_journal_bytes: u64,
    pub max_transactions: u64,
    pub max_checkpoint_age: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_journal_bytes: 64 * 1024 * 1024,
            max_transactions: 100_000,
            max_checkpoint_age: Duration::from_hours(24),
        }
    }
}

/// What `open` had to do to reach a usable database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryOpenAction {
    Created,
    Opened,
    Rebuilt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryOpenReport {
    pub action: RecoveryOpenAction,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryCommitReport {
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointOutcome {
    NotNeeded,
    Created { revision: u64, pruned_generations: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryRepositoryState {
    Healthy,
    RepairRequired,
    Rebuilding,
}

/// Whether recovery data shares a failure domain with the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryStoragePlacement {
    SameFilesystem,
    DistinctFilesystem,
    Unknown,
}

/// A redacted classification of the last failure. Never carries a path, a value
/// or a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryErrorClass {
    Io,
    Corruption,
    SchemaMismatch,
    MigrationFailed,
    ForkedHistory,
    DatabaseAhead,
    UncertainWrite,
    PublishFailed,
}

/// Operational status, safe to expose to an operator tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryHealth {
    pub state: RecoveryRepositoryState,
    pub current_revision: u64,
    pub database_revision: u64,
    pub recovery_lag: u64,
    pub last_verified_checkpoint_revision: u64,
    pub journal_bytes: u64,
    pub last_error: Option<RecoveryErrorClass>,
    pub storage_placement: RecoveryStoragePlacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryVerificationReport {
    pub database_revision: u64,
    pub recovery_revision: u64,
    pub live_records: u64,
}

/// Points at which a test can make a write fail or become uncertain.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryFaultPoint {
    BeforeJournalAppend,
    DuringJournalAppend,
    BeforeJournalSync,
    AfterJournalSync,
    DuringDatabaseBatch,
    DuringDatabaseCommit,
    AfterDatabaseCommit,
}

/// The recoverable database handle.
///
/// It owns the only mutation path: nothing else may write the operational
/// B+Tree, because a write that bypasses the journal cannot be recovered.
pub struct BPlusTreeRecoveryJournal<K, V, S> {
    paths: RecoveryPaths,
    schema: S,
    policy: RecoveryPolicy,
    database_id: String,
    generation: u64,
    journal_path: PathBuf,
    journal: Option<File>,
    journal_bytes: u64,
    journal_transactions: u64,
    head_hash: [u8; 32],
    current_revision: u64,
    database_revision: u64,
    checkpoint_revision: u64,
    checkpoint_created_at: SystemTime,
    state: RecoveryRepositoryState,
    last_error: Option<RecoveryErrorClass>,
    storage_placement: RecoveryStoragePlacement,
    updater: Option<BPlusTreeUpdate<K, V>>,
    #[cfg(any(test, feature = "test-support"))]
    fault: Option<RecoveryFaultPoint>,
    _types: PhantomData<(K, V)>,
}

impl<K, V, S> BPlusTreeRecoveryJournal<K, V, S>
where
    K: Ord + Clone + Serialize + DeserializeOwned,
    V: Clone + Serialize + DeserializeOwned,
    S: RecoverySchema<K, V>,
{
    /// Opens, creates or rebuilds the database so that it matches the newest
    /// verifiable recovery history.
    pub fn open(paths: RecoveryPaths, schema: S, policy: RecoveryPolicy) -> io::Result<(Self, RecoveryOpenReport)> {
        fs::create_dir_all(&paths.directory)?;
        if let Some(parent) = paths.database.parent() {
            fs::create_dir_all(parent)?;
        }
        let placement = detect_placement(&paths);
        let database_identity = read_database_identity::<K, V>(&paths.database);
        let fingerprint = schema_fingerprint(S::NAME);

        let database_id = match &database_identity {
            Some(identity) => {
                if identity.schema_fingerprint != fingerprint {
                    return Err(schema_mismatch());
                }
                if identity.schema_version > S::CURRENT_VERSION {
                    return Err(invalid_data(
                        "the operational database was written by a newer schema version than this build supports",
                    ));
                }
                hex16(identity.database_id)
            }
            None => discover_database_id(&paths.directory, S::NAME)?.unwrap_or_else(new_database_id),
        };

        let generations = scan_generations(&paths.directory, S::NAME, &database_id)?;
        let mut journal = Self {
            paths,
            schema,
            policy,
            database_id,
            generation: 0,
            journal_path: PathBuf::new(),
            journal: None,
            journal_bytes: 0,
            journal_transactions: 0,
            head_hash: GENESIS_HASH,
            current_revision: 0,
            database_revision: 0,
            checkpoint_revision: 0,
            checkpoint_created_at: SystemTime::now(),
            state: RecoveryRepositoryState::Healthy,
            last_error: None,
            storage_placement: placement,
            updater: None,
            #[cfg(any(test, feature = "test-support"))]
            fault: None,
            _types: PhantomData,
        };

        let report = journal.reconcile(&generations, database_identity)?;
        Ok((journal, report))
    }

    fn reconcile(
        &mut self,
        generations: &[GenerationScan],
        database_identity: Option<RecoveryIdentity>,
    ) -> io::Result<RecoveryOpenReport> {
        let selected = select_generation(generations).inspect_err(|_| {
            self.fail(RecoveryErrorClass::ForkedHistory);
        })?;

        let Some(selected) = selected else {
            if database_identity.is_some() {
                self.fail(RecoveryErrorClass::DatabaseAhead);
                return Err(invalid_data(
                    "the operational database exists but no verifiable recovery generation was found",
                ));
            }
            if self.paths.database.exists() {
                self.fail(RecoveryErrorClass::Corruption);
                return Err(invalid_data("the operational database is unreadable and no recovery data exists"));
            }
            let revision = self.create_empty()?;
            return Ok(RecoveryOpenReport { action: RecoveryOpenAction::Created, revision });
        };

        self.adopt_generation(selected)?;
        if selected.manifest.schema_version > S::CURRENT_VERSION {
            self.fail(RecoveryErrorClass::SchemaMismatch);
            return Err(invalid_data("the newest recovery generation was written by an unsupported future schema"));
        }
        // A crash between publishing a generation and updating the pointer
        // leaves `CURRENT` stale; the scan is authoritative, so repair it.
        if read_current(&self.paths.directory)? != Some(selected.generation) {
            write_current(&self.paths.directory, selected.generation)?;
        }

        match database_identity {
            Some(identity)
                if identity.applied_revision == self.current_revision
                    && identity.schema_version == S::CURRENT_VERSION =>
            {
                self.database_revision = identity.applied_revision;
                self.state = RecoveryRepositoryState::Healthy;
                Ok(RecoveryOpenReport { action: RecoveryOpenAction::Opened, revision: self.current_revision })
            }
            Some(identity) if identity.applied_revision > self.current_revision => {
                self.fail(RecoveryErrorClass::DatabaseAhead);
                Err(invalid_data("the operational database is ahead of every recovery generation"))
            }
            _ => {
                self.state = RecoveryRepositoryState::Rebuilding;
                self.rebuild(selected)?;
                Ok(RecoveryOpenReport { action: RecoveryOpenAction::Rebuilt, revision: self.current_revision })
            }
        }
    }

    fn adopt_generation(&mut self, selected: &GenerationScan) -> io::Result<()> {
        self.generation = selected.generation;
        self.journal_path.clone_from(&selected.paths.journal);
        self.journal = None;
        self.journal_bytes = selected.journal.bytes;
        self.journal_transactions = selected.journal.transactions;
        self.head_hash = selected.journal.head_hash;
        self.current_revision = selected.journal.revision;
        self.checkpoint_revision = selected.manifest.checkpoint_revision;
        self.checkpoint_created_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(selected.manifest.created_at_unix))
            .unwrap_or_else(SystemTime::now);
        // A torn final frame is dead weight: truncate so the next append starts
        // from a byte offset that the chain actually covers.
        if selected.journal.torn_tail {
            let file = OpenOptions::new().write(true).open(&self.journal_path)?;
            file.set_len(self.journal_bytes)?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn create_empty(&mut self) -> io::Result<u64> {
        let revision = 1;
        self.generation = 1;
        self.publish_checkpoint(std::iter::empty(), revision, 0)?;
        let mut tree = BPlusTree::<K, V>::new();
        tree.set_metadata(BPlusTreeMetadata::Recovery(self.identity(revision)));
        let _ = tree.store(&self.paths.database)?;
        self.current_revision = revision;
        self.database_revision = revision;
        self.state = RecoveryRepositoryState::Healthy;
        Ok(revision)
    }

    fn identity(&self, applied_revision: u64) -> RecoveryIdentity {
        RecoveryIdentity {
            database_id: unhex16(&self.database_id),
            schema_fingerprint: schema_fingerprint(S::NAME),
            schema_version: S::CURRENT_VERSION,
            applied_revision,
        }
    }

    fn fail(&mut self, class: RecoveryErrorClass) {
        self.state = RecoveryRepositoryState::RepairRequired;
        self.last_error = Some(class);
        self.updater = None;
    }

    fn ensure_mutable(&self) -> io::Result<()> {
        match self.state {
            RecoveryRepositoryState::Healthy => Ok(()),
            RecoveryRepositoryState::RepairRequired => {
                Err(io::Error::other("recovery repository requires repair; reopen it before mutating again"))
            }
            RecoveryRepositoryState::Rebuilding => Err(io::Error::other("recovery repository is rebuilding")),
        }
    }

    /// Commits one logical batch: journal first, then the B+Tree.
    ///
    /// The journal is made durable before the database is touched, so a crash
    /// can only ever leave recovery ahead - a state that `open` repairs by
    /// rebuilding - and never leave the database ahead of its own history.
    pub fn apply_batch(&mut self, batch: RecoveryBatch<K, V>) -> io::Result<RecoveryCommitReport> {
        self.ensure_mutable()?;
        let operations = self.encode_batch(&batch.operations)?;
        let revision = self.current_revision.checked_add(1).ok_or_else(|| invalid_data("revision overflow"))?;
        let envelope = RecordEnvelope::new(
            S::NAME,
            S::CURRENT_VERSION,
            &self.database_id,
            revision,
            self.head_hash,
            RecordBody::Transaction { operations },
        );
        let (frame, frame_hash) = encode_frame(&envelope)?;

        self.check_fault(RecoveryFaultPointCheck::BeforeJournalAppend)?;
        self.append_frame(&frame)?;

        // Everything past this point has a durable journal record, so any
        // failure must leave the repository requiring repair rather than
        // silently dropping a committed mutation.
        let head_hash = chain(self.head_hash, frame_hash);
        match self.apply_to_database(batch.operations, revision) {
            Ok(()) => {}
            Err(error) => {
                self.fail(RecoveryErrorClass::UncertainWrite);
                return Err(error);
            }
        }

        self.head_hash = head_hash;
        self.current_revision = revision;
        self.database_revision = revision;
        self.journal_bytes = self
            .journal_bytes
            .checked_add(u64::try_from(frame.len()).map_err(|_| invalid_data("frame length exceeds u64"))?)
            .ok_or_else(|| invalid_data("journal size overflow"))?;
        self.journal_transactions =
            self.journal_transactions.checked_add(1).ok_or_else(|| invalid_data("transaction count overflow"))?;

        self.check_fault(RecoveryFaultPointCheck::AfterDatabaseCommit)?;
        Ok(RecoveryCommitReport { revision })
    }

    fn encode_batch(&self, operations: &[RecoveryOperation<K, V>]) -> io::Result<Vec<JsonOperation>> {
        if operations.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "recovery batch must not be empty"));
        }
        if operations.len() > MAX_OPERATIONS {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "recovery batch exceeds the operation limit"));
        }
        let mut seen = BTreeSet::new();
        for operation in operations {
            let encoded = self.schema.encode_key(operation.key())?;
            let text = serde_json::to_string(&encoded).map_err(|error| invalid_data(error.to_string()))?;
            if !seen.insert(text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recovery batch touches the same key more than once",
                ));
            }
        }
        operations
            .iter()
            .map(|operation| match operation {
                RecoveryOperation::Upsert(key, value) => Ok(JsonOperation::Upsert {
                    key: self.schema.encode_key(key)?,
                    value: self.schema.encode_current(value)?,
                }),
                RecoveryOperation::Delete(key) => Ok(JsonOperation::Delete { key: self.schema.encode_key(key)? }),
            })
            .collect()
    }

    fn append_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        if self.journal.is_none() {
            self.journal = Some(OpenOptions::new().create(true).append(true).open(&self.journal_path)?);
        }
        let file = self.journal.as_mut().ok_or_else(|| invalid_data("recovery journal handle is missing"))?;

        #[cfg(any(test, feature = "test-support"))]
        if self.fault == Some(RecoveryFaultPoint::DuringJournalAppend) {
            let half = frame.len().checked_div(2).unwrap_or(0).max(1).min(frame.len());
            let partial = frame.get(..half).unwrap_or(frame);
            file.write_all(partial)?;
            file.flush()?;
            file.sync_all()?;
            self.fail(RecoveryErrorClass::UncertainWrite);
            return Err(io::Error::other("injected fault: journal append interrupted"));
        }

        let write_result = file.write_all(frame).and_then(|()| file.flush());
        if let Err(error) = write_result {
            self.fail(RecoveryErrorClass::UncertainWrite);
            return Err(error);
        }

        #[cfg(any(test, feature = "test-support"))]
        if self.fault == Some(RecoveryFaultPoint::BeforeJournalSync) {
            self.fail(RecoveryErrorClass::UncertainWrite);
            return Err(io::Error::other("injected fault: before journal sync"));
        }

        if let Err(error) = self.sync_and_verify_journal() {
            self.fail(RecoveryErrorClass::UncertainWrite);
            return Err(error);
        }

        #[cfg(any(test, feature = "test-support"))]
        if self.fault == Some(RecoveryFaultPoint::AfterJournalSync) {
            self.fail(RecoveryErrorClass::UncertainWrite);
            return Err(io::Error::other("injected fault: after journal sync"));
        }
        Ok(())
    }

    fn sync_and_verify_journal(&mut self) -> io::Result<()> {
        let file = self.journal.as_mut().ok_or_else(|| invalid_data("recovery journal handle is missing"))?;
        file.sync_all()?;
        let metadata = fs::metadata(&self.journal_path)?;
        let expected = self.journal_bytes;
        if metadata.len() < expected {
            return Err(invalid_data("recovery journal shrank after sync"));
        }
        Ok(())
    }

    fn apply_to_database(&mut self, operations: Vec<RecoveryOperation<K, V>>, revision: u64) -> io::Result<()> {
        self.check_fault(RecoveryFaultPointCheck::DuringDatabaseBatch)?;
        let identity = self.identity(revision);
        let updater = self.updater()?;
        updater.set_flush_policy(FlushPolicy::Batch);
        let mut result = Ok(());
        for operation in operations {
            result = match operation {
                RecoveryOperation::Upsert(key, value) => updater.upsert(&key, &value).map(|_| ()),
                RecoveryOperation::Delete(key) => updater.delete(&key).map(|_| ()),
            };
            if result.is_err() {
                break;
            }
        }
        if result.is_ok() {
            result = updater.set_metadata(&BPlusTreeMetadata::Recovery(identity));
        }
        if result.is_ok() {
            result = self.check_fault(RecoveryFaultPointCheck::DuringDatabaseCommit);
        }
        let updater = self.updater()?;
        if result.is_ok() {
            result = updater.commit();
        }
        if result.is_err() {
            self.updater = None;
        }
        result
    }

    fn updater(&mut self) -> io::Result<&mut BPlusTreeUpdate<K, V>> {
        if self.updater.is_none() {
            self.updater = Some(BPlusTreeUpdate::<K, V>::try_new(&self.paths.database)?);
        }
        self.updater.as_mut().ok_or_else(|| invalid_data("B+Tree updater is missing"))
    }

    /// Replaces the journal with a checkpoint when a policy threshold is met.
    pub fn checkpoint_if_needed(&mut self) -> io::Result<CheckpointOutcome> {
        self.ensure_mutable()?;
        if !self.checkpoint_needed() {
            return Ok(CheckpointOutcome::NotNeeded);
        }
        self.checkpoint_now()
    }

    fn checkpoint_needed(&self) -> bool {
        if self.journal_transactions == 0 {
            return false;
        }
        let aged = SystemTime::now()
            .duration_since(self.checkpoint_created_at)
            .is_ok_and(|age| age >= self.policy.max_checkpoint_age);
        self.journal_bytes >= self.policy.max_journal_bytes
            || self.journal_transactions >= self.policy.max_transactions
            || aged
    }

    /// Writes a fresh generation from the current live database contents.
    pub fn checkpoint_now(&mut self) -> io::Result<CheckpointOutcome> {
        self.ensure_mutable()?;
        let verified = self.verify()?;
        if verified.database_revision != verified.recovery_revision {
            self.fail(RecoveryErrorClass::Corruption);
            return Err(invalid_data("cannot checkpoint while the database and journal disagree"));
        }
        let revision = self.current_revision;
        self.updater = None;

        let entries = self.live_entries()?;
        let records = u64::try_from(entries.len()).map_err(|_| invalid_data("checkpoint record count exceeds u64"))?;
        let next_generation =
            self.generation.checked_add(1).ok_or_else(|| invalid_data("recovery generation overflow"))?;
        self.generation = next_generation;
        self.publish_checkpoint(entries.into_iter(), revision, records)?;

        let keep = self.retained_generations();
        let pruned = prune_generations(&self.paths.directory, &keep)?;

        // A checkpoint that cannot be read back is not allowed to become the
        // only surviving history, so replay it before returning.
        let restored =
            self.restore_state(&GenerationPaths::new(&self.paths.directory, next_generation), S::CURRENT_VERSION)?;
        if u64::try_from(restored.len()).unwrap_or(u64::MAX) != records {
            self.fail(RecoveryErrorClass::Corruption);
            return Err(invalid_data("checkpoint dry-run restore returned an unexpected record count"));
        }
        Ok(CheckpointOutcome::Created { revision, pruned_generations: pruned })
    }

    fn retained_generations(&self) -> Vec<u64> {
        let mut keep = Vec::with_capacity(RETAINED_GENERATIONS);
        keep.push(self.generation);
        if let Some(previous) = self.generation.checked_sub(1) {
            if previous > 0 {
                keep.push(previous);
            }
        }
        keep
    }

    fn publish_checkpoint(
        &mut self,
        entries: impl Iterator<Item = (K, V)>,
        revision: u64,
        records: u64,
    ) -> io::Result<()> {
        let mut writer = CheckpointWriter::create(
            &self.paths.directory,
            self.generation,
            S::NAME,
            S::CURRENT_VERSION,
            &self.database_id,
            revision,
            records,
        )?;
        for (key, value) in entries {
            writer.push_entry(self.schema.encode_key(&key)?, self.schema.encode_current(&value)?)?;
        }
        let (paths, manifest) = writer.finish(self.generation)?;
        write_current(&self.paths.directory, self.generation)?;
        self.journal_path = paths.journal;
        self.journal = None;
        self.journal_bytes = 0;
        self.journal_transactions = 0;
        self.head_hash = manifest.checkpoint_hash_bytes()?;
        self.checkpoint_revision = revision;
        self.checkpoint_created_at = SystemTime::now();
        Ok(())
    }

    fn live_entries(&mut self) -> io::Result<Vec<(K, V)>> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(&self.paths.database)?;
        query.iter().collect()
    }

    /// Reads every live entry. Callers outside this crate use it to project a
    /// committed in-memory view after `open`.
    pub fn entries(&mut self) -> io::Result<Vec<(K, V)>> { self.live_entries() }

    pub fn query(&mut self, key: &K) -> io::Result<Option<V>> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(&self.paths.database)?;
        query.query(key).map_err(crate::BPlusTreeError::to_io)
    }

    /// Confirms that the database and the recovery history agree.
    pub fn verify(&mut self) -> io::Result<RecoveryVerificationReport> {
        let identity = read_database_identity::<K, V>(&self.paths.database)
            .ok_or_else(|| invalid_data("the operational database carries no recovery identity"))?;
        if identity.schema_fingerprint != schema_fingerprint(S::NAME) {
            return Err(schema_mismatch());
        }
        let scan = scan_journal(
            &self.journal_path,
            S::NAME,
            &self.database_id,
            self.checkpoint_revision,
            self.checkpoint_head_hash()?,
        )?;
        let mut query = BPlusTreeQuery::<K, V>::try_new(&self.paths.database)?;
        let live = query.len().map_err(crate::BPlusTreeError::to_io)?;
        Ok(RecoveryVerificationReport {
            database_revision: identity.applied_revision,
            recovery_revision: scan.revision,
            live_records: u64::try_from(live).map_err(|_| invalid_data("live record count exceeds u64"))?,
        })
    }

    fn checkpoint_head_hash(&self) -> io::Result<[u8; 32]> {
        let paths = GenerationPaths::new(&self.paths.directory, self.generation);
        generation::read_manifest(&paths.manifest, S::NAME)?.checkpoint_hash_bytes()
    }

    pub fn health(&self) -> RecoveryHealth {
        RecoveryHealth {
            state: self.state,
            current_revision: self.current_revision,
            database_revision: self.database_revision,
            recovery_lag: self.current_revision.saturating_sub(self.database_revision),
            last_verified_checkpoint_revision: self.checkpoint_revision,
            journal_bytes: self.journal_bytes,
            last_error: self.last_error,
            storage_placement: self.storage_placement,
        }
    }

    /// Replays a generation into an in-memory map at the current schema version.
    fn restore_state(&self, paths: &GenerationPaths, checkpoint_version: u32) -> io::Result<BTreeMap<K, V>> {
        let mut state = BTreeMap::new();
        read_checkpoint_entries(&paths.checkpoint, S::NAME, &self.database_id, |key, value| {
            let key = migrate_key_to_current::<K, V, S>(&self.schema, checkpoint_version, key)?;
            let value = migrate_value_to_current::<K, V, S>(&self.schema, checkpoint_version, value)?;
            if state.insert(key, value).is_some() {
                return Err(invalid_data("checkpoint holds duplicate keys after migration"));
            }
            Ok(())
        })?;
        self.replay_journal(&paths.journal, &mut state)?;
        Ok(state)
    }

    fn replay_journal(&self, path: &Path, state: &mut BTreeMap<K, V>) -> io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let mut reader = std::io::BufReader::new(File::open(path)?);
        let _ = read_frames(&mut reader, |frame: DecodedFrame| {
            frame.envelope.validate_identity(S::NAME, &self.database_id)?;
            let version = frame.envelope.schema_version;
            let RecordBody::Transaction { operations } = frame.envelope.body else {
                return Err(invalid_data("recovery journal holds a non-transaction record"));
            };
            for operation in operations {
                match operation {
                    JsonOperation::Upsert { key, value } => {
                        let key = migrate_key_to_current::<K, V, S>(&self.schema, version, key)?;
                        let value = migrate_value_to_current::<K, V, S>(&self.schema, version, value)?;
                        let _ = state.insert(key, value);
                    }
                    JsonOperation::Delete { key } => {
                        let key = migrate_key_to_current::<K, V, S>(&self.schema, version, key)?;
                        let _ = state.remove(&key);
                    }
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Rebuilds the operational database from a generation, through staging.
    ///
    /// The published database is only replaced once the staging copy verifies,
    /// so a failed rebuild leaves the previous database exactly as it was.
    fn rebuild(&mut self, selected: &GenerationScan) -> io::Result<()> {
        self.updater = None;
        let state = match self.restore_state(&selected.paths, selected.manifest.schema_version) {
            Ok(state) => state,
            Err(error) => {
                self.fail(RecoveryErrorClass::MigrationFailed);
                return Err(error);
            }
        };
        let revision = selected.journal.revision;
        let staging = staging_path(&self.paths.database)?;
        let _ = fs::remove_file(&staging);

        let build = (|| -> io::Result<()> {
            let mut tree = BPlusTree::<K, V>::new();
            tree.set_metadata(BPlusTreeMetadata::Recovery(self.identity(revision)));
            for (key, value) in state {
                tree.insert(key, value);
            }
            let _ = tree.store(&staging)?;
            let mut query = BPlusTreeQuery::<K, V>::try_new(&staging)?;
            let _ = query.len().map_err(crate::BPlusTreeError::to_io)?;
            Ok(())
        })();
        if let Err(error) = build {
            let _ = fs::remove_file(&staging);
            self.fail(RecoveryErrorClass::Corruption);
            return Err(error);
        }

        if let Err(error) = publish_staged_database::<K, V>(&staging, &self.paths.database) {
            let _ = fs::remove_file(&staging);
            self.fail(RecoveryErrorClass::PublishFailed);
            return Err(error);
        }
        self.database_revision = revision;
        self.current_revision = revision;
        self.state = RecoveryRepositoryState::Healthy;
        self.last_error = None;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_fault(&mut self, point: Option<RecoveryFaultPoint>) { self.fault = point; }

    #[cfg(any(test, feature = "test-support"))]
    fn check_fault(&mut self, point: RecoveryFaultPointCheck) -> io::Result<()> {
        let matched = match point {
            RecoveryFaultPointCheck::BeforeJournalAppend => self.fault == Some(RecoveryFaultPoint::BeforeJournalAppend),
            RecoveryFaultPointCheck::DuringDatabaseBatch => self.fault == Some(RecoveryFaultPoint::DuringDatabaseBatch),
            RecoveryFaultPointCheck::DuringDatabaseCommit => {
                self.fault == Some(RecoveryFaultPoint::DuringDatabaseCommit)
            }
            RecoveryFaultPointCheck::AfterDatabaseCommit => self.fault == Some(RecoveryFaultPoint::AfterDatabaseCommit),
        };
        if matched {
            return Err(io::Error::other("injected recovery fault"));
        }
        Ok(())
    }

    // Fault injection is compiled out of a normal build, which leaves this
    // variant trivially successful and independent of `self`.
    #[cfg(not(any(test, feature = "test-support")))]
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn check_fault(&mut self, _point: RecoveryFaultPointCheck) -> io::Result<()> { Ok(()) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoveryFaultPointCheck {
    BeforeJournalAppend,
    DuringDatabaseBatch,
    DuringDatabaseCommit,
    AfterDatabaseCommit,
}

fn schema_mismatch() -> io::Error {
    invalid_data("the operational database was written by a different recovery schema")
}

fn staging_path(database: &Path) -> io::Result<PathBuf> {
    let name =
        database.file_name().and_then(|name| name.to_str()).ok_or_else(|| invalid_data("database has no name"))?;
    let parent = database.parent().ok_or_else(|| invalid_data("database has no parent directory"))?;
    Ok(parent.join(format!("{name}.recovery-staging")))
}

fn read_database_identity<K, V>(database: &Path) -> Option<RecoveryIdentity>
where
    K: Ord + Clone + Serialize + DeserializeOwned,
    V: Serialize + DeserializeOwned,
{
    let query = BPlusTreeQuery::<K, V>::try_new(database).ok()?;
    match query.metadata() {
        BPlusTreeMetadata::Recovery(identity) => Some(*identity),
        _ => None,
    }
}

/// Reads the database identity a generation was written for, so a database that
/// was deleted outright can still be rebuilt under its original identity.
fn discover_database_id(directory: &Path, schema_name: &str) -> io::Result<Option<String>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut newest: Option<(u64, String)> = None;
    for entry in entries {
        let entry = entry?;
        let manifest = entry.path().join("manifest.bin");
        let Ok(manifest) = generation::read_manifest(&manifest, schema_name) else { continue };
        if newest.as_ref().is_none_or(|(generation, _)| manifest.generation > *generation) {
            newest = Some((manifest.generation, manifest.database_id));
        }
    }
    Ok(newest.map(|(_, id)| id))
}

/// Picks the unique highest valid history, or refuses to guess.
fn select_generation(generations: &[GenerationScan]) -> io::Result<Option<&GenerationScan>> {
    let mut best: Option<&GenerationScan> = None;
    for candidate in generations {
        match best {
            None => best = Some(candidate),
            Some(current) => {
                if candidate.order_key() > current.order_key() {
                    best = Some(candidate);
                }
            }
        }
    }
    let Some(best) = best else { return Ok(None) };
    let ambiguous = generations
        .iter()
        .filter(|candidate| candidate.order_key() == best.order_key())
        .any(|candidate| candidate.journal.head_hash != best.journal.head_hash);
    if ambiguous {
        return Err(invalid_data("two recovery generations claim the same revision with different histories"));
    }
    Ok(Some(best))
}

fn detect_placement(paths: &RecoveryPaths) -> RecoveryStoragePlacement {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let database = paths.database.parent().unwrap_or(&paths.database);
        match (fs::metadata(database), fs::metadata(&paths.directory)) {
            (Ok(left), Ok(right)) if left.dev() == right.dev() => RecoveryStoragePlacement::SameFilesystem,
            (Ok(_), Ok(_)) => RecoveryStoragePlacement::DistinctFilesystem,
            _ => RecoveryStoragePlacement::Unknown,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        RecoveryStoragePlacement::Unknown
    }
}

fn new_database_id() -> String { hex16(*uuid::Uuid::new_v4().as_bytes()) }

fn hex16(bytes: [u8; 16]) -> String { format::hex_bytes(&bytes) }

fn unhex16(text: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let bytes = text.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let start = index.saturating_mul(2);
        let Some(pair) = bytes.get(start..start.saturating_add(2)) else { break };
        let Ok(pair) = std::str::from_utf8(pair) else { break };
        *slot = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    out
}
