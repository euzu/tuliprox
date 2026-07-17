use super::{
    format::{DatabaseHeader, PAGE_SIZE},
    page::SlottedPage,
};
use crate::repository::bplustree::common::sidecar_lock_path;
use fs2::FileExt as _;
use log::info;
use std::{
    collections::HashSet,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const WAL_HEADER_LEN: usize = 64;
const RECORD_HEADER_LEN: usize = 16;
const BEFORE_IMAGE_PAYLOAD_LEN: usize = 16 + PAGE_SIZE;
const COMMIT_PAYLOAD_LEN: usize = 32;
const WAL_HEADER_LEN_U32: u32 = 64;
const WAL_HEADER_LEN_U64: u64 = 64;
const RECORD_HEADER_LEN_U64: u64 = 16;
const PAGE_SIZE_U32: u32 = 4096;
const PAGE_SIZE_U64: u64 = 4096;
const WAL_MAGIC: &[u8; 4] = b"BTW3";
const WAL_VERSION: u32 = 1;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn out_of_memory(message: &'static str) -> impl FnOnce(std::collections::TryReserveError) -> io::Error {
    move |error| io::Error::new(io::ErrorKind::OutOfMemory, format!("{message}: {error}"))
}

fn checked_end(offset: usize, length: usize) -> io::Result<usize> {
    offset.checked_add(length).ok_or_else(|| invalid_data("WAL offset overflow"))
}

fn bytes_at<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    let end = checked_end(offset, N)?;
    bytes
        .get(offset..end)
        .ok_or_else(|| invalid_data("truncated WAL field"))?
        .try_into()
        .map_err(|_| invalid_data("invalid WAL field length"))
}

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> { Ok(u32::from_le_bytes(bytes_at(bytes, offset)?)) }

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> { Ok(u64::from_le_bytes(bytes_at(bytes, offset)?)) }

fn require_zero(bytes: &[u8], message: &'static str) -> io::Result<()> {
    if bytes.iter().all(|byte| *byte == 0) { Ok(()) } else { Err(invalid_data(message)) }
}

fn crc_with_zeroed_field(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let end = checked_end(offset, 4)?;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes.get(..offset).ok_or_else(|| invalid_data("missing checksum prefix"))?);
    hasher.update(&[0; 4]);
    hasher.update(bytes.get(end..).ok_or_else(|| invalid_data("missing checksum suffix"))?);
    Ok(hasher.finalize())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalHeader {
    database_id: [u8; 16],
    transaction_id: u64,
    original_database_len: u64,
    original_generation: u64,
}

impl WalHeader {
    fn encode(&self) -> io::Result<[u8; WAL_HEADER_LEN]> {
        self.validate()?;
        let mut encoded = [0u8; WAL_HEADER_LEN];
        encoded[0..4].copy_from_slice(WAL_MAGIC);
        encoded[4..8].copy_from_slice(&WAL_VERSION.to_le_bytes());
        encoded[8..12].copy_from_slice(&WAL_HEADER_LEN_U32.to_le_bytes());
        encoded[12..16].copy_from_slice(&PAGE_SIZE_U32.to_le_bytes());
        encoded[16..32].copy_from_slice(&self.database_id);
        encoded[32..40].copy_from_slice(&self.transaction_id.to_le_bytes());
        encoded[40..48].copy_from_slice(&self.original_database_len.to_le_bytes());
        encoded[48..56].copy_from_slice(&self.original_generation.to_le_bytes());
        let checksum = crc_with_zeroed_field(&encoded, 56)?;
        encoded[56..60].copy_from_slice(&checksum.to_le_bytes());
        Ok(encoded)
    }

    fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != WAL_HEADER_LEN {
            return Err(invalid_data("WAL header must be exactly 64 bytes"));
        }
        if bytes_at::<4>(encoded, 0)? != *WAL_MAGIC {
            return Err(invalid_data("invalid WAL magic"));
        }
        if read_u32(encoded, 4)? != WAL_VERSION {
            return Err(invalid_data("unsupported WAL version"));
        }
        if read_u32(encoded, 8)? != WAL_HEADER_LEN_U32 {
            return Err(invalid_data("invalid WAL header length"));
        }
        if read_u32(encoded, 12)? != PAGE_SIZE_U32 {
            return Err(invalid_data("invalid WAL page size"));
        }
        require_zero(&encoded[60..64], "WAL header reserved bytes must be zero")?;
        if read_u32(encoded, 56)? != crc_with_zeroed_field(encoded, 56)? {
            return Err(invalid_data("WAL header checksum mismatch"));
        }
        let header = Self {
            database_id: bytes_at(encoded, 16)?,
            transaction_id: read_u64(encoded, 32)?,
            original_database_len: read_u64(encoded, 40)?,
            original_generation: read_u64(encoded, 48)?,
        };
        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> io::Result<()> {
        if self.database_id.iter().all(|byte| *byte == 0) {
            return Err(invalid_data("WAL database identity must be nonzero"));
        }
        if self.transaction_id == 0 {
            return Err(invalid_data("WAL transaction id must be nonzero"));
        }
        if self.original_database_len < PAGE_SIZE_U64
            || !self.original_database_len.is_multiple_of(PAGE_SIZE_U64)
        {
            return Err(invalid_data("invalid original database length"));
        }
        if self.original_generation == 0 {
            return Err(invalid_data("WAL original generation must be nonzero"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BeforeImage {
    page_id: u64,
    page: [u8; PAGE_SIZE],
}

impl BeforeImage {
    #[cfg(test)]
    fn encode_record(&self) -> io::Result<Vec<u8>> {
        let mut encoded = Vec::new();
        self.encode_record_into(&mut encoded)?;
        Ok(encoded)
    }

    fn encode_record_into(&self, encoded: &mut Vec<u8>) -> io::Result<()> {
        prepare_record(encoded, 1, BEFORE_IMAGE_PAYLOAD_LEN)?;
        encoded[16..24].copy_from_slice(&self.page_id.to_le_bytes());
        encoded[24..28].copy_from_slice(&crc32fast::hash(&self.page).to_le_bytes());
        encoded[32..].copy_from_slice(&self.page);
        finish_record(encoded)
    }

    fn decode_payload(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != BEFORE_IMAGE_PAYLOAD_LEN {
            return Err(invalid_data("invalid before-image payload length"));
        }
        require_zero(&payload[12..16], "before-image reserved bytes must be zero")?;
        let page: [u8; PAGE_SIZE] = payload[16..]
            .try_into()
            .map_err(|_| invalid_data("truncated before-image page"))?;
        if read_u32(payload, 8)? != crc32fast::hash(&page) {
            return Err(invalid_data("before-image page checksum mismatch"));
        }
        Ok(Self { page_id: read_u64(payload, 0)?, page })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommitRecord {
    transaction_id: u64,
    new_generation: u64,
    database_length: u64,
    database_header_crc32: u32,
}

impl CommitRecord {
    #[cfg(test)]
    fn encode_record(self) -> io::Result<Vec<u8>> {
        let mut encoded = Vec::new();
        self.encode_record_into(&mut encoded)?;
        Ok(encoded)
    }

    fn encode_record_into(self, encoded: &mut Vec<u8>) -> io::Result<()> {
        prepare_record(encoded, 2, COMMIT_PAYLOAD_LEN)?;
        encoded[16..24].copy_from_slice(&self.transaction_id.to_le_bytes());
        encoded[24..32].copy_from_slice(&self.new_generation.to_le_bytes());
        encoded[32..40].copy_from_slice(&self.database_length.to_le_bytes());
        encoded[40..44].copy_from_slice(&self.database_header_crc32.to_le_bytes());
        finish_record(encoded)
    }

    fn decode_payload(payload: &[u8]) -> io::Result<Self> {
        if payload.len() != COMMIT_PAYLOAD_LEN {
            return Err(invalid_data("invalid commit payload length"));
        }
        require_zero(&payload[28..32], "commit reserved bytes must be zero")?;
        let commit = Self {
            transaction_id: read_u64(payload, 0)?,
            new_generation: read_u64(payload, 8)?,
            database_length: read_u64(payload, 16)?,
            database_header_crc32: read_u32(payload, 24)?,
        };
        if commit.transaction_id == 0 || commit.new_generation == 0 {
            return Err(invalid_data("invalid commit identity or generation"));
        }
        if commit.database_length < PAGE_SIZE_U64 || !commit.database_length.is_multiple_of(PAGE_SIZE_U64) {
            return Err(invalid_data("invalid committed database length"));
        }
        Ok(commit)
    }
}

fn prepare_record(encoded: &mut Vec<u8>, kind: u8, payload_length: usize) -> io::Result<()> {
    let payload_length_u32 = u32::try_from(payload_length).map_err(|_| invalid_input("WAL payload exceeds u32"))?;
    let record_length = RECORD_HEADER_LEN
        .checked_add(payload_length)
        .ok_or_else(|| invalid_input("WAL record length overflow"))?;
    encoded.clear();
    encoded.try_reserve_exact(record_length).map_err(out_of_memory("WAL record allocation failed"))?;
    encoded.resize(record_length, 0);
    encoded[0] = kind;
    encoded[4..8].copy_from_slice(&payload_length_u32.to_le_bytes());
    Ok(())
}

fn finish_record(encoded: &mut [u8]) -> io::Result<()> {
    let checksum = crc_with_zeroed_field(encoded, 8)?;
    encoded[8..12].copy_from_slice(&checksum.to_le_bytes());
    Ok(())
}

#[derive(Debug)]
struct ParsedWal {
    header: WalHeader,
    before_images: Vec<BeforeImage>,
    commit: Option<CommitRecord>,
    torn_tail: bool,
}

fn read_wal(path: &Path) -> io::Result<ParsedWal> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < WAL_HEADER_LEN_U64 {
        return Err(invalid_data("active WAL has a truncated header"));
    }
    let mut header_bytes = [0u8; WAL_HEADER_LEN];
    file.read_exact(&mut header_bytes)?;
    let header = WalHeader::decode(&header_bytes)?;
    let original_page_count = header.original_database_len / PAGE_SIZE_U64;
    let mut before_images = Vec::new();
    let mut page_ids = HashSet::new();
    let mut commit = None;
    let mut offset = WAL_HEADER_LEN_U64;
    let mut torn_tail = false;

    while offset < file_len {
        if commit.is_some() {
            return Err(invalid_data("WAL record appears after commit"));
        }
        let remaining = file_len - offset;
        if remaining < RECORD_HEADER_LEN_U64 {
            torn_tail = true;
            break;
        }
        let mut record_header = [0u8; RECORD_HEADER_LEN];
        file.read_exact(&mut record_header)?;
        let kind = record_header[0];
        if record_header[1] != 0 {
            return Err(invalid_data("unknown WAL record flags"));
        }
        require_zero(&record_header[2..4], "WAL record reserved bytes must be zero")?;
        require_zero(&record_header[12..16], "WAL record reserved bytes must be zero")?;
        let payload_length = usize::try_from(read_u32(&record_header, 4)?)
            .map_err(|_| invalid_data("WAL payload length exceeds usize"))?;
        let expected_length = match kind {
            1 => BEFORE_IMAGE_PAYLOAD_LEN,
            2 => COMMIT_PAYLOAD_LEN,
            _ => return Err(invalid_data("unknown WAL record kind")),
        };
        if payload_length != expected_length {
            return Err(invalid_data("WAL record payload length mismatch"));
        }
        let total_length = RECORD_HEADER_LEN
            .checked_add(payload_length)
            .ok_or_else(|| invalid_data("WAL record length overflow"))?;
        let total_length_u64 = u64::try_from(total_length).map_err(|_| invalid_data("WAL record exceeds u64"))?;
        if remaining < total_length_u64 {
            torn_tail = true;
            break;
        }
        let mut payload = vec![0u8; payload_length];
        file.read_exact(&mut payload)?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&record_header[..8]);
        hasher.update(&[0; 4]);
        hasher.update(&record_header[12..]);
        hasher.update(&payload);
        if read_u32(&record_header, 8)? != hasher.finalize() {
            return Err(invalid_data("WAL record checksum mismatch"));
        }

        match kind {
            1 => {
                let image = BeforeImage::decode_payload(&payload)?;
                if image.page_id >= original_page_count {
                    return Err(invalid_data("WAL before-image page id exceeds original database"));
                }
                page_ids.try_reserve(1).map_err(out_of_memory("WAL page-id set allocation failed"))?;
                if !page_ids.insert(image.page_id) {
                    return Err(invalid_data("duplicate WAL before-image page id"));
                }
                before_images.try_reserve(1).map_err(out_of_memory("WAL before-image allocation failed"))?;
                before_images.push(image);
            }
            2 => {
                let record = CommitRecord::decode_payload(&payload)?;
                if record.transaction_id != header.transaction_id {
                    return Err(invalid_data("commit transaction id does not match WAL header"));
                }
                if header.original_generation.checked_add(1) != Some(record.new_generation) {
                    return Err(invalid_data("commit generation must advance WAL generation exactly once"));
                }
                commit = Some(record);
            }
            _ => return Err(invalid_data("unknown WAL record kind")),
        }
        offset = offset
            .checked_add(total_length_u64)
            .ok_or_else(|| invalid_data("WAL record offset overflow"))?;
    }
    Ok(ParsedWal { header, before_images, commit, torn_tail })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommitBoundary {
    WalTempSynced,
    WalActivated,
    BeforeImagesSynced,
    DatabaseWritten,
    DatabaseSynced,
    CommitAppended,
    CommitSynced,
    WalCleared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryBoundary {
    PageRestored(u64),
    HeaderRestored,
    DatabaseTruncated,
    DatabaseSynced,
    WalRemoved,
    ParentDirectorySynced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalOutcome {
    RecoveryPending,
    CommittedCleanupPending,
}

#[derive(Debug)]
pub(crate) struct WalOperationError {
    outcome: WalOutcome,
    database: PathBuf,
    wal: PathBuf,
    transaction_id: u64,
    phase: &'static str,
    cause: io::Error,
}

impl WalOperationError {
    pub(crate) fn outcome(&self) -> WalOutcome { self.outcome }

    #[cfg(test)]
    pub(crate) fn database_path(&self) -> &Path { &self.database }

    #[cfg(test)]
    pub(crate) fn wal_path(&self) -> &Path { &self.wal }

    #[cfg(test)]
    pub(crate) fn transaction_id(&self) -> u64 { self.transaction_id }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> &'static str { self.phase }
}

impl fmt::Display for WalOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "WAL operation for {} failed during {} with {:?}: WAL {} transaction {}: {}",
            self.database.display(),
            self.phase,
            self.outcome,
            self.wal.display(),
            self.transaction_id,
            self.cause
        )
    }
}

impl Error for WalOperationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> { Some(&self.cause) }
}

fn wal_operation_error<'a>(
    outcome: WalOutcome,
    database: &'a Path,
    wal: &'a Path,
    transaction_id: u64,
    phase: &'static str,
) -> impl FnOnce(io::Error) -> io::Error + 'a {
    move |cause| {
        let kind = cause.kind();
        io::Error::new(
            kind,
            WalOperationError {
                outcome,
                database: database.to_path_buf(),
                wal: wal.to_path_buf(),
                transaction_id,
                phase,
                cause,
            },
        )
    }
}

#[derive(Clone, Copy)]
struct WalErrorContext<'a> {
    outcome: WalOutcome,
    database: &'a Path,
    wal: &'a Path,
    transaction_id: u64,
}

impl WalErrorContext<'_> {
    fn wrap<T>(&self, phase: &'static str, result: io::Result<T>) -> io::Result<T> {
        result.map_err(wal_operation_error(
            self.outcome,
            self.database,
            self.wal,
            self.transaction_id,
            phase,
        ))
    }
}

#[derive(Debug)]
pub(crate) struct WalReadError {
    database: PathBuf,
    wal: PathBuf,
    wal_database_id: Option<[u8; 16]>,
    current_database_id: Option<[u8; 16]>,
    cause: io::Error,
}

impl WalReadError {
    #[cfg(test)]
    pub(crate) fn database_path(&self) -> &Path { &self.database }

    #[cfg(test)]
    pub(crate) fn wal_path(&self) -> &Path { &self.wal }

    #[cfg(test)]
    pub(crate) fn wal_database_id(&self) -> Option<[u8; 16]> { self.wal_database_id }

    #[cfg(test)]
    pub(crate) fn current_database_id(&self) -> Option<[u8; 16]> { self.current_database_id }
}

impl fmt::Display for WalReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to parse WAL {} for database {}",
            self.wal.display(),
            self.database.display()
        )?;
        if let Some(identity) = self.wal_database_id {
            write!(formatter, ", WAL database identity {identity:02x?}")?;
        } else {
            write!(formatter, ", WAL database identity unavailable")?;
        }
        if let Some(identity) = self.current_database_id {
            write!(formatter, ", current database identity {identity:02x?}")?;
        } else {
            write!(formatter, ", current database identity unavailable")?;
        }
        write!(formatter, ": {}", self.cause)
    }
}

impl Error for WalReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> { Some(&self.cause) }
}

fn raw_database_id(path: &Path, offset: u64) -> Option<[u8; 16]> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut identity = [0u8; 16];
    file.read_exact(&mut identity).ok()?;
    Some(identity)
}

fn wal_read_error(database: &Path, wal: &Path, cause: io::Error) -> io::Error {
    let kind = cause.kind();
    io::Error::new(
        kind,
        WalReadError {
            database: database.to_path_buf(),
            wal: wal.to_path_buf(),
            wal_database_id: raw_database_id(wal, 16),
            current_database_id: raw_database_id(database, 48),
            cause,
        },
    )
}

fn page_offset(page_id: u64) -> io::Result<u64> {
    page_id.checked_mul(PAGE_SIZE_U64).ok_or_else(|| invalid_data("database page offset overflow"))
}

fn read_page(file: &mut File, page_id: u64) -> io::Result<[u8; PAGE_SIZE]> {
    let mut page = [0u8; PAGE_SIZE];
    file.seek(SeekFrom::Start(page_offset(page_id)?))?;
    file.read_exact(&mut page)?;
    Ok(page)
}

fn write_page(file: &mut File, page_id: u64, page: &[u8; PAGE_SIZE]) -> io::Result<()> {
    file.seek(SeekFrom::Start(page_offset(page_id)?))?;
    file.write_all(page)
}

fn database_length(next_page_id: u64) -> io::Result<u64> {
    next_page_id
        .checked_mul(PAGE_SIZE_U64)
        .ok_or_else(|| invalid_input("database length overflow"))
}

