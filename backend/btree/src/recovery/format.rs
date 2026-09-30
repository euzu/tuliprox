//! Bounded, self-describing recovery records.
//!
//! Every record is a fixed binary frame that carries its own length and content
//! hash, followed by a field-named JSON payload. Nothing here depends on B+Tree
//! page layout or on `MessagePack` field order, which is what lets a record
//! written by an older build still be understood after the value type changes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read};

/// Identifies a recovery frame and pins the framing itself to a version.
pub(crate) const FRAME_MAGIC: [u8; 4] = *b"TRJ1";
/// magic + payload length + payload hash.
pub(crate) const FRAME_HEADER_LEN: usize = 4 + 4 + 32;
/// The only payload envelope version this build writes or accepts.
pub(crate) const FORMAT_VERSION: u32 = 1;
/// Refuses a corrupt length before it becomes an allocation.
pub(crate) const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// Bounds one logical transaction so a single batch cannot exceed the frame.
pub(crate) const MAX_OPERATIONS: usize = 65_536;

pub(crate) const GENESIS_HASH: [u8; 32] = [0; 32];

pub(crate) fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// One key/value mutation, encoded by name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum JsonOperation {
    Upsert { key: Value, value: Value },
    Delete { key: Value },
}

/// The payload variants a recovery file can hold.
///
/// A checkpoint is a header frame followed by one frame per live entry, so
/// restoring never has to hold the whole checkpoint in memory at once.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RecordBody {
    Transaction { operations: Vec<JsonOperation> },
    CheckpointHeader { records: u64 },
    CheckpointEntry { key: Value, value: Value },
}

/// The common, self-describing part of every recovery record.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RecordEnvelope {
    pub(crate) format_version: u32,
    pub(crate) schema_name: String,
    pub(crate) schema_version: u32,
    pub(crate) database_id: String,
    pub(crate) revision: u64,
    pub(crate) previous_hash: String,
    #[serde(flatten)]
    pub(crate) body: RecordBody,
}

impl RecordEnvelope {
    pub(crate) fn new(
        schema_name: &str,
        schema_version: u32,
        database_id: &str,
        revision: u64,
        previous_hash: [u8; 32],
        body: RecordBody,
    ) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            schema_name: schema_name.to_owned(),
            schema_version,
            database_id: database_id.to_owned(),
            revision,
            previous_hash: hex(&previous_hash),
            body,
        }
    }

    fn validate_self(&self) -> io::Result<()> {
        if self.format_version != FORMAT_VERSION {
            return Err(invalid_data(format!(
                "unsupported recovery format version {} (supported: {FORMAT_VERSION})",
                self.format_version
            )));
        }
        if self.schema_version == 0 {
            return Err(invalid_data("recovery schema version must be nonzero"));
        }
        if self.revision == 0 {
            return Err(invalid_data("recovery revision must be nonzero"));
        }
        if let RecordBody::Transaction { operations } = &self.body {
            if operations.is_empty() {
                return Err(invalid_data("recovery transaction must contain at least one operation"));
            }
            if operations.len() > MAX_OPERATIONS {
                return Err(invalid_data(format!(
                    "recovery transaction holds {} operations, limit is {MAX_OPERATIONS}",
                    operations.len()
                )));
            }
        }
        Ok(())
    }

    /// Rejects a record that belongs to a different schema or database, before
    /// any application decoding is attempted.
    pub(crate) fn validate_identity(&self, schema_name: &str, database_id: &str) -> io::Result<()> {
        self.validate_self()?;
        if self.schema_name != schema_name {
            return Err(invalid_data(format!(
                "recovery record belongs to schema {} but {schema_name} was expected",
                self.schema_name
            )));
        }
        if self.database_id != database_id {
            return Err(invalid_data("recovery record belongs to a different database"));
        }
        Ok(())
    }

    pub(crate) fn previous_hash_bytes(&self) -> io::Result<[u8; 32]> { unhex(&self.previous_hash) }
}

pub(crate) fn hex(bytes: &[u8; 32]) -> String { hex_bytes(bytes) }

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

pub(crate) fn unhex(text: &str) -> io::Result<[u8; 32]> {
    if text.len() != 64 {
        return Err(invalid_data("recovery hash must be 64 hex characters"));
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let start = index.checked_mul(2).ok_or_else(|| invalid_data("recovery hash offset overflow"))?;
        let end = start.checked_add(2).ok_or_else(|| invalid_data("recovery hash offset overflow"))?;
        let pair = bytes.get(start..end).ok_or_else(|| invalid_data("truncated recovery hash"))?;
        let pair = std::str::from_utf8(pair).map_err(|_| invalid_data("recovery hash is not ascii"))?;
        *slot = u8::from_str_radix(pair, 16).map_err(|_| invalid_data("recovery hash is not hexadecimal"))?;
    }
    Ok(out)
}

