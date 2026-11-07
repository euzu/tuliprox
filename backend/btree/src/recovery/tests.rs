//! Crash and fault-injection tests for the journal-first commit order.
//!
//! These need to make a write fail at an exact point, which is only reachable
//! from inside the crate. Everything that can be observed from outside lives in
//! `tests/recovery_journal.rs`.

use super::{
    maintenance_lease_path, BPlusTreeRecoveryJournal, RecoveryBatch, RecoveryFaultPoint, RecoveryMaintenanceGuard,
    RecoveryOpenAction, RecoveryOperation, RecoveryPaths, RecoveryPolicy, RecoveryRepositoryState, RecoverySchema,
    RecoveryUseGuard,
};
use crate::{sidecar_lock_path, wal_path, wal_temporary_path};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use tempfile::TempDir;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Key(String);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Record {
    name: String,
}

struct Schema;

impl RecoverySchema<Key, Record> for Schema {
    const NAME: &'static str = "fault";
    const CURRENT_VERSION: u32 = 1;

    fn encode_key(&self, key: &Key) -> io::Result<Value> { to_value(key) }

    fn migrate_key_one(&self, _from: u32, key: Value) -> io::Result<Value> { Ok(key) }

    fn decode_current_key(&self, key: Value) -> io::Result<Key> { from_value(key) }

    fn encode_current(&self, value: &Record) -> io::Result<Value> { to_value(value) }

    fn migrate_one(&self, from: u32, _value: Value) -> io::Result<Value> {
        Err(io::Error::new(io::ErrorKind::InvalidData, format!("no migration from {from}")))
    }

    fn decode_current(&self, value: Value) -> io::Result<Record> { from_value(value) }
}

fn to_value<T: Serialize>(value: &T) -> io::Result<Value> {
    serde_json::to_value(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn from_value<T: for<'de> Deserialize<'de>>(value: Value) -> io::Result<T> {
    serde_json::from_value(value).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

type Journal = BPlusTreeRecoveryJournal<Key, Record, Schema>;

struct Fixture {
    _root: TempDir,
    paths: RecoveryPaths,
}

impl Fixture {
    fn new() -> io::Result<Self> {
        let root = TempDir::new()?;
        let paths =
            RecoveryPaths { database: root.path().join("db/fault.db"), directory: root.path().join("recovery") };
        Ok(Self { _root: root, paths })
    }

    fn open(&self) -> io::Result<(Journal, RecoveryOpenAction)> {
        let (journal, report) = Journal::open(self.paths.clone(), Schema, RecoveryPolicy::default())?;
        Ok((journal, report.action))
    }
}

fn batch(key: &str, name: &str) -> RecoveryBatch<Key, Record> {
    RecoveryBatch::new(vec![RecoveryOperation::Upsert(Key(key.to_owned()), Record { name: name.to_owned() })])
}

/// A failure before anything is appended must be indistinguishable from the
/// batch never having been submitted.
#[test]
fn commit_fault_before_journal_append_changes_nothing() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let (mut journal, _) = fixture.open()?;
    journal.inject_fault(Some(RecoveryFaultPoint::BeforeJournalAppend));
    assert!(journal.apply_batch(batch("a", "A")).is_err());
    assert_eq!(journal.health().state, RecoveryRepositoryState::Healthy);
    assert_eq!(journal.health().current_revision, 1);

    journal.inject_fault(None);
    assert_eq!(journal.apply_batch(batch("a", "A"))?.revision, 2);
    Ok(())
}

#[test]
fn commit_fault_during_journal_append_requires_repair() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        let _ = journal.apply_batch(batch("a", "A"))?;
        journal.inject_fault(Some(RecoveryFaultPoint::DuringJournalAppend));
        assert!(journal.apply_batch(batch("b", "B")).is_err());
        assert_eq!(journal.health().state, RecoveryRepositoryState::RepairRequired);
        // Every later mutation is blocked until the repository is reopened.
        journal.inject_fault(None);
        assert!(journal.apply_batch(batch("c", "C")).is_err());
    }
    // The partial frame is dead weight and is dropped; the committed record survives.
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 1);
    Ok(())
}

#[test]
fn commit_fault_before_journal_sync_requires_repair() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let (mut journal, _) = fixture.open()?;
    journal.inject_fault(Some(RecoveryFaultPoint::BeforeJournalSync));
    assert!(journal.apply_batch(batch("a", "A")).is_err());
    assert_eq!(journal.health().state, RecoveryRepositoryState::RepairRequired);
    Ok(())
}

/// Once the journal record is durable the mutation is committed, so reopening
/// must reproduce it even though the database never saw the write.
#[test]
fn commit_fault_after_journal_sync_is_recovered_on_reopen() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        journal.inject_fault(Some(RecoveryFaultPoint::AfterJournalSync));
        assert!(journal.apply_batch(batch("a", "A")).is_err());
        assert_eq!(journal.health().state, RecoveryRepositoryState::RepairRequired);
    }
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Rebuilt);
    assert_eq!(journal.entries()?, vec![(Key("a".into()), Record { name: "A".into() })]);
    Ok(())
}

