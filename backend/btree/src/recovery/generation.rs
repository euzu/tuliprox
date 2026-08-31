//! Recovery generations: immutable manifests, the `CURRENT` pointer, generation
//! selection after a crash, checkpoint publication and bounded pruning.
//!
//! A generation is one directory holding a checkpoint and the journal that
//! continues it. Publishing a new generation never edits an existing one, so a
//! crash in the middle of a checkpoint can always fall back to the previous
//! complete history.

use super::format::{
    chain, encode_frame, hex, invalid_data, payload_hash, read_frames, unhex, DecodedFrame, FrameStop, RecordBody,
    RecordEnvelope, FORMAT_VERSION, FRAME_MAGIC, MAX_RECORD_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const CURRENT_FILE: &str = "CURRENT";
const MANIFEST_FILE: &str = "manifest.bin";
const CHECKPOINT_FILE: &str = "checkpoint.bin";
const JOURNAL_FILE: &str = "journal.bin";
const GENERATION_PREFIX: &str = "gen-";
/// Current plus one previous verified generation.
pub(crate) const RETAINED_GENERATIONS: usize = 2;

/// The immutable description of one generation's checkpoint.
///
/// The journal head is deliberately absent: it is mutable, and is recovered by
/// scanning instead of by trusting a file that a crash could leave stale.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) format_version: u32,
    pub(crate) schema_name: String,
    pub(crate) schema_version: u32,
    pub(crate) database_id: String,
    pub(crate) generation: u64,
    pub(crate) checkpoint_revision: u64,
    pub(crate) checkpoint_records: u64,
    /// Chain hash after the final checkpoint frame; also the journal anchor.
    pub(crate) checkpoint_hash: String,
    pub(crate) created_at_unix: u64,
}

impl Manifest {
    pub(crate) fn checkpoint_hash_bytes(&self) -> io::Result<[u8; 32]> { unhex(&self.checkpoint_hash) }

    fn validate(&self, schema_name: &str) -> io::Result<()> {
        if self.format_version != FORMAT_VERSION {
            return Err(invalid_data("unsupported recovery manifest format version"));
        }
        if self.schema_name != schema_name {
            return Err(invalid_data("recovery manifest belongs to a different schema"));
        }
        if self.schema_version == 0 {
            return Err(invalid_data("recovery manifest schema version must be nonzero"));
        }
        let _ = self.checkpoint_hash_bytes()?;
        Ok(())
    }
}

/// Where one generation's files live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GenerationPaths {
    pub(crate) directory: PathBuf,
    pub(crate) manifest: PathBuf,
    pub(crate) checkpoint: PathBuf,
    pub(crate) journal: PathBuf,
}

impl GenerationPaths {
    pub(crate) fn new(root: &Path, generation: u64) -> Self {
        let directory = root.join(generation_directory_name(generation));
        Self {
            manifest: directory.join(MANIFEST_FILE),
            checkpoint: directory.join(CHECKPOINT_FILE),
            journal: directory.join(JOURNAL_FILE),
            directory,
        }
    }
}

pub(crate) fn generation_directory_name(generation: u64) -> String { format!("{GENERATION_PREFIX}{generation:020}") }

fn parse_generation_directory_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(GENERATION_PREFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The mutable tail of a generation, discovered by scanning its journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JournalScan {
    pub(crate) revision: u64,
    pub(crate) head_hash: [u8; 32],
    pub(crate) bytes: u64,
    pub(crate) transactions: u64,
    /// A crash left the last frame incomplete; the bytes before it are intact.
    pub(crate) torn_tail: bool,
}

/// One candidate history found under the recovery directory.
#[derive(Clone, Debug)]
pub(crate) struct GenerationScan {
    pub(crate) generation: u64,
    pub(crate) paths: GenerationPaths,
    pub(crate) manifest: Manifest,
    pub(crate) journal: JournalScan,
}

impl GenerationScan {
    pub(crate) fn order_key(&self) -> (u64, u64) { (self.manifest.checkpoint_revision, self.journal.revision) }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid_data("recovery file has no parent directory"))?;
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|name| name.to_str()).ok_or_else(|| invalid_data("recovery file has no name"))?
    ));
    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    sync_directory(parent)
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    match File::open(path) {
        Ok(handle) => match handle.sync_all() {
            Ok(()) => Ok(()),
            // Directory fsync is not supported everywhere; the rename is still ordered.
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(error),
    }
}