fn database_header_crc32(page: &[u8; PAGE_SIZE]) -> io::Result<u32> { read_u32(page, 72) }

fn new_transaction_id() -> io::Result<u64> {
    let bytes = uuid::Uuid::new_v4().as_u128().to_le_bytes();
    let low: [u8; 8] = bytes[..8]
        .try_into()
        .map_err(|_| io::Error::other("UUID transaction id conversion failed"))?;
    let transaction_id = u64::from_le_bytes(low);
    Ok(if transaction_id == 0 { 1 } else { transaction_id })
}

#[cfg(unix)]
pub(super) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn invalidate_sorted_index(database: &Path) -> io::Result<()> {
    let index = crate::repository::storage::get_file_path_for_db_index(database);
    if index == database {
        return Ok(());
    }
    match std::fs::remove_file(&index) {
        Ok(()) => sync_parent_directory(&index),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn clear_wal(database: &Path, transaction_id: u64, outcome: WalOutcome) -> io::Result<()> {
    clear_wal_with_hook(database, transaction_id, outcome, &mut |_| Ok(()))
}

fn clear_wal_with_hook<H>(
    database: &Path,
    transaction_id: u64,
    outcome: WalOutcome,
    hook: &mut H,
) -> io::Result<()>
where
    H: FnMut(RecoveryBoundary) -> io::Result<()>,
{
    let active = wal_path(database);
    if active.try_exists().map_err(wal_operation_error(
        outcome,
        database,
        &active,
        transaction_id,
        "inspect active WAL before cleanup",
    ))? {
        std::fs::remove_file(&active).map_err(wal_operation_error(
            outcome,
            database,
            &active,
            transaction_id,
            "remove active WAL",
        ))?;
        hook(RecoveryBoundary::WalRemoved).map_err(wal_operation_error(
            outcome,
            database,
            &active,
            transaction_id,
            "WalRemoved hook",
        ))?;
    }
    let temporary = wal_temporary_path(database);
    if temporary.try_exists().map_err(wal_operation_error(
        outcome,
        database,
        &active,
        transaction_id,
        "inspect temporary WAL before cleanup",
    ))? {
        std::fs::remove_file(temporary).map_err(wal_operation_error(
            outcome,
            database,
            &active,
            transaction_id,
            "remove temporary WAL",
        ))?;
    }
    sync_parent_directory(database).map_err(wal_operation_error(
        outcome,
        database,
        &active,
        transaction_id,
        "sync parent directory after WAL removal",
    ))?;
    hook(RecoveryBoundary::ParentDirectorySynced).map_err(wal_operation_error(
        outcome,
        database,
        &active,
        transaction_id,
        "ParentDirectorySynced hook",
    ))
}

#[cfg(test)]
fn ordered_prepared_pages(
    prepared: &[(u64, [u8; PAGE_SIZE])],
) -> io::Result<Vec<(u64, &[u8; PAGE_SIZE])>> {
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(prepared.len()).map_err(out_of_memory("prepared-page ordering allocation failed"))?;
    ordered.extend(prepared.iter().map(|(page_id, page)| (*page_id, page)));
    ordered.sort_unstable_by_key(|(page_id, _)| *page_id);
    if ordered.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid_input("duplicate prepared page id"));
    }
    Ok(ordered)
}

fn validate_prepared_pages(
    database: &mut File,
    ordered: &[(u64, &[u8; PAGE_SIZE])],
) -> io::Result<(DatabaseHeader, DatabaseHeader, u64)> {
    let original_length = database.metadata()?.len();
    if original_length < PAGE_SIZE_U64 || !original_length.is_multiple_of(PAGE_SIZE_U64) {
        return Err(invalid_data("database length is not page aligned"));
    }
    let original_page = read_page(database, 0)?;
    let original = DatabaseHeader::decode(&original_page)?;
    if database_length(original.next_page_id)? != original_length {
        return Err(invalid_data("database length does not match original header"));
    }
    let (_, final_header_page) = ordered
        .first()
        .copied()
        .filter(|(page_id, _)| *page_id == 0)
        .ok_or_else(|| invalid_input("prepared transaction must include database header page 0"))?;
    let final_header = DatabaseHeader::decode(final_header_page)?;
    if final_header.database_id != original.database_id {
        return Err(invalid_input("prepared database identity differs from current database"));
    }
    if original.generation.checked_add(1) != Some(final_header.generation) {
        return Err(invalid_input("prepared generation must advance exactly once"));
    }
    let final_length = database_length(final_header.next_page_id)?;
    if final_length < original_length {
        return Err(invalid_input("prepared transaction cannot shrink the database"));
    }
    for (page_id, page) in ordered {
        if *page_id >= final_header.next_page_id {
            return Err(invalid_input("prepared page id exceeds final database bounds"));
        }
        if *page_id == 0 {
            DatabaseHeader::decode(*page)?;
        } else {
            SlottedPage::open(page.as_slice(), *page_id, final_header.next_page_id)?;
        }
    }
    for page_id in original.next_page_id..final_header.next_page_id {
        if ordered.binary_search_by_key(&page_id, |(candidate, _)| *candidate).is_err() {
            return Err(invalid_input("prepared transaction omits an appended page"));
        }
    }
    Ok((original, final_header, final_length))
}

#[cfg(test)]
pub(crate) fn commit_prepared_pages(
    database: &Path,
    prepared: &[(u64, [u8; PAGE_SIZE])],
) -> io::Result<()> {
    with_exclusive_sidecar(database, || {
        recover_pending_under_existing_lock(database)?;
        commit_prepared_pages_under_existing_lock(database, prepared)
    })
}

#[cfg(test)]
pub(crate) fn commit_prepared_pages_under_existing_lock(
    database: &Path,
    prepared: &[(u64, [u8; PAGE_SIZE])],
) -> io::Result<()> {
    let ordered = ordered_prepared_pages(prepared)?;
    commit_ordered_page_refs_with_hook_under_existing_lock(database, &ordered, |_| Ok(()))
}

pub(crate) fn commit_ordered_page_refs_under_existing_lock(
    database: &Path,
    prepared: &[(u64, &[u8; PAGE_SIZE])],
) -> io::Result<()> {
    if prepared.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(invalid_input("prepared page ids must be strictly increasing"));
    }
    commit_ordered_page_refs_with_hook_under_existing_lock(database, prepared, |_| Ok(()))
}

#[cfg(test)]
fn commit_prepared_pages_with_hook(
    database: &Path,
    prepared: &[(u64, [u8; PAGE_SIZE])],
    hook: impl FnMut(CommitBoundary) -> io::Result<()>,
) -> io::Result<()> {
    with_exclusive_sidecar(database, || {
        recover_pending_under_existing_lock(database)?;
        commit_prepared_pages_with_hook_under_existing_lock(database, prepared, hook)
    })
}

#[cfg(test)]
fn commit_prepared_pages_with_hook_under_existing_lock(
    database_path: &Path,
    prepared: &[(u64, [u8; PAGE_SIZE])],
    hook: impl FnMut(CommitBoundary) -> io::Result<()>,
) -> io::Result<()> {
    let ordered = ordered_prepared_pages(prepared)?;
    commit_ordered_page_refs_with_hook_under_existing_lock(database_path, &ordered, hook)
}