#[test]
fn commit_fault_during_database_batch_is_recovered_on_reopen() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        journal.inject_fault(Some(RecoveryFaultPoint::DuringDatabaseBatch));
        assert!(journal.apply_batch(batch("a", "A")).is_err());
        assert_eq!(journal.health().state, RecoveryRepositoryState::RepairRequired);
    }
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Rebuilt);
    assert_eq!(journal.entries()?.len(), 1);
    Ok(())
}

#[test]
fn commit_fault_during_database_commit_is_recovered_on_reopen() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        let _ = journal.apply_batch(batch("a", "A"))?;
        journal.inject_fault(Some(RecoveryFaultPoint::DuringDatabaseCommit));
        assert!(journal.apply_batch(batch("b", "B")).is_err());
    }
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Rebuilt);
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

/// A failure after the database commit loses nothing: both histories already
/// agree, so the next open is an ordinary one.
#[test]
fn commit_fault_after_database_commit_is_already_durable() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        journal.inject_fault(Some(RecoveryFaultPoint::AfterDatabaseCommit));
        assert!(journal.apply_batch(batch("a", "A")).is_err());
    }
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 1);
    Ok(())
}

/// A crash cannot expose half of a logical batch.
#[test]
fn commit_mixed_batch_survives_a_database_fault_intact() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![
            RecoveryOperation::Upsert(Key("a".into()), Record { name: "A".into() }),
            RecoveryOperation::Upsert(Key("b".into()), Record { name: "B0".into() }),
        ]))?;
        journal.inject_fault(Some(RecoveryFaultPoint::DuringDatabaseBatch));
        let mixed = RecoveryBatch::new(vec![
            RecoveryOperation::Delete(Key("a".into())),
            RecoveryOperation::Upsert(Key("b".into()), Record { name: "B1".into() }),
            RecoveryOperation::Upsert(Key("c".into()), Record { name: "C".into() }),
        ]);
        assert!(journal.apply_batch(mixed).is_err());
    }
    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Rebuilt);
    assert_eq!(
        journal.entries()?,
        vec![(Key("b".into()), Record { name: "B1".into() }), (Key("c".into()), Record { name: "C".into() }),]
    );
    Ok(())
}

/// Writes a database the way the code did before recovery existed: real rows,
/// no recovery metadata.
fn write_legacy_database(fixture: &Fixture, rows: &[(&str, &str)]) -> io::Result<()> {
    if let Some(parent) = fixture.paths.database.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tree = crate::BPlusTree::<Key, Record>::new();
    for (key, name) in rows {
        tree.insert(Key((*key).to_owned()), Record { name: (*name).to_owned() });
    }
    let _ = tree.store(&fixture.paths.database)?;
    Ok(())
}

/// The upgrade path every existing install takes: a database that predates
/// recovery must be adopted, not mistaken for corruption.
#[test]
fn a_database_written_before_recovery_is_adopted_with_every_row() -> io::Result<()> {
    let fixture = Fixture::new()?;
    write_legacy_database(&fixture, &[("a", "A"), ("b", "B"), ("c", "C")])?;

    let (mut journal, action) = fixture.open()?;
    assert_eq!(action, RecoveryOpenAction::Adopted);
    assert_eq!(
        journal.entries()?,
        vec![
            (Key("a".into()), Record { name: "A".into() }),
            (Key("b".into()), Record { name: "B".into() }),
            (Key("c".into()), Record { name: "C".into() }),
        ]
    );
    assert_eq!(journal.health().state, RecoveryRepositoryState::Healthy);
    Ok(())
}

/// Adoption happens once. The second open finds a tracked database with a
/// matching generation and simply opens it.
#[test]
fn an_adopted_database_is_merely_opened_the_next_time() -> io::Result<()> {
    let fixture = Fixture::new()?;
    write_legacy_database(&fixture, &[("a", "A")])?;
    let (_, first) = fixture.open()?;
    assert_eq!(first, RecoveryOpenAction::Adopted);

    let (mut journal, second) = fixture.open()?;
    assert_eq!(second, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 1);
    // Adoption left a writable repository behind, not a read-only snapshot.
    assert_eq!(journal.apply_batch(batch("b", "B"))?.revision, 2);
    Ok(())
}

/// The distinction adoption rests on: a file that cannot be read is still
/// refused. Without this, "adopt anything unrecognised" would silently discard
/// a corrupt database's contents.
#[test]
fn a_database_that_cannot_be_read_is_still_refused() -> io::Result<()> {
    let fixture = Fixture::new()?;
    if let Some(parent) = fixture.paths.database.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&fixture.paths.database, b"this is not a b+tree")?;

    let Err(error) = fixture.open() else {
        panic!("an unreadable database must not be adopted");
    };
    assert!(error.to_string().contains("unreadable"), "the error must name corruption, not adoption: {error}");
    Ok(())
}