fn encode_manifest(manifest: &Manifest) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(manifest).map_err(|error| invalid_data(error.to_string()))?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(invalid_data("recovery manifest exceeds the record size limit"));
    }
    let hash = payload_hash(&payload);
    let length = u32::try_from(payload.len()).map_err(|_| invalid_data("manifest length exceeds u32"))?;
    let mut bytes = Vec::with_capacity(payload.len().saturating_add(40));
    bytes.extend_from_slice(&FRAME_MAGIC);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&hash);
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn decode_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    let magic = bytes.get(..4).ok_or_else(|| invalid_data("truncated recovery manifest"))?;
    if magic != FRAME_MAGIC {
        return Err(invalid_data("invalid recovery manifest magic"));
    }
    let length_bytes: [u8; 4] = bytes
        .get(4..8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| invalid_data("truncated recovery manifest length"))?;
    let length =
        usize::try_from(u32::from_le_bytes(length_bytes)).map_err(|_| invalid_data("manifest length exceeds usize"))?;
    if length == 0 || length > MAX_RECORD_BYTES {
        return Err(invalid_data("recovery manifest length is out of bounds"));
    }
    let claimed: [u8; 32] = bytes
        .get(8..40)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| invalid_data("truncated recovery manifest hash"))?;
    let end = 40usize.checked_add(length).ok_or_else(|| invalid_data("manifest offset overflow"))?;
    let payload = bytes.get(40..end).ok_or_else(|| invalid_data("truncated recovery manifest payload"))?;
    if payload_hash(payload) != claimed {
        return Err(invalid_data("recovery manifest hash mismatch"));
    }
    serde_json::from_slice(payload).map_err(|error| invalid_data(error.to_string()))
}

pub(crate) fn read_manifest(path: &Path, schema_name: &str) -> io::Result<Manifest> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_RECORD_BYTES.saturating_add(64)).unwrap_or(u64::MAX);
    let _ = File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    let manifest = decode_manifest(&bytes)?;
    manifest.validate(schema_name)?;
    Ok(manifest)
}

/// Scans one journal file from its anchor, verifying the hash chain.
pub(crate) fn scan_journal(
    path: &Path,
    schema_name: &str,
    database_id: &str,
    anchor_revision: u64,
    anchor_hash: [u8; 32],
) -> io::Result<JournalScan> {
    let mut scan =
        JournalScan { revision: anchor_revision, head_hash: anchor_hash, bytes: 0, transactions: 0, torn_tail: false };
    if !path.exists() {
        return Ok(scan);
    }
    let mut reader = BufReader::new(File::open(path)?);
    let stop = read_frames(&mut reader, |frame: DecodedFrame| {
        frame.envelope.validate_identity(schema_name, database_id)?;
        if !matches!(frame.envelope.body, RecordBody::Transaction { .. }) {
            return Err(invalid_data("recovery journal holds a non-transaction record"));
        }
        let expected_revision =
            scan.revision.checked_add(1).ok_or_else(|| invalid_data("recovery revision overflow"))?;
        if frame.envelope.revision != expected_revision {
            return Err(invalid_data(format!(
                "recovery journal revision {} does not follow {}",
                frame.envelope.revision, scan.revision
            )));
        }
        if frame.envelope.previous_hash_bytes()? != scan.head_hash {
            return Err(invalid_data("recovery journal hash chain is broken"));
        }
        scan.revision = expected_revision;
        scan.head_hash = chain(scan.head_hash, frame.payload_hash);
        scan.bytes = scan.bytes.checked_add(frame.frame_len).ok_or_else(|| invalid_data("journal size overflow"))?;
        scan.transactions =
            scan.transactions.checked_add(1).ok_or_else(|| invalid_data("journal transaction count overflow"))?;
        Ok(())
    })?;
    scan.torn_tail = matches!(stop, FrameStop::TornTail);
    Ok(scan)
}

