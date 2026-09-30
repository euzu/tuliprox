//! Public contract, durability, generation-selection, compaction and schema
//! migration tests for the recoverable B+Tree wrapper.
//!
//! Everything here goes through the crate's public API only. Nothing in this
//! file may name an application type: the wrapper has to be usable by any
//! caller that can describe its records as field-named JSON.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use tuliprox_btree::{
    BPlusTreeRecoveryJournal, CheckpointOutcome, RecoveryBatch, RecoveryErrorClass, RecoveryOpenAction,
    RecoveryOperation, RecoveryPaths, RecoveryPolicy, RecoveryRepositoryState, RecoverySchema,
    RecoveryStoragePlacement,
};

// --- Test domain -----------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    id: String,
}

impl Key {
    fn new(id: &str) -> Self { Self { id: id.to_owned() } }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct V1 {
    name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct V2 {
    name: String,
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct V3 {
    display_name: String,
    enabled: bool,
    tags: Vec<String>,
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.to_owned()) }

fn encode<T: Serialize>(value: &T) -> io::Result<Value> {
    serde_json::to_value(value).map_err(|error| invalid(&error.to_string()))
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> io::Result<T> {
    serde_json::from_value(value).map_err(|error| invalid(&error.to_string()))
}

/// The V1 to V2 step: a record gains a field with an explicit default.
fn step_v1_to_v2(value: Value) -> io::Result<Value> {
    let v1: V1 = decode(value)?;
    encode(&V2 { name: v1.name, enabled: true })
}

/// The V2 to V3 step: a field is renamed and another is added.
fn step_v2_to_v3(value: Value) -> io::Result<Value> {
    let v2: V2 = decode(value)?;
    encode(&V3 { display_name: v2.name, enabled: v2.enabled, tags: Vec::new() })
}

macro_rules! key_schema {
    () => {
        fn encode_key(&self, key: &Key) -> io::Result<Value> { encode(key) }

        fn migrate_key_one(&self, _from: u32, key: Value) -> io::Result<Value> { Ok(key) }

        fn decode_current_key(&self, key: Value) -> io::Result<Key> { decode(key) }
    };
}

struct SchemaV1;

impl RecoverySchema<Key, V1> for SchemaV1 {
    const NAME: &'static str = "fixture";
    const CURRENT_VERSION: u32 = 1;

    key_schema!();

    fn encode_current(&self, value: &V1) -> io::Result<Value> { encode(value) }

    fn migrate_one(&self, from: u32, _value: Value) -> io::Result<Value> {
        Err(invalid(&format!("schema v1 has no migration from {from}")))
    }

    fn decode_current(&self, value: Value) -> io::Result<V1> { decode(value) }
}

struct SchemaV2;

impl RecoverySchema<Key, V2> for SchemaV2 {
    const NAME: &'static str = "fixture";
    const CURRENT_VERSION: u32 = 2;

    key_schema!();

    fn encode_current(&self, value: &V2) -> io::Result<Value> { encode(value) }

    fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value> {
        match from {
            1 => step_v1_to_v2(value),
            other => Err(invalid(&format!("schema v2 has no migration from {other}"))),
        }
    }

    fn decode_current(&self, value: Value) -> io::Result<V2> { decode(value) }
}

struct SchemaV3;

impl RecoverySchema<Key, V3> for SchemaV3 {
    const NAME: &'static str = "fixture";
    const CURRENT_VERSION: u32 = 3;

    key_schema!();

    fn encode_current(&self, value: &V3) -> io::Result<Value> { encode(value) }

    fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value> {
        match from {
            1 => step_v1_to_v2(value),
            2 => step_v2_to_v3(value),
            other => Err(invalid(&format!("schema v3 has no migration from {other}"))),
        }
    }

    fn decode_current(&self, value: Value) -> io::Result<V3> { decode(value) }
}

/// A schema whose migration chain is deliberately incomplete.
struct SchemaV3MissingStep;

impl RecoverySchema<Key, V3> for SchemaV3MissingStep {
    const NAME: &'static str = "fixture";
    const CURRENT_VERSION: u32 = 3;

    key_schema!();

    fn encode_current(&self, value: &V3) -> io::Result<Value> { encode(value) }

    fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value> {
        match from {
            2 => step_v2_to_v3(value),
            other => Err(invalid(&format!("no migration step from {other}"))),
        }
    }

    fn decode_current(&self, value: Value) -> io::Result<V3> { decode(value) }
}

type JournalV1 = BPlusTreeRecoveryJournal<Key, V1, SchemaV1>;
type JournalV2 = BPlusTreeRecoveryJournal<Key, V2, SchemaV2>;
type JournalV3 = BPlusTreeRecoveryJournal<Key, V3, SchemaV3>;

// --- Harness ---------------------------------------------------------------

struct Harness {
    _root: TempDir,
    paths: RecoveryPaths,
}

impl Harness {
    fn new() -> io::Result<Self> {
        let root = TempDir::new()?;
        let paths = RecoveryPaths {
            database: root.path().join("db").join("fixture.db"),
            directory: root.path().join("recovery"),
        };
        Ok(Self { _root: root, paths })
    }

    fn paths(&self) -> RecoveryPaths { self.paths.clone() }

    fn open_v3(&self) -> io::Result<JournalV3> {
        JournalV3::open(self.paths(), SchemaV3, RecoveryPolicy::default()).map(|(journal, _)| journal)
    }

    /// The newest generation directory that currently exists on disk.
    fn newest_generation(&self) -> io::Result<PathBuf> {
        let mut best: Option<PathBuf> = None;
        for entry in fs::read_dir(&self.paths.directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && best.as_ref().is_none_or(|current| entry.path() > *current) {
                best = Some(entry.path());
            }
        }
        best.ok_or_else(|| invalid("no recovery generation exists"))
    }

    fn journal_file(&self) -> io::Result<PathBuf> { Ok(self.newest_generation()?.join("journal.bin")) }

    fn generation_count(&self) -> io::Result<usize> {
        let mut count = 0;
        for entry in fs::read_dir(&self.paths.directory)? {
            if entry?.file_type()?.is_dir() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn recovery_bytes(&self) -> io::Result<u64> { directory_size(&self.paths.directory) }
}

fn directory_size(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        total += if metadata.is_dir() { directory_size(&entry.path())? } else { metadata.len() };
    }
    Ok(total)
}

fn upsert_v3(key: &str, display_name: &str) -> RecoveryOperation<Key, V3> {
    RecoveryOperation::Upsert(
        Key::new(key),
        V3 { display_name: display_name.to_owned(), enabled: true, tags: Vec::new() },
    )
}

fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let _ = File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn fixture(name: &str) -> io::Result<Vec<u8>> {
    read_bytes(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recovery").join(name))
}

fn fixture_pairs(name: &str) -> io::Result<Vec<(Value, Value)>> {
    #[derive(Deserialize)]
    struct Pair {
        key: Value,
        value: Value,
    }
    let bytes = fixture(name)?;
    let pairs: Vec<Pair> = serde_json::from_slice(&bytes).map_err(|error| invalid(&error.to_string()))?;
    Ok(pairs.into_iter().map(|pair| (pair.key, pair.value)).collect())
}

// --- Public contract -------------------------------------------------------

#[test]
fn contract_open_apply_checkpoint_verify_health() -> io::Result<()> {
    let harness = Harness::new()?;
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Created);
    assert_eq!(opened.revision, 1);

    let committed = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    assert_eq!(committed.revision, 2);

    assert_eq!(journal.checkpoint_if_needed()?, CheckpointOutcome::NotNeeded);

    let verified = journal.verify()?;
    assert_eq!(verified.database_revision, 2);
    assert_eq!(verified.recovery_revision, 2);
    assert_eq!(verified.live_records, 1);

    let health = journal.health();
    assert_eq!(health.state, RecoveryRepositoryState::Healthy);
    assert_eq!(health.current_revision, 2);
    assert_eq!(health.recovery_lag, 0);
    assert_eq!(health.last_error, None);
    Ok(())
}

#[test]
fn contract_reopen_sees_committed_state() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ =
            journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha"), upsert_v3("beta", "Beta")]))?;
    }
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(journal.query(&Key::new("alpha"))?.map(|value| value.display_name), Some("Alpha".to_owned()));
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

#[test]
fn contract_rejects_empty_and_duplicate_batches() -> io::Result<()> {
    let harness = Harness::new()?;
    let mut journal = harness.open_v3()?;
    assert!(journal.apply_batch(RecoveryBatch::new(Vec::new())).is_err());
    let duplicate = RecoveryBatch::new(vec![upsert_v3("alpha", "One"), upsert_v3("alpha", "Two")]);
    assert!(journal.apply_batch(duplicate).is_err());
    // A rejected batch is not a durability failure, so mutation stays allowed.
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    Ok(())
}

// --- Record format ---------------------------------------------------------

#[test]
fn format_records_are_field_named_json() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    let bytes = read_bytes(&harness.journal_file()?)?;
    let text = String::from_utf8_lossy(&bytes);
    for field in ["format_version", "schema_name", "schema_version", "database_id", "revision", "previous_hash"] {
        assert!(text.contains(field), "journal record is missing the {field} field");
    }
    assert!(text.contains("\"display_name\":\"Alpha\""), "value is not stored by field name");
    Ok(())
}

#[test]
fn format_truncated_final_record_is_ignored() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    // A crash during the append leaves a frame that never got its payload. The
    // committed records in front of it must stay readable.
    let path = harness.journal_file()?;
    let committed_len = fs::metadata(&path)?.len();
    let head = read_bytes(&path)?.into_iter().take(30).collect::<Vec<_>>();
    OpenOptions::new().append(true).open(&path)?.write_all(&head)?;
    assert!(fs::metadata(&path)?.len() > committed_len);

    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(opened.revision, 3);
    assert_eq!(journal.entries()?.len(), 2);
    // The dead bytes are dropped so the next append continues the hash chain.
    assert_eq!(fs::metadata(&path)?.len(), committed_len);
    Ok(())
}

#[test]
fn format_database_ahead_of_a_truncated_journal_fails_closed() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    let path = harness.journal_file()?;
    let length = fs::metadata(&path)?.len();
    OpenOptions::new().write(true).open(&path)?.set_len(length - 20)?;
    assert!(JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()).is_err());
    Ok(())
}