/// Links one record to its predecessor so a rewritten middle record is detected
/// even when its own frame hash is consistent.
pub(crate) fn chain(previous: [u8; 32], payload_hash: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous);
    hasher.update(&payload_hash);
    *hasher.finalize().as_bytes()
}

pub(crate) fn payload_hash(payload: &[u8]) -> [u8; 32] { *blake3::hash(payload).as_bytes() }

/// Encodes one envelope as a complete frame.
pub(crate) fn encode_frame(envelope: &RecordEnvelope) -> io::Result<(Vec<u8>, [u8; 32])> {
    envelope.validate_self()?;
    let payload = serde_json::to_vec(envelope).map_err(|error| invalid_data(error.to_string()))?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(invalid_data(format!(
            "recovery record of {} bytes exceeds the {MAX_RECORD_BYTES} byte limit",
            payload.len()
        )));
    }
    let hash = payload_hash(&payload);
    let length = u32::try_from(payload.len()).map_err(|_| invalid_data("recovery record length exceeds u32"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN.saturating_add(payload.len()));
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&hash);
    frame.extend_from_slice(&payload);
    Ok((frame, hash))
}

/// A frame that was read from disk, with the hash the frame itself claims.
pub(crate) struct DecodedFrame {
    pub(crate) envelope: RecordEnvelope,
    pub(crate) payload_hash: [u8; 32],
    pub(crate) frame_len: u64,
}

/// Why frame reading stopped.
pub(crate) enum FrameStop {
    /// The reader reached a clean end of file.
    Eof,
    /// A final frame was cut short by a crash. Everything before it is intact.
    TornTail,
}

/// Reads frames until the stream ends, a torn tail is found, or `visit` fails.
///
/// A frame is only tolerated as a torn tail when it is genuinely incomplete. A
/// complete frame whose hash disagrees is corruption and always fails.
pub(crate) fn read_frames<R, F>(reader: &mut R, mut visit: F) -> io::Result<FrameStop>
where
    R: Read,
    F: FnMut(DecodedFrame) -> io::Result<()>,
{
    let mut header = [0u8; FRAME_HEADER_LEN];
    loop {
        match read_exact_or_eof(reader, &mut header)? {
            ReadOutcome::Eof => return Ok(FrameStop::Eof),
            ReadOutcome::Partial => return Ok(FrameStop::TornTail),
            ReadOutcome::Full => {}
        }
        let magic: [u8; 4] =
            header.get(..4).and_then(|slice| slice.try_into().ok()).ok_or_else(|| invalid_data("short frame magic"))?;
        if magic != FRAME_MAGIC {
            return Err(invalid_data("invalid recovery frame magic"));
        }
        let length_bytes: [u8; 4] = header
            .get(4..8)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid_data("short frame length"))?;
        let length = usize::try_from(u32::from_le_bytes(length_bytes))
            .map_err(|_| invalid_data("recovery frame length exceeds usize"))?;
        if length == 0 || length > MAX_RECORD_BYTES {
            return Err(invalid_data(format!("recovery frame length {length} is out of bounds")));
        }
        let claimed: [u8; 32] = header
            .get(8..FRAME_HEADER_LEN)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| invalid_data("short frame hash"))?;

        let mut payload = Vec::new();
        payload.try_reserve_exact(length).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        payload.resize(length, 0);
        match read_exact_or_eof(reader, &mut payload)? {
            ReadOutcome::Eof | ReadOutcome::Partial => return Ok(FrameStop::TornTail),
            ReadOutcome::Full => {}
        }
        if payload_hash(&payload) != claimed {
            return Err(invalid_data("recovery record hash mismatch"));
        }
        let envelope: RecordEnvelope =
            serde_json::from_slice(&payload).map_err(|error| invalid_data(error.to_string()))?;
        envelope.validate_self()?;
        let frame_len = u64::try_from(FRAME_HEADER_LEN.saturating_add(length))
            .map_err(|_| invalid_data("recovery frame length exceeds u64"))?;
        visit(DecodedFrame { envelope, payload_hash: claimed, frame_len })?;
    }
}

enum ReadOutcome {
    Full,
    Partial,
    Eof,
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<ReadOutcome> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        let slice = buffer.get_mut(filled..).ok_or_else(|| invalid_data("frame buffer overflow"))?;
        match reader.read(slice) {
            Ok(0) => {
                return Ok(if filled == 0 { ReadOutcome::Eof } else { ReadOutcome::Partial });
            }
            Ok(count) => {
                filled = filled.checked_add(count).ok_or_else(|| invalid_data("frame read overflow"))?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ReadOutcome::Full)
}