fn commit_ordered_page_refs_with_hook_under_existing_lock(
    database_path: &Path,
    ordered: &[(u64, &[u8; PAGE_SIZE])],
    mut hook: impl FnMut(CommitBoundary) -> io::Result<()>,
) -> io::Result<()> {
    if ordered.is_empty() {
        return Err(invalid_input("prepared transaction is empty"));
    }
    let active_path = wal_path(database_path);
    let temporary_path = wal_temporary_path(database_path);
    if active_path.try_exists()? || temporary_path.try_exists()? {
        return Err(invalid_data("pending WAL must be recovered before commit"));
    }
    let mut database = OpenOptions::new().read(true).write(true).open(database_path)?;
    let (original_header, final_header, final_length) = validate_prepared_pages(&mut database, ordered)?;
    let original_length = database.metadata()?.len();
    let original_page_count = original_length / PAGE_SIZE_U64;
    let mut before_images = Vec::new();
    before_images.try_reserve(ordered.len()).map_err(out_of_memory("before-image allocation failed"))?;
    for (page_id, _) in ordered {
        if *page_id < original_page_count {
            let page = read_page(&mut database, *page_id)?;
            if *page_id == 0 {
                DatabaseHeader::decode(&page)?;
            } else {
                SlottedPage::open(page.as_slice(), *page_id, original_header.next_page_id)?;
            }
            before_images.push(BeforeImage { page_id: *page_id, page });
        }
    }
    let transaction_id = new_transaction_id()?;
    let wal_header = WalHeader {
        database_id: original_header.database_id,
        transaction_id,
        original_database_len: original_length,
        original_generation: original_header.generation,
    };
    let mut temporary = OpenOptions::new().write(true).create_new(true).open(&temporary_path)?;
    temporary.write_all(&wal_header.encode()?)?;
    temporary.sync_all()?;
    hook(CommitBoundary::WalTempSynced)?;
    drop(temporary);

    std::fs::rename(&temporary_path, &active_path)?;
    let mut context = WalErrorContext {
        outcome: WalOutcome::RecoveryPending,
        database: database_path,
        wal: &active_path,
        transaction_id,
    };
    context.wrap("sync parent directory after WAL activation", sync_parent_directory(&active_path))?;
    context.wrap("WalActivated hook", hook(CommitBoundary::WalActivated))?;

    let mut wal = context.wrap("open activated WAL", OpenOptions::new().append(true).open(&active_path))?;
    let mut record = Vec::new();
    let reserve = record
        .try_reserve_exact(RECORD_HEADER_LEN + BEFORE_IMAGE_PAYLOAD_LEN)
        .map_err(out_of_memory("WAL record allocation failed"));
    context.wrap("allocate WAL record buffer", reserve)?;
    for image in &before_images {
        context.wrap("encode WAL before-image", image.encode_record_into(&mut record))?;
        context.wrap("append WAL before-image", wal.write_all(&record))?;
    }
    context.wrap("sync WAL before-images", wal.sync_data())?;
    context.wrap("BeforeImagesSynced hook", hook(CommitBoundary::BeforeImagesSynced))?;

    context.wrap("resize database", database.set_len(final_length))?;
    for (page_id, page) in ordered {
        context.wrap("write database page", write_page(&mut database, *page_id, page))?;
    }
    context.wrap("DatabaseWritten hook", hook(CommitBoundary::DatabaseWritten))?;
    context.wrap("sync database", database.sync_all())?;
    context.wrap("DatabaseSynced hook", hook(CommitBoundary::DatabaseSynced))?;

    let final_header_page = ordered
        .first()
        .map(|(_, page)| page)
        .ok_or_else(|| invalid_input("prepared header page is missing"));
    let final_header_page = context.wrap("locate prepared database header", final_header_page)?;
    let commit = CommitRecord {
        transaction_id,
        new_generation: final_header.generation,
        database_length: final_length,
        database_header_crc32: context.wrap(
            "read prepared database header checksum",
            database_header_crc32(final_header_page),
        )?,
    };
    context.wrap("encode WAL commit record", commit.encode_record_into(&mut record))?;
    context.wrap("append WAL commit record", wal.write_all(&record))?;
    context.wrap("CommitAppended hook", hook(CommitBoundary::CommitAppended))?;
    context.wrap("sync WAL commit record", wal.sync_data())?;
    context.outcome = WalOutcome::CommittedCleanupPending;
    context.wrap("CommitSynced hook", hook(CommitBoundary::CommitSynced))?;
    drop(wal);

    context.wrap("invalidate sorted index after commit", invalidate_sorted_index(database_path))?;

    clear_wal(database_path, transaction_id, context.outcome)?;
    context.wrap("WalCleared hook", hook(CommitBoundary::WalCleared))
}

#[derive(Debug)]
pub(crate) struct RecoveryRequired {
    database: PathBuf,
    pending: PathBuf,
    cause: Option<io::Error>,
}

impl fmt::Display for RecoveryRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "recovery required for {} because {} exists",
            self.database.display(),
            self.pending.display()
        )?;
        if let Some(cause) = &self.cause {
            write!(formatter, ": {cause}")?;
        }
        Ok(())
    }
}

impl Error for RecoveryRequired {
    fn source(&self) -> Option<&(dyn Error + 'static)> { self.cause.as_ref().map(|cause| cause as &(dyn Error + 'static)) }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) fn wal_path(database: &Path) -> PathBuf { append_suffix(database, ".wal") }

pub(crate) fn wal_temporary_path(database: &Path) -> PathBuf { append_suffix(database, ".wal.tmp") }

pub(crate) fn recovery_required(database: &Path, cause: io::Error) -> io::Error {
    let active = wal_path(database);
    let pending = if active.exists() { active } else { wal_temporary_path(database) };
    io::Error::other(RecoveryRequired { database: database.to_path_buf(), pending, cause: Some(cause) })
}

fn open_sidecar(database: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sidecar_lock_path(database))
}

pub(crate) struct SharedSidecarGuard {
    _file: File,
}

impl SharedSidecarGuard {
    pub(crate) fn acquire(database: &Path) -> io::Result<Self> {
        let file = open_sidecar(database)?;
        file.lock_shared()?;
        Ok(Self { _file: file })
    }
}

pub(crate) struct ExclusiveSidecarGuard {
    _file: File,
}