#[test]
fn format_corrupt_complete_record_fails() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    let path = harness.journal_file()?;
    flip_byte(&path, 60)?;
    // A generation whose journal does not decode is not a candidate at all, and
    // the database is then ahead of every remaining history.
    assert!(JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()).is_err());
    Ok(())
}

#[test]
fn format_oversized_length_is_rejected_before_allocation() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    let path = harness.journal_file()?;
    let mut file = OpenOptions::new().write(true).open(&path)?;
    // The length field sits directly behind the four magic bytes.
    let _ = file.seek(SeekFrom::Start(4))?;
    file.write_all(&u32::MAX.to_le_bytes())?;
    file.sync_all()?;
    assert!(JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()).is_err());
    Ok(())
}

#[test]
fn format_invalid_magic_is_rejected() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    let mut file = OpenOptions::new().write(true).open(harness.journal_file()?)?;
    file.write_all(b"XXXX")?;
    file.sync_all()?;
    assert!(JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()).is_err());
    Ok(())
}

#[test]
fn format_revision_and_chain_are_contiguous() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        for index in 0..4 {
            let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3(&format!("k{index}"), "value")]))?;
        }
    }
    let bytes = read_bytes(&harness.journal_file()?)?;
    let mut revisions = Vec::new();
    let mut previous_hashes = Vec::new();
    for value in decode_journal_payloads(&bytes)? {
        revisions.push(value.get("revision").and_then(Value::as_u64).unwrap_or_default());
        previous_hashes.push(value.get("previous_hash").and_then(Value::as_str).unwrap_or_default().to_owned());
    }
    assert_eq!(revisions, vec![2, 3, 4, 5]);
    assert_eq!(previous_hashes.len(), 4);
    // Every record links to a distinct predecessor.
    let mut sorted = previous_hashes.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), previous_hashes.len());
    Ok(())
}