#[test]
fn lease_exclusivity_rejects_concurrent_maintenance_guards() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let first = RecoveryMaintenanceGuard::try_acquire(&fixture.paths)?;
    let second = RecoveryMaintenanceGuard::try_acquire(&fixture.paths);
    assert!(
        matches!(second, Err(ref error) if error.kind() == io::ErrorKind::WouldBlock),
        "second exclusive acquisition must fail with WouldBlock: {second:?}"
    );
    drop(first);
    let after_drop = RecoveryMaintenanceGuard::try_acquire(&fixture.paths)?;
    drop(after_drop);
    Ok(())
}

#[test]
fn shared_use_guard_and_exclusive_maintenance_guard_exclude_each_other() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let use_guard = RecoveryUseGuard::try_acquire(&fixture.paths)?;
    let maintenance_attempt = RecoveryMaintenanceGuard::try_acquire(&fixture.paths);
    assert!(
        matches!(maintenance_attempt, Err(ref error) if error.kind() == io::ErrorKind::WouldBlock),
        "maintenance guard must fail with WouldBlock while use guard is held: {maintenance_attempt:?}"
    );
    drop(use_guard);

    let maintenance_guard = RecoveryMaintenanceGuard::try_acquire(&fixture.paths)?;
    let use_attempt = RecoveryUseGuard::try_acquire(&fixture.paths);
    assert!(
        matches!(use_attempt, Err(ref error) if error.kind() == io::ErrorKind::WouldBlock),
        "use guard must fail with WouldBlock while maintenance guard is held: {use_attempt:?}"
    );
    drop(maintenance_guard);
    Ok(())
}

#[test]
fn maintenance_lease_path_is_stable_and_distinct_from_database_artifacts() -> io::Result<()> {
    let fixture = Fixture::new()?;
    let lease_path_1 = maintenance_lease_path(&fixture.paths)?;
    let lease_path_2 = maintenance_lease_path(&fixture.paths)?;
    assert_eq!(lease_path_1, lease_path_2, "maintenance_lease_path must be stable");

    assert_ne!(lease_path_1, fixture.paths.database);
    assert_ne!(lease_path_1, sidecar_lock_path(&fixture.paths.database));

    let _guard = RecoveryMaintenanceGuard::try_acquire(&fixture.paths)?;
    let lease_resolved = crate::common::resolved_path_identity(&lease_path_1)?;
    let db_resolved = crate::common::resolved_path_identity(&fixture.paths.database)?;
    let lock_resolved = crate::common::resolved_path_identity(&sidecar_lock_path(&fixture.paths.database))?;

    assert_ne!(lease_resolved, db_resolved, "lease path must not resolve to the database file itself");
    assert_ne!(lease_resolved, lock_resolved, "lease path must not resolve to the sidecar lock");
    Ok(())
}

#[test]
fn verify_rebuild_history_never_modifies_a_database_it_refuses() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        let _ = journal.apply_batch(batch("a", "one"))?;
    }
    let mut db_bytes = std::fs::read(&fixture.paths.database)?;
    let corrupt_len = 64.min(db_bytes.len());
    db_bytes[..corrupt_len].fill(0);
    std::fs::write(&fixture.paths.database, &db_bytes)?;
    let recorded_bytes = std::fs::read(&fixture.paths.database)?;

    assert!(BPlusTreeRecoveryJournal::verify_rebuild_history(&fixture.paths, &Schema).is_err());
    assert_eq!(
        std::fs::read(&fixture.paths.database)?,
        recorded_bytes,
        "database file bytes must be unchanged after verify_rebuild_history refusal"
    );
    assert!(!wal_path(&fixture.paths.database).try_exists()?, "no WAL file must appear after verify_rebuild_history");
    assert!(
        !wal_temporary_path(&fixture.paths.database).try_exists()?,
        "no temporary WAL file must appear after verify_rebuild_history"
    );
    Ok(())
}

#[test]
fn read_rebuild_history_returns_committed_records_and_reports_matching_revisions() -> io::Result<()> {
    let fixture = Fixture::new()?;
    {
        let (mut journal, _) = fixture.open()?;
        let _ = journal.apply_batch(batch("a", "one"))?;
        let _ = journal.apply_batch(batch("b", "two"))?;
    }
    let db_bytes_before = std::fs::read(&fixture.paths.database)?;

    let (report, records) = BPlusTreeRecoveryJournal::read_rebuild_history(&fixture.paths, &Schema)?;
    assert_eq!(report.database_revision, report.recovery_revision);
    assert_eq!(report.live_records, 2);

    let mut expected = std::collections::BTreeMap::new();
    expected.insert(Key("a".to_owned()), Record { name: "one".to_owned() });
    expected.insert(Key("b".to_owned()), Record { name: "two".to_owned() });
    assert_eq!(records, expected);

    assert_eq!(
        std::fs::read(&fixture.paths.database)?,
        db_bytes_before,
        "database file bytes must be unchanged after read_rebuild_history"
    );
    Ok(())
}
