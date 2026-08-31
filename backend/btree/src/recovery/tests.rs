//! Crash and fault-injection tests for the journal-first commit order.
//!
//! These need to make a write fail at an exact point, which is only reachable
//! from inside the crate. Everything that can be observed from outside lives in
//! `tests/recovery_journal.rs`.

use super::{
    BPlusTreeRecoveryJournal, RecoveryBatch, RecoveryFaultPoint, RecoveryOpenAction, RecoveryOperation, RecoveryPaths,
    RecoveryPolicy, RecoveryRepositoryState, RecoverySchema,
};
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