/// Splits a framed journal into its JSON payloads without depending on crate
/// internals: magic, length, hash, payload.
fn decode_journal_payloads(bytes: &[u8]) -> io::Result<Vec<Value>> {
    let mut payloads = Vec::new();
    let mut offset = 0usize;
    while offset + 40 <= bytes.len() {
        let length_bytes: [u8; 4] =
            bytes[offset + 4..offset + 8].try_into().map_err(|_| invalid("bad frame length"))?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        let start = offset + 40;
        let end = start + length;
        if end > bytes.len() {
            break;
        }
        payloads.push(serde_json::from_slice(&bytes[start..end]).map_err(|error| invalid(&error.to_string()))?);
        offset = end;
    }
    Ok(payloads)
}

fn flip_byte(path: &Path, offset: u64) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let _ = file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0xff;
    let _ = file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    file.sync_all()
}

// --- Commit ordering -------------------------------------------------------

#[test]
fn commit_batch_is_all_or_nothing() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("a", "A"), upsert_v3("b", "B0")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![
            RecoveryOperation::Delete(Key::new("a")),
            upsert_v3("b", "B1"),
            upsert_v3("c", "C"),
        ]))?;
    }
    let mut journal = harness.open_v3()?;
    let entries = journal.entries()?;
    let names: Vec<_> = entries.iter().map(|(key, value)| (key.id.clone(), value.display_name.clone())).collect();
    assert_eq!(names, vec![("b".to_owned(), "B1".to_owned()), ("c".to_owned(), "C".to_owned())]);
    Ok(())
}