/// Reads the `CURRENT` pointer, if it names a syntactically valid generation.
pub(crate) fn read_current(root: &Path) -> io::Result<Option<u64>> {
    let path = root.join(CURRENT_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse_generation_directory_name(text.trim())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn write_current(root: &Path, generation: u64) -> io::Result<()> {
    let mut text = generation_directory_name(generation);
    text.push('\n');
    write_atomic(&root.join(CURRENT_FILE), text.as_bytes())
}

/// Every generation directory that holds a decodable, chain-consistent history.
pub(crate) fn scan_generations(root: &Path, schema_name: &str, database_id: &str) -> io::Result<Vec<GenerationScan>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let Some(generation) = parse_generation_directory_name(&name) else { continue };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let paths = GenerationPaths::new(root, generation);
        let Ok(manifest) = read_manifest(&paths.manifest, schema_name) else { continue };
        if manifest.generation != generation || manifest.database_id != database_id {
            continue;
        }
        let anchor = manifest.checkpoint_hash_bytes()?;
        // A checkpoint that cannot be replayed end to end is not a candidate.
        if verify_checkpoint(&paths.checkpoint, schema_name, database_id, &manifest).is_err() {
            continue;
        }
        let Ok(journal) = scan_journal(&paths.journal, schema_name, database_id, manifest.checkpoint_revision, anchor)
        else {
            continue;
        };
        found.push(GenerationScan { generation, paths, manifest, journal });
    }
    found.sort_by_key(|scan| scan.generation);
    Ok(found)
}

/// Confirms that a checkpoint file matches the record count and hash its
/// manifest claims.
pub(crate) fn verify_checkpoint(
    path: &Path,
    schema_name: &str,
    database_id: &str,
    manifest: &Manifest,
) -> io::Result<u64> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hash = super::format::GENESIS_HASH;
    let mut seen_header = false;
    let mut records = 0u64;
    let stop = read_frames(&mut reader, |frame: DecodedFrame| {
        frame.envelope.validate_identity(schema_name, database_id)?;
        if frame.envelope.revision != manifest.checkpoint_revision {
            return Err(invalid_data("checkpoint record revision does not match the manifest"));
        }
        if frame.envelope.previous_hash_bytes()? != hash {
            return Err(invalid_data("checkpoint hash chain is broken"));
        }
        match &frame.envelope.body {
            RecordBody::CheckpointHeader { records: declared } => {
                if seen_header {
                    return Err(invalid_data("checkpoint holds more than one header"));
                }
                if *declared != manifest.checkpoint_records {
                    return Err(invalid_data("checkpoint header disagrees with the manifest"));
                }
                seen_header = true;
            }
            RecordBody::CheckpointEntry { .. } => {
                if !seen_header {
                    return Err(invalid_data("checkpoint entry appears before the header"));
                }
                records = records.checked_add(1).ok_or_else(|| invalid_data("checkpoint record overflow"))?;
            }
            RecordBody::Transaction { .. } => return Err(invalid_data("checkpoint holds a transaction record")),
        }
        hash = chain(hash, frame.payload_hash);
        Ok(())
    })?;
    if matches!(stop, FrameStop::TornTail) {
        return Err(invalid_data("checkpoint was not completely written"));
    }
    if !seen_header {
        return Err(invalid_data("checkpoint is missing its header"));
    }
    if records != manifest.checkpoint_records {
        return Err(invalid_data("checkpoint holds a different number of records than the manifest"));
    }
    if hash != manifest.checkpoint_hash_bytes()? {
        return Err(invalid_data("checkpoint hash does not match the manifest"));
    }
    Ok(records)
}

/// Streams the live entries of a checkpoint in file order.
pub(crate) fn read_checkpoint_entries<F>(
    path: &Path,
    schema_name: &str,
    database_id: &str,
    mut visit: F,
) -> io::Result<()>
where
    F: FnMut(serde_json::Value, serde_json::Value) -> io::Result<()>,
{
    let mut reader = BufReader::new(File::open(path)?);
    let _ = read_frames(&mut reader, |frame: DecodedFrame| {
        frame.envelope.validate_identity(schema_name, database_id)?;
        match frame.envelope.body {
            RecordBody::CheckpointEntry { key, value } => visit(key, value),
            RecordBody::CheckpointHeader { .. } => Ok(()),
            RecordBody::Transaction { .. } => Err(invalid_data("checkpoint holds a transaction record")),
        }
    })?;
    Ok(())
}