impl ExclusiveSidecarGuard {
    pub(crate) fn acquire(database: &Path) -> io::Result<Self> {
        let file = open_sidecar(database)?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn with_exclusive_sidecar<T>(database: &Path, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let _guard = ExclusiveSidecarGuard::acquire(database)?;
    operation()
}

fn validate_recovery_before_images(
    parsed: &ParsedWal,
    current_header_page: &[u8; PAGE_SIZE],
) -> io::Result<DatabaseHeader> {
    let original_header = match parsed.before_images.iter().find(|image| image.page_id == 0) {
        Some(image) => DatabaseHeader::decode(&image.page)?,
        None if parsed.before_images.is_empty() => DatabaseHeader::decode(current_header_page)?,
        None => return Err(invalid_data("WAL before-images omit database header page 0")),
    };
    if original_header.database_id != parsed.header.database_id {
        return Err(invalid_data("WAL before-image database identity does not match WAL header"));
    }
    if original_header.generation != parsed.header.original_generation {
        return Err(invalid_data("WAL before-image generation does not match WAL header"));
    }
    if database_length(original_header.next_page_id)? != parsed.header.original_database_len {
        return Err(invalid_data("WAL original length does not match before-image header"));
    }
    let original_page_count = parsed.header.original_database_len / PAGE_SIZE_U64;
    for image in &parsed.before_images {
        if image.page_id >= original_page_count {
            return Err(invalid_data("WAL before-image page id exceeds original database"));
        }
        if image.page_id != 0 {
            SlottedPage::open(image.page.as_slice(), image.page_id, original_header.next_page_id)?;
        }
    }
    Ok(original_header)
}

fn validate_current_identity(
    database_path: &Path,
    current_header_page: &[u8; PAGE_SIZE],
    expected: [u8; 16],
) -> io::Result<()> {
    let actual = bytes_at::<16>(current_header_page, 48)?;
    if actual == expected {
        Ok(())
    } else {
        Err(invalid_data(format!(
            "WAL recovery for {} refused: WAL database identity {expected:02x?} does not match current database identity {actual:02x?}",
            database_path.display()
        )))
    }
}

fn validate_committed_database(
    database: &mut File,
    parsed: &ParsedWal,
    commit: CommitRecord,
    current_header_page: &[u8; PAGE_SIZE],
) -> io::Result<()> {
    if parsed.torn_tail {
        return Err(invalid_data("committed WAL has a torn trailing record"));
    }
    let current_length = database.metadata()?.len();
    if current_length != commit.database_length {
        return Err(invalid_data("committed WAL database length mismatch"));
    }
    let current_header = DatabaseHeader::decode(current_header_page)?;
    if current_header.database_id != parsed.header.database_id {
        return Err(invalid_data("committed WAL database identity mismatch"));
    }
    if current_header.generation != commit.new_generation {
        return Err(invalid_data("committed WAL database generation mismatch"));
    }
    if database_length(current_header.next_page_id)? != commit.database_length {
        return Err(invalid_data("committed WAL header length mismatch"));
    }
    if database_header_crc32(current_header_page)? != commit.database_header_crc32 {
        return Err(invalid_data("committed WAL database header checksum mismatch"));
    }
    Ok(())
}

fn rollback_uncommitted<H>(
    database: &mut File,
    database_path: &Path,
    parsed: &ParsedWal,
    hook: &mut H,
) -> io::Result<()>
where
    H: FnMut(RecoveryBoundary) -> io::Result<()>,
{
    let active_path = wal_path(database_path);
    let outcome = WalOutcome::RecoveryPending;
    let transaction_id = parsed.header.transaction_id;
    for image in parsed.before_images.iter().filter(|image| image.page_id != 0) {
        write_page(database, image.page_id, &image.page).map_err(wal_operation_error(
            outcome,
            database_path,
            &active_path,
            transaction_id,
            "restore database page",
        ))?;
        hook(RecoveryBoundary::PageRestored(image.page_id)).map_err(wal_operation_error(
            outcome,
            database_path,
            &active_path,
            transaction_id,
            "PageRestored hook",
        ))?;
    }
    if let Some(header) = parsed.before_images.iter().find(|image| image.page_id == 0) {
        write_page(database, 0, &header.page).map_err(wal_operation_error(
            outcome,
            database_path,
            &active_path,
            transaction_id,
            "restore database header",
        ))?;
        hook(RecoveryBoundary::HeaderRestored).map_err(wal_operation_error(
            outcome,
            database_path,
            &active_path,
            transaction_id,
            "HeaderRestored hook",
        ))?;
    }
    database.set_len(parsed.header.original_database_len).map_err(wal_operation_error(
        outcome,
        database_path,
        &active_path,
        transaction_id,
        "truncate database after rollback",
    ))?;
    hook(RecoveryBoundary::DatabaseTruncated).map_err(wal_operation_error(
        outcome,
        database_path,
        &active_path,
        transaction_id,
        "DatabaseTruncated hook",
    ))?;
    database.sync_all().map_err(wal_operation_error(
        outcome,
        database_path,
        &active_path,
        transaction_id,
        "sync rolled-back database",
    ))?;
    hook(RecoveryBoundary::DatabaseSynced).map_err(wal_operation_error(
        outcome,
        database_path,
        &active_path,
        transaction_id,
        "DatabaseSynced recovery hook",
    ))?;
    clear_wal_with_hook(database_path, transaction_id, outcome, hook)?;
    info!(
        "rolled back uncommitted WAL database={} transaction_id={}",
        database_path.display(),
        parsed.header.transaction_id
    );
    Ok(())
}

pub(crate) fn recover_pending(database: &Path) -> io::Result<()> {
    with_exclusive_sidecar(database, || recover_pending_under_existing_lock(database))
}

pub(crate) fn recover_pending_under_existing_lock(database_path: &Path) -> io::Result<()> {
    recover_pending_with_hook_under_existing_lock(database_path, |_| Ok(()))
}

#[cfg(test)]
fn recover_pending_with_hook(
    database: &Path,
    hook: impl FnMut(RecoveryBoundary) -> io::Result<()>,
) -> io::Result<()> {
    with_exclusive_sidecar(database, || recover_pending_with_hook_under_existing_lock(database, hook))
}

fn recover_pending_with_hook_under_existing_lock<H>(database_path: &Path, mut hook: H) -> io::Result<()>
where
    H: FnMut(RecoveryBoundary) -> io::Result<()>,
{
    let active_path = wal_path(database_path);
    let temporary_path = wal_temporary_path(database_path);
    if !active_path.try_exists()? {
        if temporary_path.try_exists()? {
            std::fs::remove_file(temporary_path)?;
            sync_parent_directory(database_path)?;
        }
        return Ok(());
    }

    let parsed = read_wal(&active_path).map_err(|cause| wal_read_error(database_path, &active_path, cause))?;
    let mut database = OpenOptions::new().read(true).write(true).open(database_path)?;
    let current_length = database.metadata()?.len();
    if current_length < PAGE_SIZE_U64 || current_length < parsed.header.original_database_len {
        return Err(invalid_data("database is shorter than WAL recovery requires"));
    }
    let current_header_page = read_page(&mut database, 0)?;
    validate_current_identity(database_path, &current_header_page, parsed.header.database_id)?;
    let original_header = validate_recovery_before_images(&parsed, &current_header_page)?;

    if let Some(commit) = parsed.commit {
        validate_committed_database(&mut database, &parsed, commit, &current_header_page)?;
        invalidate_sorted_index(database_path).map_err(wal_operation_error(
            WalOutcome::CommittedCleanupPending,
            database_path,
            &active_path,
            parsed.header.transaction_id,
            "invalidate sorted index during committed recovery",
        ))?;
        clear_wal_with_hook(
            database_path,
            parsed.header.transaction_id,
            WalOutcome::CommittedCleanupPending,
            &mut hook,
        )?;
        info!(
            "cleared committed WAL database={} transaction_id={}",
            database_path.display(),
            parsed.header.transaction_id
        );
        Ok(())
    } else {
        if parsed.before_images.is_empty()
            && (current_length != parsed.header.original_database_len
                || original_header.generation != parsed.header.original_generation)
        {
            return Err(invalid_data("header-only WAL does not match unchanged database"));
        }
        rollback_uncommitted(&mut database, database_path, &parsed, &mut hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::bplustree::v3::{
        format::{write_page_checksum, DatabaseHeader, PAGE_SIZE},
        page::encode_free_page,
        tree::{BPlusTree, BPlusTreeQuery},
    };
    use std::{
        fs,
        io::{Read, Seek, SeekFrom, Write},
    };

    const DATABASE_ID: [u8; 16] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    ];
    const TRANSACTION_ID: u64 = 0x0102_0304_0506_0708;

    fn checksum_with_zeroed_field(bytes: &[u8], offset: usize) -> io::Result<u32> {
        let end = offset.checked_add(4).ok_or_else(|| io::Error::other("checksum offset overflow"))?;
        let before = bytes.get(..offset).ok_or_else(|| io::Error::other("missing checksum prefix"))?;
        let after = bytes.get(end..).ok_or_else(|| io::Error::other("missing checksum suffix"))?;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(before);
        hasher.update(&[0; 4]);
        hasher.update(after);
        Ok(hasher.finalize())
    }

    fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
        let end = offset.checked_add(4).ok_or_else(|| io::Error::other("u32 offset overflow"))?;
        let encoded: [u8; 4] = bytes
            .get(offset..end)
            .ok_or_else(|| io::Error::other("truncated u32"))?
            .try_into()
            .map_err(io::Error::other)?;
        Ok(u32::from_le_bytes(encoded))
    }

    fn sample_header() -> WalHeader {
        WalHeader {
            database_id: DATABASE_ID,
            transaction_id: TRANSACTION_ID,
            original_database_len: 3 * PAGE_SIZE as u64,
            original_generation: 9,
        }
    }

    fn sample_before_image() -> BeforeImage {
        let mut page = [0u8; PAGE_SIZE];
        for (index, byte) in page.iter_mut().enumerate() {
            *byte = index.to_le_bytes()[0];
        }
        BeforeImage { page_id: 2, page }
    }

    fn sample_commit() -> CommitRecord {
        CommitRecord {
            transaction_id: TRANSACTION_ID,
            new_generation: 10,
            database_length: 4 * PAGE_SIZE as u64,
            database_header_crc32: 0x4433_2211,
        }
    }

    fn create_database(name: &str) -> io::Result<(tempfile::TempDir, PathBuf, DatabaseHeader, [u8; PAGE_SIZE])> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut tree = BPlusTree::<u32, String>::new();
        tree.insert(1, String::from("original"));
        tree.store(&path)?;
        let mut original_page = [0u8; PAGE_SIZE];
        File::open(&path)?.read_exact(&mut original_page)?;
        let original_header = DatabaseHeader::decode(&original_page)?;
        Ok((directory, path, original_header, original_page))
    }

    fn prepared_header_page(original: &DatabaseHeader) -> io::Result<[u8; PAGE_SIZE]> {
        let mut updated = original.clone();
        updated.generation = updated
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("test generation overflow"))?;
        updated.encode()
    }

    fn database_header(path: &Path) -> io::Result<DatabaseHeader> {
        let mut page = [0u8; PAGE_SIZE];
        File::open(path)?.read_exact(&mut page)?;
        DatabaseHeader::decode(&page)
    }

    fn fail_at(target: CommitBoundary) -> impl FnMut(CommitBoundary) -> io::Result<()> {
        move |actual| {
            if actual == target {
                Err(io::Error::other(format!("fault at {actual:?}")))
            } else {
                Ok(())
            }
        }
    }

    fn fail_recovery_at(target: RecoveryBoundary) -> impl FnMut(RecoveryBoundary) -> io::Result<()> {
        move |actual| {
            if actual == target {
                Err(io::Error::other(format!("recovery fault at {actual:?}")))
            } else {
                Ok(())
            }
        }
    }