#[test]
fn commit_journal_ahead_of_database_rebuilds_on_open() -> io::Result<()> {
    let harness = Harness::new()?;
    let database_copy = {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        read_bytes(&harness.paths.database)?
    };
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    // Roll the database back to before the second batch, leaving the journal ahead.
    fs::write(&harness.paths.database, &database_copy)?;

    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Rebuilt);
    assert_eq!(opened.revision, 3);
    assert_eq!(journal.entries()?.len(), 2);
    assert_eq!(journal.health().state, RecoveryRepositoryState::Healthy);
    Ok(())
}

#[test]
fn commit_database_ahead_of_recovery_fails_closed() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    let journal_path = harness.journal_file()?;
    OpenOptions::new().write(true).open(&journal_path)?.set_len(0)?;
    assert!(JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()).is_err());
    Ok(())
}

#[test]
fn commit_missing_recovery_and_missing_database_creates_empty() -> io::Result<()> {
    let harness = Harness::new()?;
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Created);
    assert_eq!(journal.entries()?.len(), 0);
    Ok(())
}

#[test]
fn commit_deleted_database_is_rebuilt_from_recovery() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    fs::remove_file(&harness.paths.database)?;

    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Rebuilt);
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

// --- Generations -----------------------------------------------------------

fn eager_policy() -> RecoveryPolicy {
    RecoveryPolicy { max_journal_bytes: 1, max_transactions: 1, max_checkpoint_age: Duration::from_hours(24) }
}

#[test]
fn generation_checkpoint_publishes_and_resets_the_journal() -> io::Result<()> {
    let harness = Harness::new()?;
    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    let outcome = journal.checkpoint_if_needed()?;
    assert_eq!(outcome, CheckpointOutcome::Created { revision: 2, pruned_generations: 0 });
    assert_eq!(journal.health().journal_bytes, 0);
    assert_eq!(journal.health().last_verified_checkpoint_revision, 2);

    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    drop(journal);

    let (mut reopened, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(reopened.entries()?.len(), 2);
    Ok(())
}

#[test]
fn generation_retains_only_current_and_previous() -> io::Result<()> {
    let harness = Harness::new()?;
    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    for index in 0..5 {
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3(&format!("k{index}"), "value")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    assert_eq!(harness.generation_count()?, 2);
    Ok(())
}

#[test]
fn generation_stale_current_pointer_is_repaired() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    let current = harness.paths.directory.join("CURRENT");
    fs::write(&current, "gen-00000000000000000001\n")?;

    let (_journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(fs::read_to_string(&current)?.trim(), "gen-00000000000000000002");
    Ok(())
}

#[test]
fn generation_missing_current_pointer_still_selects_the_newest() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    fs::remove_file(harness.paths.directory.join("CURRENT"))?;
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 1);
    Ok(())
}