/// Builds one complete generation directory and returns its manifest.
///
/// The caller publishes it by writing `CURRENT` only after this succeeds, so a
/// crash here leaves the previous generation authoritative.
pub(crate) struct CheckpointWriter {
    paths: GenerationPaths,
    writer: BufWriter<File>,
    hash: [u8; 32],
    records: u64,
    schema_name: String,
    schema_version: u32,
    database_id: String,
    revision: u64,
}

impl CheckpointWriter {
    pub(crate) fn create(
        root: &Path,
        generation: u64,
        schema_name: &str,
        schema_version: u32,
        database_id: &str,
        revision: u64,
        records: u64,
    ) -> io::Result<Self> {
        let paths = GenerationPaths::new(root, generation);
        // A leftover directory from an interrupted checkpoint is never merged into.
        if paths.directory.exists() {
            fs::remove_dir_all(&paths.directory)?;
        }
        fs::create_dir_all(&paths.directory)?;
        let file = OpenOptions::new().create_new(true).write(true).open(&paths.checkpoint)?;
        let mut writer = Self {
            paths,
            writer: BufWriter::new(file),
            hash: super::format::GENESIS_HASH,
            records: 0,
            schema_name: schema_name.to_owned(),
            schema_version,
            database_id: database_id.to_owned(),
            revision,
        };
        writer.append(RecordBody::CheckpointHeader { records })?;
        Ok(writer)
    }

    fn append(&mut self, body: RecordBody) -> io::Result<()> {
        let envelope = RecordEnvelope::new(
            &self.schema_name,
            self.schema_version,
            &self.database_id,
            self.revision,
            self.hash,
            body,
        );
        let (frame, hash) = encode_frame(&envelope)?;
        self.writer.write_all(&frame)?;
        self.hash = chain(self.hash, hash);
        Ok(())
    }

    pub(crate) fn push_entry(&mut self, key: serde_json::Value, value: serde_json::Value) -> io::Result<()> {
        self.append(RecordBody::CheckpointEntry { key, value })?;
        self.records = self.records.checked_add(1).ok_or_else(|| invalid_data("checkpoint record overflow"))?;
        Ok(())
    }

    /// Syncs the checkpoint, re-reads it, then writes the manifest and an empty
    /// anchored journal.
    pub(crate) fn finish(mut self, generation: u64) -> io::Result<(GenerationPaths, Manifest)> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            schema_name: self.schema_name.clone(),
            schema_version: self.schema_version,
            database_id: self.database_id.clone(),
            generation,
            checkpoint_revision: self.revision,
            checkpoint_records: self.records,
            checkpoint_hash: hex(&self.hash),
            created_at_unix: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs(),
        };
        let verified = verify_checkpoint(&self.paths.checkpoint, &self.schema_name, &self.database_id, &manifest)?;
        if verified != manifest.checkpoint_records {
            return Err(invalid_data("checkpoint verification returned an unexpected record count"));
        }
        {
            let journal = OpenOptions::new().create(true).truncate(true).write(true).open(&self.paths.journal)?;
            journal.sync_all()?;
        }
        write_atomic(&self.paths.manifest, &encode_manifest(&manifest)?)?;
        sync_directory(&self.paths.directory)?;
        Ok((self.paths, manifest))
    }
}

/// Deletes every generation outside the newest `RETAINED_GENERATIONS`.
pub(crate) fn prune_generations(root: &Path, keep: &[u64]) -> io::Result<u64> {
    let mut pruned = 0u64;
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let Some(generation) = parse_generation_directory_name(&name) else { continue };
        if keep.contains(&generation) {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
        pruned = pruned.checked_add(1).ok_or_else(|| invalid_data("pruned generation count overflow"))?;
    }
    if pruned > 0 {
        sync_directory(root)?;
    }
    Ok(pruned)
}