    fn create_multi_page_database(
        name: &str,
    ) -> io::Result<(tempfile::TempDir, PathBuf, DatabaseHeader, Vec<u8>)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut tree = BPlusTree::<u32, String>::new();
        for key in 0..160 {
            tree.insert(key, format!("value-{key:03}-{}", "x".repeat(96)));
        }
        tree.store(&path)?;
        let bytes = fs::read(&path)?;
        let header_page: [u8; PAGE_SIZE] = bytes
            .get(..PAGE_SIZE)
            .ok_or_else(|| io::Error::other("multi-page database lacks a header"))?
            .try_into()
            .map_err(io::Error::other)?;
        let header = DatabaseHeader::decode(&header_page)?;
        if header.next_page_id < 3 {
            return Err(io::Error::other("multi-page fixture did not create two data pages"));
        }
        Ok((directory, path, header, bytes))
    }

    fn mutate_fixture_page(
        mut page: [u8; PAGE_SIZE],
        page_id: u64,
        next_page_id: u64,
        marker: u8,
    ) -> io::Result<[u8; PAGE_SIZE]> {
        let header = SlottedPage::open(page.as_slice(), page_id, next_page_id)?.header();
        let free_start = usize::from(header.free_start);
        if free_start >= usize::from(header.free_end) {
            return Err(io::Error::other("fixture data page has no unused byte"));
        }
        let byte = page
            .get_mut(free_start)
            .ok_or_else(|| io::Error::other("fixture free-space offset is outside the page"))?;
        *byte ^= marker;
        write_page_checksum(&mut page)?;
        SlottedPage::open(page.as_slice(), page_id, next_page_id)?;
        Ok(page)
    }

    fn create_uncommitted_multi_page_wal(
        name: &str,
    ) -> io::Result<(tempfile::TempDir, PathBuf, Vec<u8>, Vec<u8>)> {
        let (directory, path, original, original_bytes) = create_multi_page_database(name)?;
        let appended_page_id = original.next_page_id;
        let mut updated = original.clone();
        updated.generation = updated
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("test generation overflow"))?;
        updated.next_page_id = updated
            .next_page_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("test page-id overflow"))?;
        updated.free_page_head = appended_page_id;

        let prepared_len = usize::try_from(updated.next_page_id).map_err(io::Error::other)?;
        let mut prepared = Vec::with_capacity(prepared_len);
        prepared.push((0, updated.encode()?));
        for page_id in 1..original.next_page_id {
            let start = usize::try_from(page_id)
                .map_err(io::Error::other)?
                .checked_mul(PAGE_SIZE)
                .ok_or_else(|| io::Error::other("test page offset overflow"))?;
            let end = start
                .checked_add(PAGE_SIZE)
                .ok_or_else(|| io::Error::other("test page end overflow"))?;
            let page: [u8; PAGE_SIZE] = original_bytes
                .get(start..end)
                .ok_or_else(|| io::Error::other("multi-page fixture is truncated"))?
                .try_into()
                .map_err(io::Error::other)?;
            let page = match page_id {
                1 => mutate_fixture_page(page, page_id, updated.next_page_id, 0x5a)?,
                2 => mutate_fixture_page(page, page_id, updated.next_page_id, 0xa5)?,
                _ => page,
            };
            prepared.push((page_id, page));
        }
        prepared.push((
            appended_page_id,
            encode_free_page(appended_page_id, updated.next_page_id, 0)?,
        ));
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::DatabaseWritten));
        let active = wal_path(&path);
        if !active.try_exists()? {
            return Err(io::Error::other("uncommitted multi-page WAL was not activated"));
        }
        Ok((directory, path, original_bytes, fs::read(active)?))
    }

    fn fixture_page(bytes: &[u8], page_id: u64) -> io::Result<&[u8]> {
        let start = usize::try_from(page_id)
            .map_err(io::Error::other)?
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| io::Error::other("fixture page offset overflow"))?;
        let end = start
            .checked_add(PAGE_SIZE)
            .ok_or_else(|| io::Error::other("fixture page end overflow"))?;
        bytes.get(start..end).ok_or_else(|| io::Error::other("fixture page is truncated"))
    }

    fn assert_wal_outcome(error: &io::Error, expected: WalOutcome, path: &Path) -> io::Result<()> {
        let operation = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<WalOperationError>())
            .ok_or_else(|| io::Error::other("error lacked typed WAL outcome"))?;
        assert_eq!(operation.outcome(), expected);
        assert_eq!(operation.database_path(), path);
        assert_eq!(operation.wal_path(), wal_path(path));
        assert_ne!(operation.transaction_id(), 0);
        assert!(!operation.phase().is_empty());
        assert!(operation.source().is_some());
        Ok(())
    }

    fn write_active_wal(path: &Path, header: &WalHeader, records: &[Vec<u8>]) -> io::Result<()> {
        let mut file = File::create(wal_path(path))?;
        file.write_all(&header.encode()?)?;
        for record in records {
            file.write_all(record)?;
        }
        file.sync_all()
    }

    fn rebuild_record_crc(record: &mut [u8]) -> io::Result<()> {
        let payload_length = usize::try_from(read_u32(record, 4)?).map_err(io::Error::other)?;
        let record_length = 16usize
            .checked_add(payload_length)
            .ok_or_else(|| io::Error::other("record length overflow"))?;
        let bytes = record
            .get(..record_length)
            .ok_or_else(|| io::Error::other("truncated record fixture"))?;
        let checksum = checksum_with_zeroed_field(bytes, 8)?;
        record
            .get_mut(8..12)
            .ok_or_else(|| io::Error::other("missing record checksum"))?
            .copy_from_slice(&checksum.to_le_bytes());
        Ok(())
    }

    #[test]
    fn wal_header_golden_layout_and_round_trip() -> io::Result<()> {
        let header = sample_header();
        let encoded = header.encode()?;
        assert_eq!(encoded.len(), 64);
        assert_eq!(&encoded[0..4], b"BTW3");
        assert_eq!(&encoded[4..8], &1u32.to_le_bytes());
        assert_eq!(&encoded[8..12], &64u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &4096u32.to_le_bytes());
        assert_eq!(&encoded[16..32], &DATABASE_ID);
        assert_eq!(&encoded[32..40], &TRANSACTION_ID.to_le_bytes());
        assert_eq!(&encoded[40..48], &(3 * PAGE_SIZE as u64).to_le_bytes());
        assert_eq!(&encoded[48..56], &9u64.to_le_bytes());
        assert_eq!(read_u32(&encoded, 56)?, 0x3ad1_eb4d);
        assert_eq!(read_u32(&encoded, 56)?, checksum_with_zeroed_field(&encoded, 56)?);
        assert_eq!(&encoded[60..64], &[0; 4]);
        assert_eq!(WalHeader::decode(&encoded)?, header);
        Ok(())
    }

    #[test]
    fn before_image_golden_layout_and_round_trip() -> io::Result<()> {
        let image = sample_before_image();
        let encoded = image.encode_record()?;
        assert_eq!(encoded.len(), 16 + 4112);
        assert_eq!(encoded[0], 1);
        assert_eq!(encoded[1], 0);
        assert_eq!(&encoded[2..4], &[0; 2]);
        assert_eq!(&encoded[4..8], &4112u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &[0; 4]);
        assert_eq!(&encoded[16..24], &2u64.to_le_bytes());
        assert_eq!(read_u32(&encoded, 24)?, 0xa291_2082);
        assert_eq!(read_u32(&encoded, 24)?, crc32fast::hash(&image.page));
        assert_eq!(&encoded[28..32], &[0; 4]);
        assert_eq!(&encoded[32..], &image.page);
        assert_eq!(read_u32(&encoded, 8)?, 0x9ea7_c8fc);
        assert_eq!(read_u32(&encoded, 8)?, checksum_with_zeroed_field(&encoded, 8)?);
        assert_eq!(BeforeImage::decode_payload(&encoded[16..])?, image);
        Ok(())
    }

    #[test]
    fn commit_record_golden_layout_and_round_trip() -> io::Result<()> {
        let commit = sample_commit();
        let encoded = commit.encode_record()?;
        assert_eq!(encoded.len(), 48);
        assert_eq!(encoded[0], 2);
        assert_eq!(encoded[1], 0);
        assert_eq!(&encoded[2..4], &[0; 2]);
        assert_eq!(&encoded[4..8], &32u32.to_le_bytes());
        assert_eq!(&encoded[12..16], &[0; 4]);
        assert_eq!(&encoded[16..24], &TRANSACTION_ID.to_le_bytes());
        assert_eq!(&encoded[24..32], &10u64.to_le_bytes());
        assert_eq!(&encoded[32..40], &(4 * PAGE_SIZE as u64).to_le_bytes());
        assert_eq!(&encoded[40..44], &0x4433_2211u32.to_le_bytes());
        assert_eq!(&encoded[44..48], &[0; 4]);
        assert_eq!(read_u32(&encoded, 8)?, 0xb9d2_319c);
        assert_eq!(read_u32(&encoded, 8)?, checksum_with_zeroed_field(&encoded, 8)?);
        assert_eq!(CommitRecord::decode_payload(&encoded[16..])?, commit);
        Ok(())
    }

    #[test]
    fn rejects_invalid_header_fields_and_checksum() -> io::Result<()> {
        for offset in [0usize, 4, 8, 12, 16, 32, 40, 48, 56, 60] {
            let mut encoded = sample_header().encode()?;
            let byte = encoded.get_mut(offset).ok_or_else(|| io::Error::other("bad fixture offset"))?;
            *byte ^= 0x5a;
            assert!(WalHeader::decode(&encoded).is_err(), "offset {offset} was accepted");
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_record_fields_and_duplicates() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("invalid-record.db");
        let header = sample_header();
        let before = sample_before_image().encode_record()?;
        let commit = sample_commit().encode_record()?;

        for offset in [0usize, 1, 2, 4, 8, 12] {
            let mut invalid = before.clone();
            let byte = invalid.get_mut(offset).ok_or_else(|| io::Error::other("bad record fixture offset"))?;
            *byte ^= 0x7f;
            write_active_wal(&path, &header, &[invalid])?;
            assert!(read_wal(&wal_path(&path)).is_err(), "record offset {offset} was accepted");
        }

        write_active_wal(&path, &header, &[before.clone(), before.clone()])?;
        assert!(read_wal(&wal_path(&path)).is_err());

        let mut lower = sample_before_image();
        lower.page_id = 1;
        write_active_wal(&path, &header, &[before.clone(), lower.encode_record()?])?;
        assert_eq!(read_wal(&wal_path(&path))?.before_images.len(), 2);

        write_active_wal(&path, &header, &[commit.clone(), before.clone()])?;
        assert!(read_wal(&wal_path(&path)).is_err());
        write_active_wal(&path, &header, &[commit.clone(), commit])?;
        assert!(read_wal(&wal_path(&path)).is_err());

        let mut skipped_generation = sample_commit();
        skipped_generation.new_generation += 1;
        write_active_wal(&path, &header, &[skipped_generation.encode_record()?])?;
        assert!(read_wal(&wal_path(&path)).is_err());

        let mut bad_page_crc = before;
        bad_page_crc[24] ^= 1;
        rebuild_record_crc(&mut bad_page_crc)?;
        write_active_wal(&path, &header, &[bad_page_crc])?;
        assert!(read_wal(&wal_path(&path)).is_err());

        let mut out_of_bounds = sample_before_image();
        out_of_bounds.page_id = 3;
        write_active_wal(&path, &header, &[out_of_bounds.encode_record()?])?;
        assert!(read_wal(&wal_path(&path)).is_err());
        Ok(())
    }

    #[test]
    fn torn_final_record_rolls_back_complete_before_images() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("torn-tail.db")?;
        let updated_page = prepared_header_page(&original)?;
        let prepared = [(0, updated_page)];
        let error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::DatabaseWritten))
            .err()
            .ok_or_else(|| io::Error::other("fault did not interrupt commit"))?;
        assert!(error.to_string().contains("DatabaseWritten"));
        let mut wal = OpenOptions::new().append(true).open(wal_path(&path))?;
        wal.write_all(&[2, 0, 0, 0, 32])?;
        wal.sync_all()?;
        drop(wal);

        assert_eq!(database_header(&path)?.generation, original.generation + 1);
        recover_pending(&path)?;
        assert_eq!(database_header(&path)?.generation, original.generation);
        assert!(!wal_path(&path).try_exists()?);
        recover_pending(&path)?;
        Ok(())
    }

    #[test]
    fn every_commit_boundary_recovers_idempotently() -> io::Result<()> {
        for boundary in [
            CommitBoundary::WalTempSynced,
            CommitBoundary::WalActivated,
            CommitBoundary::BeforeImagesSynced,
            CommitBoundary::DatabaseWritten,
            CommitBoundary::DatabaseSynced,
            CommitBoundary::CommitAppended,
            CommitBoundary::CommitSynced,
            CommitBoundary::WalCleared,
        ] {
            let (_directory, path, original, _) = create_database(&format!("boundary-{boundary:?}.db"))?;
            let updated_page = prepared_header_page(&original)?;
            let prepared = [(0, updated_page)];
            let error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(boundary))
                .err()
                .ok_or_else(|| io::Error::other(format!("{boundary:?} did not interrupt commit")))?;
            assert!(error.to_string().contains(&format!("{boundary:?}")));

            recover_pending(&path)?;
            recover_pending(&path)?;
            let generation = database_header(&path)?.generation;
            let committed = matches!(
                boundary,
                CommitBoundary::CommitAppended | CommitBoundary::CommitSynced | CommitBoundary::WalCleared
            );
            assert_eq!(generation, original.generation + u64::from(committed), "boundary {boundary:?}");
            assert!(!wal_path(&path).try_exists()?);
            assert!(!wal_temporary_path(&path).try_exists()?);
        }
        Ok(())
    }

    #[test]
    fn committed_recovery_invalidates_sorted_index_before_clearing_wal() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("committed-index-recovery.db")?;
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
        fs::write(&index_path, b"stale sorted index")?;
        let prepared = [(0, prepared_header_page(&original)?)];

        let error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::CommitSynced))
            .err()
            .ok_or_else(|| io::Error::other("CommitSynced did not interrupt commit"))?;
        assert_wal_outcome(&error, WalOutcome::CommittedCleanupPending, &path)?;
        assert!(index_path.try_exists()?);
        assert!(wal_path(&path).try_exists()?);

        recover_pending(&path)?;
        assert_eq!(database_header(&path)?.generation, original.generation + 1);
        assert!(!index_path.try_exists()?);
        assert!(!wal_path(&path).try_exists()?);
        Ok(())
    }

    #[test]
    fn late_commit_failures_report_the_durable_wal_outcome() -> io::Result<()> {
        for (boundary, expected) in [
            (CommitBoundary::WalActivated, WalOutcome::RecoveryPending),
            (CommitBoundary::CommitAppended, WalOutcome::RecoveryPending),
            (CommitBoundary::CommitSynced, WalOutcome::CommittedCleanupPending),
            (CommitBoundary::WalCleared, WalOutcome::CommittedCleanupPending),
        ] {
            let (_directory, path, original, _) = create_database(&format!("outcome-{boundary:?}.db"))?;
            let prepared = [(0, prepared_header_page(&original)?)];
            let error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(boundary))
                .err()
                .ok_or_else(|| io::Error::other(format!("{boundary:?} did not interrupt commit")))?;
            let operation = error
                .get_ref()
                .and_then(|source| source.downcast_ref::<WalOperationError>())
                .ok_or_else(|| io::Error::other(format!("{boundary:?} lacked typed WAL outcome")))?;
            assert_eq!(operation.outcome(), expected);
            assert_eq!(operation.database_path(), path);
            assert_eq!(operation.wal_path(), wal_path(&path));
            assert_ne!(operation.transaction_id(), 0);
            assert!(!operation.phase().is_empty());
            assert!(operation.source().is_some());
        }

        let (_directory, path, original, _) = create_database("pre-rename-error.db")?;
        let prepared = [(0, prepared_header_page(&original)?)];
        let error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::WalTempSynced))
            .err()
            .ok_or_else(|| io::Error::other("pre-rename fault did not interrupt commit"))?;
        assert!(error.get_ref().and_then(|source| source.downcast_ref::<WalOperationError>()).is_none());
        Ok(())
    }

    #[test]
    fn committed_mismatch_leaves_database_and_wal_untouched() -> io::Result<()> {
        for mismatch in ["length", "identity", "generation", "header-page-crc", "commit-header-crc", "transaction"] {
            let (_directory, path, original, _) = create_database(&format!("mismatch-{mismatch}.db"))?;
            let updated_page = prepared_header_page(&original)?;
            let prepared = [(0, updated_page)];
            let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::CommitSynced));

            match mismatch {
                "length" => {
                    let file = OpenOptions::new().write(true).open(&path)?;
                    file.set_len(file.metadata()?.len() + PAGE_SIZE as u64)?;
                }
                "identity" => {
                    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                    file.seek(SeekFrom::Start(48))?;
                    file.write_all(&[0x77; 16])?;
                    file.sync_all()?;
                }
                "generation" => {
                    let mut page = [0u8; PAGE_SIZE];
                    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                    file.read_exact(&mut page)?;
                    let mut header = DatabaseHeader::decode(&page)?;
                    header.generation += 1;
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&header.encode()?)?;
                    file.sync_all()?;
                }
                "header-page-crc" => {
                    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                    file.seek(SeekFrom::Start(72))?;
                    file.write_all(&[0; 4])?;
                    file.sync_all()?;
                }
                "commit-header-crc" => {
                    let wal_path = wal_path(&path);
                    let mut bytes = fs::read(&wal_path)?;
                    let record_offset = 64 + 16 + 4112;
                    let crc = bytes
                        .get_mut(record_offset + 40..record_offset + 44)
                        .ok_or_else(|| io::Error::other("commit header CRC fixture is truncated"))?;
                    crc.copy_from_slice(&0x1122_3344u32.to_le_bytes());
                    rebuild_record_crc(
                        bytes
                            .get_mut(record_offset..)
                            .ok_or_else(|| io::Error::other("commit record fixture is missing"))?,
                    )?;
                    fs::write(wal_path, bytes)?;
                }
                "transaction" => {
                    let wal_path = wal_path(&path);
                    let mut bytes = fs::read(&wal_path)?;
                    let record_offset = 64 + 16 + 4112;
                    let tx = bytes
                        .get_mut(record_offset + 16..record_offset + 24)
                        .ok_or_else(|| io::Error::other("commit transaction fixture is truncated"))?;
                    tx.copy_from_slice(&0x9999u64.to_le_bytes());
                    rebuild_record_crc(
                        bytes
                            .get_mut(record_offset..)
                            .ok_or_else(|| io::Error::other("commit record fixture is missing"))?,
                    )?;
                    fs::write(wal_path, bytes)?;
                }
                _ => return Err(io::Error::other("unknown mismatch fixture")),
            }
            let database_before = fs::read(&path)?;
            let wal_before = fs::read(wal_path(&path))?;
            assert!(recover_pending(&path).is_err(), "{mismatch} was accepted");
            assert_eq!(fs::read(&path)?, database_before, "database changed for {mismatch}");
            assert_eq!(fs::read(wal_path(&path))?, wal_before, "WAL changed for {mismatch}");
        }
        Ok(())
    }

    #[test]
    fn abandoned_wal_temp_is_removed_without_touching_database() -> io::Result<()> {
        let (_directory, path, _, _) = create_database("abandoned-temp.db")?;
        let database_before = fs::read(&path)?;
        fs::write(wal_temporary_path(&path), b"not activated")?;
        recover_pending(&path)?;
        assert_eq!(fs::read(&path)?, database_before);
        assert!(!wal_temporary_path(&path).try_exists()?);
        Ok(())
    }

    #[test]
    fn foreign_uncommitted_wal_leaves_database_and_wal_untouched() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("foreign-wal.db")?;
        let prepared = [(0, prepared_header_page(&original)?)];
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::BeforeImagesSynced));
        let active = wal_path(&path);
        let mut bytes = fs::read(&active)?;
        bytes
            .get_mut(16..32)
            .ok_or_else(|| io::Error::other("WAL header identity fixture is truncated"))?
            .copy_from_slice(&[0x88; 16]);
        let checksum = checksum_with_zeroed_field(
            bytes.get(..64).ok_or_else(|| io::Error::other("WAL header fixture is truncated"))?,
            56,
        )?;
        bytes
            .get_mut(56..60)
            .ok_or_else(|| io::Error::other("WAL header checksum fixture is truncated"))?
            .copy_from_slice(&checksum.to_le_bytes());
        fs::write(&active, bytes)?;
        let database_before = fs::read(&path)?;
        let wal_before = fs::read(&active)?;

        let error = recover_pending(&path)
            .err()
            .ok_or_else(|| io::Error::other("foreign WAL was accepted"))?;
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains(&format!("{:02x?}", [0x88; 16])));
        assert!(message.contains(&format!("{:02x?}", original.database_id)));
        assert_eq!(fs::read(&path)?, database_before);
        assert_eq!(fs::read(&active)?, wal_before);
        Ok(())
    }

    #[test]
    fn corrupt_record_reports_recovery_paths_identities_and_source_without_mutation() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("corrupt-record-context.db")?;
        let prepared = [(0, prepared_header_page(&original)?)];
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::BeforeImagesSynced));
        let active = wal_path(&path);
        let mut bytes = fs::read(&active)?;
        let checksum_byte = bytes
            .get_mut(WAL_HEADER_LEN + 8)
            .ok_or_else(|| io::Error::other("WAL record fixture is truncated"))?;
        *checksum_byte ^= 1;
        fs::write(&active, bytes)?;
        let database_before = fs::read(&path)?;
        let wal_before = fs::read(&active)?;

        let error = recover_pending(&path)
            .err()
            .ok_or_else(|| io::Error::other("corrupt WAL record was accepted"))?;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let context = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<WalReadError>())
            .ok_or_else(|| io::Error::other("WAL parser error lacked recovery context"))?;
        assert_eq!(context.database_path(), path);
        assert_eq!(context.wal_path(), active);
        assert_eq!(context.wal_database_id(), Some(original.database_id));
        assert_eq!(context.current_database_id(), Some(original.database_id));
        assert!(context.source().and_then(|source| source.downcast_ref::<io::Error>()).is_some());
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains(&active.display().to_string()));
        assert!(message.contains(&format!("WAL database identity {:02x?}", original.database_id)));
        assert!(message.contains(&format!("current database identity {:02x?}", original.database_id)));
        assert_eq!(fs::read(&path)?, database_before);
        assert_eq!(fs::read(&active)?, wal_before);
        Ok(())
    }

    #[test]
    fn rollback_faults_leave_recovery_idempotent_at_every_database_boundary() -> io::Result<()> {
        for boundary in [
            RecoveryBoundary::PageRestored(1),
            RecoveryBoundary::HeaderRestored,
            RecoveryBoundary::DatabaseTruncated,
            RecoveryBoundary::DatabaseSynced,
        ] {
            let (_directory, path, original_bytes, _) =
                create_uncommitted_multi_page_wal(&format!("rollback-{boundary:?}.db"))?;
            let modified_bytes = fs::read(&path)?;
            assert_ne!(fixture_page(&modified_bytes, 1)?, fixture_page(&original_bytes, 1)?);
            assert_ne!(fixture_page(&modified_bytes, 2)?, fixture_page(&original_bytes, 2)?);
            let error = recover_pending_with_hook(&path, fail_recovery_at(boundary))
                .err()
                .ok_or_else(|| io::Error::other(format!("{boundary:?} did not interrupt recovery")))?;
            assert_wal_outcome(&error, WalOutcome::RecoveryPending, &path)?;
            assert!(wal_path(&path).try_exists()?);
            if boundary == RecoveryBoundary::PageRestored(1) {
                let partially_restored = fs::read(&path)?;
                assert_eq!(fixture_page(&partially_restored, 1)?, fixture_page(&original_bytes, 1)?);
                assert_eq!(fixture_page(&partially_restored, 2)?, fixture_page(&modified_bytes, 2)?);
            }

            recover_pending(&path)?;
            assert_eq!(fs::read(&path)?, original_bytes, "second recovery failed after {boundary:?}");
            assert!(!wal_path(&path).try_exists()?);
            recover_pending(&path)?;
            assert_eq!(fs::read(&path)?, original_bytes, "third recovery changed data after {boundary:?}");
        }
        Ok(())
    }

    #[test]
    fn rollback_cleanup_faults_report_recovery_pending_and_allow_reappeared_wal() -> io::Result<()> {
        for boundary in [RecoveryBoundary::WalRemoved, RecoveryBoundary::ParentDirectorySynced] {
            let (_directory, path, original_bytes, wal_bytes) =
                create_uncommitted_multi_page_wal(&format!("rollback-cleanup-{boundary:?}.db"))?;
            let error = recover_pending_with_hook(&path, fail_recovery_at(boundary))
                .err()
                .ok_or_else(|| io::Error::other(format!("{boundary:?} did not interrupt rollback cleanup")))?;
            assert_wal_outcome(&error, WalOutcome::RecoveryPending, &path)?;
            assert_eq!(fs::read(&path)?, original_bytes);
            assert!(!wal_path(&path).try_exists()?);

            fs::write(wal_path(&path), &wal_bytes)?;
            recover_pending(&path)?;
            assert_eq!(fs::read(&path)?, original_bytes);
            assert!(!wal_path(&path).try_exists()?);
            recover_pending(&path)?;
        }
        Ok(())
    }

    #[test]
    fn committed_cleanup_faults_preserve_commit_and_allow_reappeared_wal() -> io::Result<()> {
        for boundary in [RecoveryBoundary::WalRemoved, RecoveryBoundary::ParentDirectorySynced] {
            let (_directory, path, original, _) = create_database(&format!("commit-cleanup-{boundary:?}.db"))?;
            let prepared = [(0, prepared_header_page(&original)?)];
            let commit_error = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::CommitSynced))
                .err()
                .ok_or_else(|| io::Error::other("CommitSynced did not preserve committed WAL"))?;
            assert_wal_outcome(&commit_error, WalOutcome::CommittedCleanupPending, &path)?;
            let committed_bytes = fs::read(&path)?;
            let active = wal_path(&path);
            let wal_bytes = fs::read(&active)?;

            let error = recover_pending_with_hook(&path, fail_recovery_at(boundary))
                .err()
                .ok_or_else(|| io::Error::other(format!("{boundary:?} did not interrupt committed cleanup")))?;
            assert_wal_outcome(&error, WalOutcome::CommittedCleanupPending, &path)?;
            assert_eq!(fs::read(&path)?, committed_bytes);
            assert!(!active.try_exists()?);

            fs::write(&active, &wal_bytes)?;
            recover_pending(&path)?;
            assert_eq!(fs::read(&path)?, committed_bytes);
            assert!(!active.try_exists()?);
            recover_pending(&path)?;
        }
        Ok(())
    }

    #[test]
    fn rollback_truncates_new_pages_and_second_recovery_is_noop() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("truncate-appended.db")?;
        let original_length = fs::metadata(&path)?.len();
        let appended_page_id = original.next_page_id;
        let mut updated = original.clone();
        updated.generation += 1;
        updated.next_page_id += 1;
        updated.free_page_head = appended_page_id;
        let prepared = [
            (0, updated.encode()?),
            (appended_page_id, encode_free_page(appended_page_id, updated.next_page_id, 0)?),
        ];
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::DatabaseWritten));
        assert_eq!(fs::metadata(&path)?.len(), original_length + PAGE_SIZE as u64);

        recover_pending(&path)?;
        assert_eq!(fs::metadata(&path)?.len(), original_length);
        let after_first = fs::read(&path)?;
        recover_pending(&path)?;
        assert_eq!(fs::read(&path)?, after_first);
        Ok(())
    }

    #[test]
    fn query_open_recovers_uncommitted_wal_before_mapping() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("query-recovers.db")?;
        let prepared = [(0, prepared_header_page(&original)?)];
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::DatabaseWritten));
        assert_eq!(database_header(&path)?.generation, original.generation + 1);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
        assert_eq!(query.query(&1).map_err(crate::repository::bplustree::common::BPlusTreeError::to_io)?, Some(String::from("original")));
        assert_eq!(database_header(&path)?.generation, original.generation);
        assert!(!wal_path(&path).try_exists()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn query_reports_recovery_required_when_database_is_not_writable() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let (_directory, path, original, _) = create_database("readonly-recovery.db")?;
        let prepared = [(0, prepared_header_page(&original)?)];
        let _ = commit_prepared_pages_with_hook(&path, &prepared, fail_at(CommitBoundary::DatabaseWritten));
        let original_permissions = fs::metadata(&path)?.permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        let result = BPlusTreeQuery::<u32, String>::try_new(&path);
        fs::set_permissions(&path, original_permissions)?;

        let error = result.err().ok_or_else(|| io::Error::other("query recovered read-only database"))?;
        assert!(error.get_ref().and_then(|source| source.downcast_ref::<RecoveryRequired>()).is_some());
        assert!(wal_path(&path).try_exists()?);
        recover_pending(&path)?;
        Ok(())
    }

    #[test]
    fn commit_rejects_corrupt_original_page_before_creating_wal() -> io::Result<()> {
        let (_directory, path, original, _) = create_database("corrupt-original.db")?;
        let mut valid_leaf = [0u8; PAGE_SIZE];
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::Start(PAGE_SIZE_U64))?;
        file.read_exact(&mut valid_leaf)?;
        file.seek(SeekFrom::Start(PAGE_SIZE_U64 + 24))?;
        file.write_all(&[valid_leaf[24] ^ 1])?;
        file.sync_all()?;
        drop(file);
        let database_before = fs::read(&path)?;
        let prepared = [(0, prepared_header_page(&original)?), (1, valid_leaf)];

        assert!(commit_prepared_pages(&path, &prepared).is_err());
        assert_eq!(fs::read(&path)?, database_before);
        assert!(!wal_path(&path).try_exists()?);
        assert!(!wal_temporary_path(&path).try_exists()?);
        Ok(())
    }
}