#[test]
fn generation_torn_staging_checkpoint_falls_back_to_previous() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    // Tear the newest checkpoint the way an interrupted publication would.
    let newest = harness.newest_generation()?;
    let checkpoint = newest.join("checkpoint.bin");
    let length = fs::metadata(&checkpoint)?.len();
    OpenOptions::new().write(true).open(&checkpoint)?.set_len(length / 2)?;

    // The previous generation's own journal still carries everything committed
    // after its checkpoint, so falling back loses no revision.
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(opened.revision, 3);
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

#[test]
fn generation_corrupt_newest_falls_back_to_previous() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    let newest = harness.newest_generation()?;
    fs::write(newest.join("manifest.bin"), b"not a manifest")?;

    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

#[test]
fn generation_losing_every_history_at_the_database_revision_fails_closed() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
        let _ = journal.checkpoint_if_needed()?;
    }
    // Destroy the newest generation and the tail of the previous one, so no
    // surviving history reaches the revision the database already applied.
    fs::remove_dir_all(harness.newest_generation()?)?;
    let previous = harness.newest_generation()?;
    OpenOptions::new().write(true).open(previous.join("journal.bin"))?.set_len(0)?;

    assert!(JournalV3::open(harness.paths(), SchemaV3, eager_policy()).is_err());
    Ok(())
}

#[test]
fn generation_tampered_duplicate_history_is_never_selected() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_if_needed()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("beta", "Beta")]))?;
    }
    // A generation directory is bound to the number its own hashed manifest
    // claims, so a copy placed under another number can never be adopted - and
    // a copy that keeps its number cannot be edited without breaking the chain.
    let newest = harness.newest_generation()?;
    let fork = harness.paths.directory.join("gen-00000000000000000099");
    copy_directory(&newest, &fork)?;
    let mut tampered = read_bytes(&newest.join("journal.bin"))?;
    if let Some(last) = tampered.last_mut() {
        *last ^= 0x01;
    }
    fs::write(fork.join("journal.bin"), &tampered)?;

    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?.len(), 2);
    Ok(())
}

fn copy_directory(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            let _ = fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

// --- Compaction ------------------------------------------------------------

#[test]
fn compaction_removes_dead_history() -> io::Result<()> {
    let harness = Harness::new()?;
    let policy =
        RecoveryPolicy { max_journal_bytes: u64::MAX, max_transactions: u64::MAX, max_checkpoint_age: Duration::MAX };
    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, policy)?;

    for round in 0..400 {
        let key = format!("live-{}", round % 5);
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3(&key, &format!("round-{round}"))]))?;
        let scratch = format!("scratch-{round}");
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3(&scratch, "temporary")]))?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Delete(Key::new(&scratch))]))?;
    }
    let before = harness.recovery_bytes()?;
    let expected = journal.entries()?;
    assert_eq!(expected.len(), 5);

    let outcome = journal.checkpoint_now()?;
    assert!(matches!(outcome, CheckpointOutcome::Created { .. }));
    let newest = harness.newest_generation()?;
    let checkpoint = String::from_utf8_lossy(&read_bytes(&newest.join("checkpoint.bin"))?).to_string();
    assert!(!checkpoint.contains("scratch-"), "checkpoint still holds deleted records");

    // The generation holding the dead history is only dropped once a second
    // verified generation exists behind it.
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("live-0", "final")]))?;
    let _ = journal.checkpoint_now()?;
    assert_eq!(harness.generation_count()?, 2);
    let after = harness.recovery_bytes()?;
    assert!(after * 4 < before, "compaction did not bound recovery storage: {before} -> {after}");

    drop(journal);
    let (mut reopened, _) = JournalV3::open(harness.paths(), SchemaV3, policy)?;
    let restored = reopened.entries()?;
    assert_eq!(restored.len(), 5);
    assert_eq!(restored.iter().map(|(key, _)| key.id.clone()).collect::<Vec<_>>(), expected_keys());
    Ok(())
}

fn expected_keys() -> Vec<String> { (0..5).map(|index| format!("live-{index}")).collect() }

#[test]
fn compaction_triggers_on_transaction_count() -> io::Result<()> {
    let harness = Harness::new()?;
    let policy = RecoveryPolicy { max_journal_bytes: u64::MAX, max_transactions: 3, max_checkpoint_age: Duration::MAX };
    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, policy)?;
    for index in 0..2 {
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3(&format!("k{index}"), "v")]))?;
        assert_eq!(journal.checkpoint_if_needed()?, CheckpointOutcome::NotNeeded);
    }
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("k2", "v")]))?;
    assert!(matches!(journal.checkpoint_if_needed()?, CheckpointOutcome::Created { .. }));
    Ok(())
}

#[test]
fn compaction_triggers_on_checkpoint_age() -> io::Result<()> {
    let harness = Harness::new()?;
    let policy =
        RecoveryPolicy { max_journal_bytes: u64::MAX, max_transactions: u64::MAX, max_checkpoint_age: Duration::ZERO };
    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, policy)?;
    // A zero age threshold still requires at least one journalled transaction.
    assert_eq!(journal.checkpoint_if_needed()?, CheckpointOutcome::NotNeeded);
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    assert!(matches!(journal.checkpoint_if_needed()?, CheckpointOutcome::Created { .. }));
    Ok(())
}

// --- Schema migration ------------------------------------------------------

#[test]
fn migration_v1_checkpoint_restores_as_v3() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV1::open(harness.paths(), SchemaV1, eager_policy())?;
        for (key, value) in fixture_pairs("v1-checkpoint.json")? {
            let key: Key = decode(key)?;
            let value: V1 = decode(value)?;
            let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Upsert(key, value)]))?;
        }
        let _ = journal.checkpoint_now()?;
    }
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Rebuilt);
    let entries = journal.entries()?;
    assert_eq!(
        entries,
        vec![
            (Key::new("alpha"), V3 { display_name: "Alpha".into(), enabled: true, tags: Vec::new() }),
            (Key::new("beta"), V3 { display_name: "Beta".into(), enabled: true, tags: Vec::new() }),
            (Key::new("gamma"), V3 { display_name: "Gamma".into(), enabled: true, tags: Vec::new() }),
        ]
    );
    Ok(())
}

#[test]
fn migration_v2_checkpoint_restores_as_v3() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV2::open(harness.paths(), SchemaV2, eager_policy())?;
        for (key, value) in fixture_pairs("v2-checkpoint.json")? {
            let key: Key = decode(key)?;
            let value: V2 = decode(value)?;
            let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Upsert(key, value)]))?;
        }
        let _ = journal.checkpoint_now()?;
    }
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Rebuilt);
    let entries = journal.entries()?;
    assert_eq!(entries[1].1, V3 { display_name: "Beta".into(), enabled: false, tags: Vec::new() });
    assert_eq!(entries.len(), 3);
    Ok(())
}

#[test]
fn migration_v3_checkpoint_restores_unchanged() -> io::Result<()> {
    let harness = Harness::new()?;
    let expected: Vec<(Key, V3)> = fixture_pairs("v3-checkpoint.json")?
        .into_iter()
        .map(|(key, value)| Ok((decode::<Key>(key)?, decode::<V3>(value)?)))
        .collect::<io::Result<_>>()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        for (key, value) in expected.clone() {
            let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Upsert(key, value)]))?;
        }
        let _ = journal.checkpoint_now()?;
    }
    let (mut journal, opened) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
    assert_eq!(opened.action, RecoveryOpenAction::Opened);
    assert_eq!(journal.entries()?, expected);
    Ok(())
}

#[test]
fn migration_mixed_version_journal_resolves_to_one_v3_state() -> io::Result<()> {
    #[derive(Deserialize)]
    struct Line {
        schema_version: u32,
        operations: Vec<Value>,
    }

    let harness = Harness::new()?;
    let bytes = fixture("mixed-v1-v2-v3.jsonl")?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let lines: Vec<Line> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|error| invalid(&error.to_string())))
        .collect::<io::Result<_>>()?;

    // Each line is written by the build that was current at the time, so the
    // one journal ends up holding V1, V2 and V3 records together.
    for line in &lines {
        match line.schema_version {
            1 => {
                let (mut journal, _) = JournalV1::open(harness.paths(), SchemaV1, RecoveryPolicy::default())?;
                let _ = journal.apply_batch(build_batch::<V1>(&line.operations)?)?;
            }
            2 => {
                let (mut journal, _) = JournalV2::open(harness.paths(), SchemaV2, RecoveryPolicy::default())?;
                let _ = journal.apply_batch(build_batch::<V2>(&line.operations)?)?;
            }
            _ => {
                let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
                let _ = journal.apply_batch(build_batch::<V3>(&line.operations)?)?;
            }
        }
    }

    let journal_text = String::from_utf8_lossy(&read_bytes(&harness.journal_file()?)?).to_string();
    for version in ["\"schema_version\":1", "\"schema_version\":2", "\"schema_version\":3"] {
        assert!(journal_text.contains(version), "journal is missing {version}");
    }

    let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default())?;
    let entries = journal.entries()?;
    assert_eq!(
        entries,
        vec![
            (Key::new("beta"), V3 { display_name: "Beta".into(), enabled: true, tags: Vec::new() }),
            (Key::new("delta"), V3 { display_name: "Delta".into(), enabled: true, tags: vec!["new".into()] }),
            (Key::new("gamma"), V3 { display_name: "Gamma".into(), enabled: false, tags: Vec::new() }),
        ]
    );
    Ok(())
}

fn build_batch<T>(operations: &[Value]) -> io::Result<RecoveryBatch<Key, T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut built = Vec::new();
    for operation in operations {
        let key: Key = decode(operation.get("key").cloned().unwrap_or(Value::Null))?;
        if operation.get("op").and_then(Value::as_str) == Some("delete") {
            built.push(RecoveryOperation::Delete(key));
        } else {
            let value: T = decode(operation.get("value").cloned().unwrap_or(Value::Null))?;
            built.push(RecoveryOperation::Upsert(key, value));
        }
    }
    Ok(RecoveryBatch::new(built))
}

#[test]
fn migration_missing_step_fails_without_replacing_the_database() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV1::open(harness.paths(), SchemaV1, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Upsert(
            Key::new("alpha"),
            V1 { name: "Alpha".into() },
        )]))?;
        let _ = journal.checkpoint_now()?;
    }
    let before = read_bytes(&harness.paths.database)?;
    let result = BPlusTreeRecoveryJournal::<Key, V3, SchemaV3MissingStep>::open(
        harness.paths(),
        SchemaV3MissingStep,
        eager_policy(),
    );
    assert!(result.is_err(), "a broken migration chain must not be accepted");
    assert_eq!(read_bytes(&harness.paths.database)?, before, "the database was replaced by a failed rebuild");
    Ok(())
}

#[test]
fn migration_future_schema_version_is_refused() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV3::open(harness.paths(), SchemaV3, eager_policy())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
        let _ = journal.checkpoint_now()?;
    }
    // An older build meets a database and history written by a newer one.
    let result = JournalV1::open(harness.paths(), SchemaV1, eager_policy());
    assert!(result.is_err(), "an older build must refuse a newer schema instead of guessing");
    Ok(())
}

#[test]
fn migration_older_database_is_upgraded_in_place() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let (mut journal, _) = JournalV1::open(harness.paths(), SchemaV1, RecoveryPolicy::default())?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Upsert(
            Key::new("alpha"),
            V1 { name: "Alpha".into() },
        )]))?;
    }
    let (mut journal, opened) = JournalV2::open(harness.paths(), SchemaV2, RecoveryPolicy::default())?;
    assert_eq!(opened.action, RecoveryOpenAction::Rebuilt);
    assert_eq!(journal.query(&Key::new("alpha"))?, Some(V2 { name: "Alpha".into(), enabled: true }));
    // The upgrade is durable: a second open no longer rebuilds.
    drop(journal);
    let (_journal, reopened) = JournalV2::open(harness.paths(), SchemaV2, RecoveryPolicy::default())?;
    assert_eq!(reopened.action, RecoveryOpenAction::Opened);
    Ok(())
}

// --- Health ----------------------------------------------------------------

#[test]
fn health_reports_same_filesystem_placement() -> io::Result<()> {
    let harness = Harness::new()?;
    let journal = harness.open_v3()?;
    let placement = journal.health().storage_placement;
    assert!(
        matches!(placement, RecoveryStoragePlacement::SameFilesystem | RecoveryStoragePlacement::Unknown),
        "unexpected placement {placement:?}"
    );
    Ok(())
}

#[test]
fn health_never_exposes_values_or_paths() -> io::Result<()> {
    let harness = Harness::new()?;
    let mut journal = harness.open_v3()?;
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "SecretName")]))?;
    let rendered = format!("{:?} {:?}", journal.health(), journal.verify()?);
    assert!(!rendered.contains("SecretName"));
    assert!(!rendered.contains("alpha"));
    assert!(!rendered.contains(&harness.paths.database.display().to_string()));
    Ok(())
}

#[test]
fn health_error_class_is_redacted() -> io::Result<()> {
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    fs::write(harness.newest_generation()?.join("manifest.bin"), b"broken")?;
    let Err(error) = JournalV3::open(harness.paths(), SchemaV3, RecoveryPolicy::default()) else {
        return Err(invalid("a broken manifest must not open cleanly"));
    };
    let rendered = error.to_string();
    assert!(!rendered.contains("Alpha"));
    // The class vocabulary itself carries no operational detail.
    let classes = [
        RecoveryErrorClass::Io,
        RecoveryErrorClass::Corruption,
        RecoveryErrorClass::SchemaMismatch,
        RecoveryErrorClass::MigrationFailed,
        RecoveryErrorClass::ForkedHistory,
        RecoveryErrorClass::DatabaseAhead,
        RecoveryErrorClass::UncertainWrite,
        RecoveryErrorClass::PublishFailed,
    ];
    assert_eq!(classes.len(), 8);
    Ok(())
}

#[test]
fn health_verify_reports_live_record_count() -> io::Result<()> {
    let harness = Harness::new()?;
    let mut journal = harness.open_v3()?;
    let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("a", "A"), upsert_v3("b", "B")]))?;
    let _ = journal.apply_batch(RecoveryBatch::new(vec![RecoveryOperation::Delete(Key::new("a"))]))?;
    let report = journal.verify()?;
    assert_eq!(report.live_records, 1);
    assert_eq!(report.database_revision, report.recovery_revision);
    Ok(())
}

#[test]
fn health_json_contract_is_stable() -> io::Result<()> {
    // The wrapper is generic over the application's records, so the only thing
    // this file may assert about a payload is the envelope shape.
    let expected = json!({
        "format_version": 1,
        "schema_name": "fixture",
        "schema_version": 3,
        "kind": "transaction",
    });
    let harness = Harness::new()?;
    {
        let mut journal = harness.open_v3()?;
        let _ = journal.apply_batch(RecoveryBatch::new(vec![upsert_v3("alpha", "Alpha")]))?;
    }
    let payloads = decode_journal_payloads(&read_bytes(&harness.journal_file()?)?)?;
    let record = payloads.first().ok_or_else(|| invalid("journal is empty"))?;
    for (field, value) in expected.as_object().ok_or_else(|| invalid("bad expectation"))? {
        assert_eq!(record.get(field), Some(value), "envelope field {field} changed");
    }
    Ok(())
}
