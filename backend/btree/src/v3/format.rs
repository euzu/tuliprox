use super::{BPlusTreeMetadata, RecoveryIdentity};
use std::io;

pub(crate) const PAGE_SIZE: usize = 4096;
const PAGE_SIZE_U32: u32 = 4096;
#[cfg(test)]
pub(crate) const STORAGE_VERSION_V1: u32 = 1;
#[cfg(test)]
pub(crate) const STORAGE_VERSION_V2: u32 = 2;
pub const STORAGE_VERSION_V3: u32 = 3;
pub(crate) const MAX_ENCODED_KEY_LEN: usize = 2004;
pub(crate) const MAX_CELL_FOOTPRINT: usize = 2032;
pub(crate) const MAX_INLINE_STORED_VALUE: usize = 512;
pub(crate) const OVERFLOW_PAYLOAD_LEN: usize = 4056;
pub const MAGIC: &[u8; 4] = b"BTRE";

const DATABASE_CHECKSUM_OFFSET: usize = 72;
const PAGE_CHECKSUM_OFFSET: usize = 24;
const DATABASE_METADATA_OFFSET: usize = 76;
// tag + database id + schema fingerprint + schema version + applied revision.
const RECOVERY_METADATA_LEN_U32: u32 = 1 + 16 + 32 + 4 + 8;
pub(crate) const PAGE_HEADER_LEN: usize = 32;
pub(crate) const INTERNAL_PREAMBLE_LEN: usize = 8;
pub(crate) const SLOT_LEN: usize = 4;
pub(crate) const INTERNAL_CELL_PREFIX_LEN: usize = 12;
pub(crate) const LEAF_CELL_PREFIX_LEN: usize = 24;
#[cfg(test)]
pub(crate) const OVERFLOW_HEADER_LEN: usize = 8;
const COMPRESSION_MIN_LENGTH: usize = 64;
const COMPRESSION_PERCENT: usize = 85;

fn invalid_data(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn invalid_input(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

fn checked_end(offset: usize, length: usize) -> io::Result<usize> {
    offset.checked_add(length).ok_or_else(|| invalid_data("format offset overflow"))
}

fn bytes_at<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    let end = checked_end(offset, N)?;
    bytes
        .get(offset..end)
        .ok_or_else(|| invalid_data("truncated format field"))?
        .try_into()
        .map_err(|_| invalid_data("invalid format field length"))
}

fn write_at(bytes: &mut [u8], offset: usize, value: &[u8]) -> io::Result<()> {
    let end = checked_end(offset, value.len())?;
    bytes.get_mut(offset..end).ok_or_else(|| invalid_data("truncated format destination"))?.copy_from_slice(value);
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> { Ok(u16::from_le_bytes(bytes_at(bytes, offset)?)) }

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> { Ok(u32::from_le_bytes(bytes_at(bytes, offset)?)) }

fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> { Ok(u64::from_le_bytes(bytes_at(bytes, offset)?)) }

fn read_u8(bytes: &[u8], offset: usize) -> io::Result<u8> {
    let [value] = bytes_at(bytes, offset)?;
    Ok(value)
}

fn require_zero(bytes: &[u8], message: &'static str) -> io::Result<()> {
    if bytes.iter().all(|byte| *byte == 0) {
        Ok(())
    } else {
        Err(invalid_data(message))
    }
}

fn exact_page(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() == PAGE_SIZE {
        Ok(())
    } else {
        Err(invalid_data("page must be exactly 4096 bytes"))
    }
}

fn checksum_with_zeroed_field(page: &[u8], offset: usize) -> io::Result<u32> {
    exact_page(page)?;
    let checksum_end = checked_end(offset, 4)?;
    let before = page.get(..offset).ok_or_else(|| invalid_data("missing checksum prefix"))?;
    let after = page.get(checksum_end..).ok_or_else(|| invalid_data("missing checksum suffix"))?;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(before);
    hasher.update(&[0; 4]);
    hasher.update(after);
    Ok(hasher.finalize())
}

fn write_checksum(page: &mut [u8], offset: usize) -> io::Result<()> {
    exact_page(page)?;
    write_at(page, offset, &[0; 4])?;
    let checksum = checksum_with_zeroed_field(page, offset)?;
    write_at(page, offset, &checksum.to_le_bytes())
}

fn verify_checksum(page: &[u8], offset: usize) -> io::Result<()> {
    let stored = read_u32(page, offset)?;
    if stored == checksum_with_zeroed_field(page, offset)? {
        Ok(())
    } else {
        Err(invalid_data("page checksum mismatch"))
    }
}

fn validate_database_header(header: &DatabaseHeader) -> io::Result<()> {
    if header.root_page_id == 0 || header.root_page_id >= header.next_page_id {
        return Err(invalid_data("invalid root page id"));
    }
    if header.free_page_head != 0
        && (header.free_page_head >= header.next_page_id || header.free_page_head == header.root_page_id)
    {
        return Err(invalid_data("invalid free page head"));
    }
    if header.generation == 0 {
        return Err(invalid_data("generation must be nonzero"));
    }
    if header.database_id.iter().all(|byte| *byte == 0) {
        return Err(invalid_data("database identity must be nonzero"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseHeader {
    pub(crate) root_page_id: u64,
    pub(crate) next_page_id: u64,
    pub(crate) free_page_head: u64,
    pub(crate) generation: u64,
    pub(crate) database_id: [u8; 16],
    pub(crate) metadata: BPlusTreeMetadata,
}

impl DatabaseHeader {
    pub(crate) fn encode(&self) -> io::Result<[u8; PAGE_SIZE]> {
        validate_database_header(self)?;
        let mut page = [0u8; PAGE_SIZE];
        write_at(&mut page, 0, MAGIC)?;
        write_at(&mut page, 4, &STORAGE_VERSION_V3.to_le_bytes())?;
        write_at(&mut page, 8, &PAGE_SIZE_U32.to_le_bytes())?;
        write_at(&mut page, 16, &self.root_page_id.to_le_bytes())?;
        write_at(&mut page, 24, &self.next_page_id.to_le_bytes())?;
        write_at(&mut page, 32, &self.free_page_head.to_le_bytes())?;
        write_at(&mut page, 40, &self.generation.to_le_bytes())?;
        write_at(&mut page, 48, &self.database_id)?;
        match self.metadata {
            BPlusTreeMetadata::Empty => {}
            BPlusTreeMetadata::TargetIdMapping(value) => {
                write_at(&mut page, 64, &5u32.to_le_bytes())?;
                write_at(&mut page, DATABASE_METADATA_OFFSET, &[1])?;
                write_at(&mut page, DATABASE_METADATA_OFFSET + 1, &value.to_le_bytes())?;
            }
            BPlusTreeMetadata::Recovery(identity) => {
                write_at(&mut page, 64, &RECOVERY_METADATA_LEN_U32.to_le_bytes())?;
                let mut offset = DATABASE_METADATA_OFFSET;
                write_at(&mut page, offset, &[2])?;
                offset = checked_end(offset, 1)?;
                write_at(&mut page, offset, &identity.database_id)?;
                offset = checked_end(offset, 16)?;
                write_at(&mut page, offset, &identity.schema_fingerprint)?;
                offset = checked_end(offset, 32)?;
                write_at(&mut page, offset, &identity.schema_version.to_le_bytes())?;
                offset = checked_end(offset, 4)?;
                write_at(&mut page, offset, &identity.applied_revision.to_le_bytes())?;
            }
        }
        write_checksum(&mut page, DATABASE_CHECKSUM_OFFSET)?;
        Ok(page)
    }

    pub(crate) fn decode(page: &[u8]) -> io::Result<Self> {
        exact_page(page)?;
        if bytes_at::<4>(page, 0)? != *MAGIC {
            return Err(invalid_data("invalid database magic"));
        }
        if read_u32(page, 4)? != STORAGE_VERSION_V3 {
            return Err(invalid_data("unsupported storage version"));
        }
        if read_u32(page, 8)? != PAGE_SIZE_U32 {
            return Err(invalid_data("invalid page size"));
        }
        if read_u32(page, 12)? != 0 {
            return Err(invalid_data("unknown database feature flags"));
        }
        require_zero(
            page.get(68..72).ok_or_else(|| invalid_data("missing database reserved bytes"))?,
            "database reserved bytes must be zero",
        )?;
        verify_checksum(page, DATABASE_CHECKSUM_OFFSET)?;

        let metadata_length = read_u32(page, 64)?;
        let metadata = match metadata_length {
            0 => BPlusTreeMetadata::Empty,
            5 => {
                let encoded = bytes_at::<5>(page, DATABASE_METADATA_OFFSET)?;
                let [tag, value0, value1, value2, value3] = encoded;
                if tag != 1 {
                    return Err(invalid_data("unknown metadata tag"));
                }
                BPlusTreeMetadata::TargetIdMapping(u32::from_le_bytes([value0, value1, value2, value3]))
            }
            RECOVERY_METADATA_LEN_U32 => {
                let mut offset = DATABASE_METADATA_OFFSET;
                if read_u8(page, offset)? != 2 {
                    return Err(invalid_data("unknown metadata tag"));
                }
                offset = checked_end(offset, 1)?;
                let database_id = bytes_at::<16>(page, offset)?;
                offset = checked_end(offset, 16)?;
                let schema_fingerprint = bytes_at::<32>(page, offset)?;
                offset = checked_end(offset, 32)?;
                let schema_version = read_u32(page, offset)?;
                offset = checked_end(offset, 4)?;
                let applied_revision = read_u64(page, offset)?;
                BPlusTreeMetadata::Recovery(RecoveryIdentity {
                    database_id,
                    schema_fingerprint,
                    schema_version,
                    applied_revision,
                })
            }
            _ => return Err(invalid_data("invalid metadata length")),
        };
        let tail_start = checked_end(
            DATABASE_METADATA_OFFSET,
            usize::try_from(metadata_length).map_err(|_| invalid_data("metadata length exceeds usize"))?,
        )?;
        require_zero(
            page.get(tail_start..).ok_or_else(|| invalid_data("metadata extends beyond header"))?,
            "database header tail must be zero",
        )?;

        let header = Self {
            root_page_id: read_u64(page, 16)?,
            next_page_id: read_u64(page, 24)?,
            free_page_head: read_u64(page, 32)?,
            generation: read_u64(page, 40)?,
            database_id: bytes_at(page, 48)?,
            metadata,
        };
        validate_database_header(&header)?;
        Ok(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PageType {
    Leaf = 1,
    Internal = 2,
    Overflow = 3,
    Free = 4,
}

impl TryFrom<u8> for PageType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Leaf),
            2 => Ok(Self::Internal),
            3 => Ok(Self::Overflow),
            4 => Ok(Self::Free),
            _ => Err(invalid_data("unknown page type")),
        }
    }
}

fn validate_page_id(page_id: u64, next_page_id: u64, kind: io::ErrorKind) -> io::Result<()> {
    if page_id != 0 && page_id < next_page_id {
        Ok(())
    } else {
        Err(io::Error::new(kind, "invalid page id bounds"))
    }
}

fn validate_reference(reference: u64, page_id: u64, next_page_id: u64, kind: io::ErrorKind) -> io::Result<()> {
    if reference == 0 || (reference < next_page_id && reference != page_id) {
        Ok(())
    } else {
        Err(io::Error::new(kind, "invalid page reference"))
    }
}

fn expected_slot_end(base: usize, cell_count: u16, kind: io::ErrorKind) -> io::Result<u16> {
    let slots = usize::from(cell_count)
        .checked_mul(SLOT_LEN)
        .ok_or_else(|| io::Error::new(kind, "slot directory size overflow"))?;
    let end = base.checked_add(slots).ok_or_else(|| io::Error::new(kind, "slot directory offset overflow"))?;
    u16::try_from(end).map_err(|_| io::Error::new(kind, "slot directory exceeds page"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageHeader {
    pub(crate) page_type: PageType,
    pub(crate) cell_count: u16,
    pub(crate) free_start: u16,
    pub(crate) free_end: u16,
    pub(crate) left: u64,
    pub(crate) right: u64,
}

impl PageHeader {
    fn validate(&self, page_id: u64, next_page_id: u64, kind: io::ErrorKind) -> io::Result<()> {
        validate_page_id(page_id, next_page_id, kind)?;
        match self.page_type {
            PageType::Leaf => {
                if self.free_start != expected_slot_end(PAGE_HEADER_LEN, self.cell_count, kind)? {
                    return Err(io::Error::new(kind, "invalid leaf free_start"));
                }
                if self.cell_count == 0 {
                    if usize::from(self.free_end) != PAGE_SIZE {
                        return Err(io::Error::new(kind, "invalid empty leaf free_end"));
                    }
                } else if self.free_end < self.free_start || usize::from(self.free_end) >= PAGE_SIZE {
                    return Err(io::Error::new(kind, "invalid leaf free_end"));
                }
                validate_reference(self.left, page_id, next_page_id, kind)?;
                validate_reference(self.right, page_id, next_page_id, kind)
            }
            PageType::Internal => {
                if self.cell_count == 0
                    || self.free_start
                        != expected_slot_end(PAGE_HEADER_LEN + INTERNAL_PREAMBLE_LEN, self.cell_count, kind)?
                    || self.free_end < self.free_start
                    || usize::from(self.free_end) >= PAGE_SIZE
                    || self.left != 0
                    || self.right != 0
                {
                    return Err(io::Error::new(kind, "invalid internal page header"));
                }
                Ok(())
            }
            PageType::Overflow | PageType::Free => {
                if self.cell_count != 0 || self.free_start != 0 || self.free_end != 0 || self.left != 0 {
                    return Err(io::Error::new(kind, "invalid chain page header"));
                }
                validate_reference(self.right, page_id, next_page_id, kind)
            }
        }
    }

    pub(crate) fn encode_into(&self, page: &mut [u8], page_id: u64, next_page_id: u64) -> io::Result<()> {
        exact_page(page)?;
        self.validate(page_id, next_page_id, io::ErrorKind::InvalidInput)?;
        write_at(page, 0, &[self.page_type as u8])?;
        write_at(page, 1, &[0])?;
        write_at(page, 2, &self.cell_count.to_le_bytes())?;
        write_at(page, 4, &self.free_start.to_le_bytes())?;
        write_at(page, 6, &self.free_end.to_le_bytes())?;
        write_at(page, 8, &self.left.to_le_bytes())?;
        write_at(page, 16, &self.right.to_le_bytes())?;
        write_at(page, 28, &[0; 4])?;
        write_page_checksum(page)
    }

    pub(crate) fn decode(page: &[u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        exact_page(page)?;
        verify_page_checksum(page)?;
        let page_type = PageType::try_from(read_u8(page, 0)?)?;
        if read_u8(page, 1)? != 0 {
            return Err(invalid_data("unknown page flags"));
        }
        require_zero(
            page.get(28..32).ok_or_else(|| invalid_data("missing page reserved bytes"))?,
            "page reserved bytes must be zero",
        )?;
        let header = Self {
            page_type,
            cell_count: read_u16(page, 2)?,
            free_start: read_u16(page, 4)?,
            free_end: read_u16(page, 6)?,
            left: read_u64(page, 8)?,
            right: read_u64(page, 16)?,
        };
        header.validate(page_id, next_page_id, io::ErrorKind::InvalidData)?;
        Ok(header)
    }
}

#[cfg(test)]
pub(crate) fn page_checksum(page: &[u8]) -> io::Result<u32> { checksum_with_zeroed_field(page, PAGE_CHECKSUM_OFFSET) }

pub(crate) fn write_page_checksum(page: &mut [u8]) -> io::Result<()> { write_checksum(page, PAGE_CHECKSUM_OFFSET) }

pub(crate) fn verify_page_checksum(page: &[u8]) -> io::Result<()> { verify_checksum(page, PAGE_CHECKSUM_OFFSET) }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Slot {
    pub(crate) offset: u16,
    pub(crate) length: u16,
}

impl Slot {
    pub(crate) fn encode(self) -> [u8; SLOT_LEN] {
        let [offset0, offset1] = self.offset.to_le_bytes();
        let [length0, length1] = self.length.to_le_bytes();
        [offset0, offset1, length0, length1]
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != SLOT_LEN {
            return Err(invalid_data("invalid slot length"));
        }
        Ok(Self { offset: read_u16(bytes, 0)?, length: read_u16(bytes, 2)? })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InternalPreamble {
    pub(crate) leftmost_child: u64,
}

impl InternalPreamble {
    pub(crate) fn encode_into(self, page: &mut [u8], page_id: u64, next_page_id: u64) -> io::Result<()> {
        exact_page(page)?;
        if self.leftmost_child == 0 {
            return Err(invalid_input("internal leftmost child must be nonzero"));
        }
        validate_reference(self.leftmost_child, page_id, next_page_id, io::ErrorKind::InvalidInput)?;
        write_at(page, PAGE_HEADER_LEN, &self.leftmost_child.to_le_bytes())
    }

    pub(crate) fn decode(page: &[u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        exact_page(page)?;
        let leftmost_child = read_u64(page, PAGE_HEADER_LEN)?;
        if leftmost_child == 0 {
            return Err(invalid_data("internal leftmost child must be nonzero"));
        }
        validate_reference(leftmost_child, page_id, next_page_id, io::ErrorKind::InvalidData)?;
        Ok(Self { leftmost_child })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InternalCellPrefix {
    pub(crate) key_length: u16,
    pub(crate) right_child: u64,
}

impl InternalCellPrefix {
    fn validate(&self, page_id: u64, next_page_id: u64, kind: io::ErrorKind) -> io::Result<()> {
        if self.key_length == 0 || usize::from(self.key_length) > MAX_ENCODED_KEY_LEN || self.right_child == 0 {
            return Err(io::Error::new(kind, "invalid internal cell prefix"));
        }
        validate_reference(self.right_child, page_id, next_page_id, kind)
    }

    pub(crate) fn encode(&self, page_id: u64, next_page_id: u64) -> io::Result<[u8; INTERNAL_CELL_PREFIX_LEN]> {
        self.validate(page_id, next_page_id, io::ErrorKind::InvalidInput)?;
        let mut bytes = [0u8; INTERNAL_CELL_PREFIX_LEN];
        write_at(&mut bytes, 0, &self.key_length.to_le_bytes())?;
        write_at(&mut bytes, 4, &self.right_child.to_le_bytes())?;
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        let prefix =
            bytes.get(..INTERNAL_CELL_PREFIX_LEN).ok_or_else(|| invalid_data("truncated internal cell prefix"))?;
        require_zero(
            prefix.get(2..4).ok_or_else(|| invalid_data("missing internal cell reserved bytes"))?,
            "internal cell reserved bytes must be zero",
        )?;
        let decoded = Self { key_length: read_u16(prefix, 0)?, right_child: read_u64(prefix, 4)? };
        decoded.validate(page_id, next_page_id, io::ErrorKind::InvalidData)?;
        Ok(decoded)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ValueKind {
    Inline = 0,
    Overflow = 1,
    Tombstone = 2,
}

impl TryFrom<u8> for ValueKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Inline),
            1 => Ok(Self::Overflow),
            2 => Ok(Self::Tombstone),
            _ => Err(invalid_data("unknown value kind")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Compression {
    None = 0,
    Lz4 = 1,
}

impl TryFrom<u8> for Compression {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Lz4),
            _ => Err(invalid_data("unknown compression mode")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LeafCellPrefix {
    pub(crate) key_length: u16,
    pub(crate) value_kind: ValueKind,
    pub(crate) compression: Compression,
    pub(crate) logical_length: u32,
    pub(crate) stored_length: u32,
    pub(crate) overflow_head: u64,
    pub(crate) stored_crc32: u32,
}

impl LeafCellPrefix {
    fn validate(&self, page_id: u64, next_page_id: u64, kind: io::ErrorKind) -> io::Result<()> {
        if self.key_length == 0 || usize::from(self.key_length) > MAX_ENCODED_KEY_LEN {
            return Err(io::Error::new(kind, "invalid leaf key length"));
        }
        let stored_on_page = match self.value_kind {
            ValueKind::Inline => usize::try_from(self.stored_length)
                .map_err(|_| io::Error::new(kind, "inline stored length exceeds usize"))?,
            ValueKind::Overflow | ValueKind::Tombstone => 0,
        };
        let footprint = SLOT_LEN
            .checked_add(LEAF_CELL_PREFIX_LEN)
            .and_then(|size| size.checked_add(usize::from(self.key_length)))
            .and_then(|size| size.checked_add(stored_on_page))
            .ok_or_else(|| io::Error::new(kind, "leaf cell footprint overflow"))?;
        if footprint > MAX_CELL_FOOTPRINT {
            return Err(io::Error::new(kind, "leaf cell footprint exceeds limit"));
        }
        match self.value_kind {
            ValueKind::Inline => {
                if self.logical_length == 0
                    || self.stored_length == 0
                    || stored_on_page > MAX_INLINE_STORED_VALUE
                    || self.overflow_head != 0
                    || (self.compression == Compression::None && self.logical_length != self.stored_length)
                {
                    return Err(io::Error::new(kind, "invalid inline value descriptor"));
                }
            }
            ValueKind::Overflow => {
                if self.logical_length == 0 || self.stored_length == 0 || self.overflow_head == 0 {
                    return Err(io::Error::new(kind, "invalid overflow value descriptor"));
                }
                if self.compression == Compression::None && self.logical_length != self.stored_length {
                    return Err(io::Error::new(kind, "invalid uncompressed overflow lengths"));
                }
                validate_reference(self.overflow_head, page_id, next_page_id, kind)?;
            }
            ValueKind::Tombstone => {
                if self.compression != Compression::None
                    || self.logical_length != 0
                    || self.stored_length != 0
                    || self.overflow_head != 0
                    || self.stored_crc32 != 0
                {
                    return Err(io::Error::new(kind, "invalid tombstone descriptor"));
                }
            }
        }
        if self.compression == Compression::Lz4 {
            let threshold = u64::from(self.logical_length) * COMPRESSION_PERCENT as u64 / 100;
            if self.logical_length < 64 || self.stored_length < 4 || u64::from(self.stored_length) >= threshold {
                return Err(io::Error::new(kind, "invalid compressed value lengths"));
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self, page_id: u64, next_page_id: u64) -> io::Result<[u8; LEAF_CELL_PREFIX_LEN]> {
        self.validate(page_id, next_page_id, io::ErrorKind::InvalidInput)?;
        let mut bytes = [0u8; LEAF_CELL_PREFIX_LEN];
        write_at(&mut bytes, 0, &self.key_length.to_le_bytes())?;
        write_at(&mut bytes, 2, &[self.value_kind as u8])?;
        write_at(&mut bytes, 3, &[self.compression as u8])?;
        write_at(&mut bytes, 4, &self.logical_length.to_le_bytes())?;
        write_at(&mut bytes, 8, &self.stored_length.to_le_bytes())?;
        write_at(&mut bytes, 12, &self.overflow_head.to_le_bytes())?;
        write_at(&mut bytes, 20, &self.stored_crc32.to_le_bytes())?;
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        let prefix = bytes.get(..LEAF_CELL_PREFIX_LEN).ok_or_else(|| invalid_data("truncated leaf cell prefix"))?;
        let decoded = Self {
            key_length: read_u16(prefix, 0)?,
            value_kind: ValueKind::try_from(read_u8(prefix, 2)?)?,
            compression: Compression::try_from(read_u8(prefix, 3)?)?,
            logical_length: read_u32(prefix, 4)?,
            stored_length: read_u32(prefix, 8)?,
            overflow_head: read_u64(prefix, 12)?,
            stored_crc32: read_u32(prefix, 20)?,
        };
        decoded.validate(page_id, next_page_id, io::ErrorKind::InvalidData)?;
        Ok(decoded)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OverflowHeader {
    pub(crate) payload_length: u16,
}

impl OverflowHeader {
    pub(crate) fn encode_into(self, page: &mut [u8]) -> io::Result<()> {
        exact_page(page)?;
        let payload_length = usize::from(self.payload_length);
        if payload_length > OVERFLOW_PAYLOAD_LEN {
            return Err(invalid_input("overflow payload is too large"));
        }
        write_at(page, PAGE_HEADER_LEN, &self.payload_length.to_le_bytes())?;
        write_at(page, PAGE_HEADER_LEN + 2, &[0; 6])?;
        let tail_start = checked_end(40, payload_length)?;
        page.get_mut(tail_start..).ok_or_else(|| invalid_data("overflow payload exceeds page"))?.fill(0);
        Ok(())
    }

    pub(crate) fn decode(page: &[u8]) -> io::Result<Self> {
        exact_page(page)?;
        let payload_length = read_u16(page, PAGE_HEADER_LEN)?;
        if usize::from(payload_length) > OVERFLOW_PAYLOAD_LEN {
            return Err(invalid_data("overflow payload is too large"));
        }
        require_zero(
            page.get(34..40).ok_or_else(|| invalid_data("missing overflow reserved bytes"))?,
            "overflow reserved bytes must be zero",
        )?;
        let tail_start = checked_end(40, usize::from(payload_length))?;
        require_zero(
            page.get(tail_start..).ok_or_else(|| invalid_data("overflow payload exceeds page"))?,
            "overflow page tail must be zero",
        )?;
        Ok(Self { payload_length })
    }
}

pub(crate) fn encode_free_body(page: &mut [u8]) -> io::Result<()> {
    exact_page(page)?;
    page.get_mut(PAGE_HEADER_LEN..).ok_or_else(|| invalid_data("missing free page body"))?.fill(0);
    Ok(())
}

pub(crate) fn validate_free_body(page: &[u8]) -> io::Result<()> {
    exact_page(page)?;
    require_zero(
        page.get(PAGE_HEADER_LEN..).ok_or_else(|| invalid_data("missing free page body"))?,
        "free page body must be zero",
    )
}

pub(crate) fn stored_value_checksum(stored: &[u8]) -> u32 { crc32fast::hash(stored) }

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StoredValue<'a> {
    BorrowedRaw(&'a [u8]),
    Compressed(&'a [u8]),
}

impl StoredValue<'_> {
    pub(crate) const fn compression(&self) -> Compression {
        match self {
            Self::BorrowedRaw(_) => Compression::None,
            Self::Compressed(_) => Compression::Lz4,
        }
    }

    pub(crate) const fn as_slice(&self) -> &[u8] {
        match self {
            Self::BorrowedRaw(bytes) | Self::Compressed(bytes) => bytes,
        }
    }
}

fn compression_is_beneficial(raw_length: usize, stored_length: usize) -> io::Result<bool> {
    let threshold =
        raw_length.checked_mul(COMPRESSION_PERCENT).ok_or_else(|| invalid_input("compression threshold overflow"))?
            / 100;
    Ok(stored_length < threshold)
}

pub(crate) fn encode_value<'a>(raw: &'a [u8], scratch: &'a mut Vec<u8>) -> io::Result<StoredValue<'a>> {
    scratch.clear();
    if raw.len() < COMPRESSION_MIN_LENGTH {
        return Ok(StoredValue::BorrowedRaw(raw));
    }
    let raw_length = u32::try_from(raw.len()).map_err(|_| invalid_input("value exceeds u32 length"))?;
    let maximum = lz4_flex::block::get_maximum_output_size(raw.len())
        .checked_add(4)
        .ok_or_else(|| invalid_input("compressed value size overflow"))?;
    scratch.try_reserve(maximum).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
    scratch.resize(maximum, 0);
    write_at(scratch, 0, &raw_length.to_le_bytes())?;
    let output = scratch.get_mut(4..).ok_or_else(|| invalid_data("missing compression output"))?;
    let compressed_length = lz4_flex::block::compress_into(raw, output)
        .map_err(|err| io::Error::other(format!("LZ4 compression failed: {err}")))?;
    let stored_length = checked_end(4, compressed_length)?;
    scratch.truncate(stored_length);
    if compression_is_beneficial(raw.len(), scratch.len())? {
        Ok(StoredValue::Compressed(scratch.as_slice()))
    } else {
        Ok(StoredValue::BorrowedRaw(raw))
    }
}

pub(crate) fn decompress_value_into<'a>(
    stored: &[u8],
    logical_length: u32,
    maximum_length: usize,
    scratch: &'a mut Vec<u8>,
) -> io::Result<&'a [u8]> {
    let encoded_length = read_u32(stored, 0)?;
    if encoded_length != logical_length {
        return Err(invalid_data("LZ4 logical length mismatch"));
    }
    let logical_length = usize::try_from(logical_length).map_err(|_| invalid_data("logical length exceeds usize"))?;
    if logical_length > maximum_length {
        return Err(invalid_data("logical length exceeds allocation limit"));
    }
    let payload = stored.get(4..).ok_or_else(|| invalid_data("missing LZ4 payload"))?;
    scratch.clear();
    scratch.try_reserve(logical_length).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
    scratch.resize(logical_length, 0);
    let decoded_length = lz4_flex::block::decompress_into(payload, scratch.as_mut_slice())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 decompression failed: {err}")))?;
    if decoded_length != logical_length {
        return Err(invalid_data("decompressed value length mismatch"));
    }
    scratch.get(..decoded_length).ok_or_else(|| invalid_data("decompressed value exceeds scratch buffer"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LeafCellRef<'a> {
    pub(crate) key_bytes: &'a [u8],
    pub(crate) value: LeafValueRef<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeafValueRef<'a> {
    Inline { compression: Compression, logical_len: u32, stored: &'a [u8], crc32: u32 },
    Overflow { compression: Compression, logical_len: u32, stored_len: u32, head: u64, crc32: u32 },
    Tombstone,
}

impl<'a> LeafCellRef<'a> {
    pub(crate) fn decode(cell: &'a [u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        let prefix = LeafCellPrefix::decode(cell, page_id, next_page_id)?;
        let key_end = LEAF_CELL_PREFIX_LEN
            .checked_add(usize::from(prefix.key_length))
            .ok_or_else(|| invalid_data("leaf key range overflow"))?;
        let key_bytes = cell.get(LEAF_CELL_PREFIX_LEN..key_end).ok_or_else(|| invalid_data("truncated leaf key"))?;
        let value = match prefix.value_kind {
            ValueKind::Inline => {
                let cell_end = key_end
                    .checked_add(
                        usize::try_from(prefix.stored_length)
                            .map_err(|_| invalid_data("inline stored length exceeds usize"))?,
                    )
                    .ok_or_else(|| invalid_data("inline value range overflow"))?;
                if cell_end != cell.len() {
                    return Err(invalid_data("invalid inline leaf cell length"));
                }
                let stored = cell.get(key_end..cell_end).ok_or_else(|| invalid_data("truncated inline value"))?;
                if stored_value_checksum(stored) != prefix.stored_crc32 {
                    return Err(invalid_data("stored value checksum mismatch"));
                }
                LeafValueRef::Inline {
                    compression: prefix.compression,
                    logical_len: prefix.logical_length,
                    stored,
                    crc32: prefix.stored_crc32,
                }
            }
            ValueKind::Overflow => {
                if key_end != cell.len() {
                    return Err(invalid_data("invalid overflow leaf cell length"));
                }
                LeafValueRef::Overflow {
                    compression: prefix.compression,
                    logical_len: prefix.logical_length,
                    stored_len: prefix.stored_length,
                    head: prefix.overflow_head,
                    crc32: prefix.stored_crc32,
                }
            }
            ValueKind::Tombstone => {
                if key_end != cell.len() {
                    return Err(invalid_data("invalid tombstone leaf cell length"));
                }
                LeafValueRef::Tombstone
            }
        };
        Ok(Self { key_bytes, value })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InternalCellRef<'a> {
    pub(crate) key_bytes: &'a [u8],
    pub(crate) right_child: u64,
}

impl<'a> InternalCellRef<'a> {
    pub(crate) fn decode(cell: &'a [u8], page_id: u64, next_page_id: u64) -> io::Result<Self> {
        let prefix = InternalCellPrefix::decode(cell, page_id, next_page_id)?;
        let cell_end = INTERNAL_CELL_PREFIX_LEN
            .checked_add(usize::from(prefix.key_length))
            .ok_or_else(|| invalid_data("internal key range overflow"))?;
        if cell_end != cell.len() {
            return Err(invalid_data("invalid internal cell length"));
        }
        let key_bytes =
            cell.get(INTERNAL_CELL_PREFIX_LEN..cell_end).ok_or_else(|| invalid_data("truncated internal key"))?;
        Ok(Self { key_bytes, right_child: prefix.right_child })
    }
}

fn encoded_key_length(key: &[u8]) -> io::Result<u16> {
    if key.is_empty() || key.len() > MAX_ENCODED_KEY_LEN {
        return Err(invalid_input("invalid encoded key length"));
    }
    u16::try_from(key.len()).map_err(|_| invalid_input("encoded key length exceeds u16"))
}

fn write_cell(output: &mut Vec<u8>, prefix: &[u8], key: &[u8], stored: &[u8]) -> io::Result<()> {
    let length = prefix
        .len()
        .checked_add(key.len())
        .and_then(|value| value.checked_add(stored.len()))
        .ok_or_else(|| invalid_input("encoded cell length overflow"))?;
    output.clear();
    output.try_reserve(length).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
    output.extend_from_slice(prefix);
    output.extend_from_slice(key);
    output.extend_from_slice(stored);
    Ok(())
}

pub(crate) fn encode_inline_leaf_cell(
    key: &[u8],
    logical_len: u32,
    compression: Compression,
    stored: &[u8],
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let prefix = LeafCellPrefix {
        key_length: encoded_key_length(key)?,
        value_kind: ValueKind::Inline,
        compression,
        logical_length: logical_len,
        stored_length: u32::try_from(stored.len()).map_err(|_| invalid_input("stored value exceeds u32"))?,
        overflow_head: 0,
        stored_crc32: stored_value_checksum(stored),
    }
    .encode(1, 2)?;
    write_cell(output, &prefix, key, stored)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_overflow_leaf_cell(
    key: &[u8],
    logical_len: u32,
    compression: Compression,
    stored_len: u32,
    head: u64,
    stored_crc32: u32,
    page_id: u64,
    next_page_id: u64,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let prefix = LeafCellPrefix {
        key_length: encoded_key_length(key)?,
        value_kind: ValueKind::Overflow,
        compression,
        logical_length: logical_len,
        stored_length: stored_len,
        overflow_head: head,
        stored_crc32,
    }
    .encode(page_id, next_page_id)?;
    write_cell(output, &prefix, key, &[])
}

pub(crate) fn encode_tombstone_leaf_cell(key: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
    let prefix = LeafCellPrefix {
        key_length: encoded_key_length(key)?,
        value_kind: ValueKind::Tombstone,
        compression: Compression::None,
        logical_length: 0,
        stored_length: 0,
        overflow_head: 0,
        stored_crc32: 0,
    }
    .encode(1, 2)?;
    write_cell(output, &prefix, key, &[])
}

pub(crate) fn encode_internal_cell(
    key: &[u8],
    right_child: u64,
    page_id: u64,
    next_page_id: u64,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let prefix =
        InternalCellPrefix { key_length: encoded_key_length(key)?, right_child }.encode(page_id, next_page_id)?;
    write_cell(output, &prefix, key, &[])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Locator {
    pub leaf_page_id: u64,
    pub slot_index: u16,
    pub serialized_key_crc32: u32,
}

impl Locator {
    pub(crate) fn for_key(leaf_page_id: u64, slot_index: u16, serialized_key: &[u8]) -> io::Result<Self> {
        if leaf_page_id == 0 {
            return Err(invalid_input("locator leaf page id must be nonzero"));
        }
        Ok(Self { leaf_page_id, slot_index, serialized_key_crc32: crc32fast::hash(serialized_key) })
    }

    pub(crate) fn encode(self) -> [u8; 16] {
        let mut encoded = [0; 16];
        encoded[0..8].copy_from_slice(&self.leaf_page_id.to_le_bytes());
        encoded[8..10].copy_from_slice(&self.slot_index.to_le_bytes());
        encoded[12..16].copy_from_slice(&self.serialized_key_crc32.to_le_bytes());
        encoded
    }

    pub(crate) fn decode(encoded: &[u8]) -> io::Result<Self> {
        if encoded.len() != 16 {
            return Err(invalid_data("locator must be exactly 16 bytes"));
        }
        require_zero(
            encoded.get(10..12).ok_or_else(|| invalid_data("missing locator reserved bytes"))?,
            "locator reserved bytes must be zero",
        )?;
        let locator = Self {
            leaf_page_id: read_u64(encoded, 0)?,
            slot_index: read_u16(encoded, 8)?,
            serialized_key_crc32: read_u32(encoded, 12)?,
        };
        if locator.leaf_page_id == 0 {
            return Err(invalid_data("locator leaf page id must be nonzero"));
        }
        Ok(locator)
    }
}

pub(crate) fn decompress_value_in_place(
    scratch: &mut Vec<u8>,
    logical_length: u32,
    maximum_length: usize,
) -> io::Result<()> {
    let encoded_length = read_u32(scratch, 0)?;
    if encoded_length != logical_length {
        return Err(invalid_data("LZ4 logical length mismatch"));
    }
    let logical_length = usize::try_from(logical_length).map_err(|_| invalid_data("logical length exceeds usize"))?;
    if logical_length > maximum_length {
        return Err(invalid_data("logical length exceeds allocation limit"));
    }
    let stored_length = scratch.len();
    let total_length = logical_length
        .checked_add(stored_length)
        .ok_or_else(|| invalid_data("in-place decompression size overflow"))?;
    scratch
        .try_reserve(total_length.saturating_sub(stored_length))
        .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
    scratch.resize(total_length, 0);
    scratch.copy_within(0..stored_length, logical_length);
    let (output, encoded) = scratch.split_at_mut(logical_length);
    let payload = encoded.get(4..stored_length).ok_or_else(|| invalid_data("missing LZ4 payload"))?;
    let decoded_length = lz4_flex::block::decompress_into(payload, output)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 decompression failed: {err}")))?;
    if decoded_length != logical_length {
        return Err(invalid_data("decompressed value length mismatch"));
    }
    scratch.truncate(logical_length);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3::BPlusTreeMetadata;
    use std::io;

    const PAGE_ID: u64 = 7;
    const NEXT_PAGE_ID: u64 = 19;

    fn invalid_data<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidData => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidData, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidData")),
        }
    }

    fn invalid_input<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidInput, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidInput")),
        }
    }

    fn u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
        let value = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing u32"))?
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(u32::from_le_bytes(value))
    }

    fn write_crc(page: &mut [u8], checksum_offset: usize) -> io::Result<()> {
        page.get_mut(checksum_offset..checksum_offset + 4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing checksum"))?
            .fill(0);
        let checksum = crc32fast::hash(page);
        page.get_mut(checksum_offset..checksum_offset + 4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing checksum"))?
            .copy_from_slice(&checksum.to_le_bytes());
        Ok(())
    }

    #[test]
    fn write_checksum_rejects_short_buffer_without_mutating_it() -> io::Result<()> {
        let mut short = [0x5a; DATABASE_CHECKSUM_OFFSET + 4];
        let original = short;
        invalid_data(write_checksum(&mut short, DATABASE_CHECKSUM_OFFSET))?;
        assert_eq!(short, original);
        Ok(())
    }

    fn golden_database_header() -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(b"BTRE");
        page[4..8].copy_from_slice(&3u32.to_le_bytes());
        page[8..12].copy_from_slice(&4096u32.to_le_bytes());
        page[16..24].copy_from_slice(&7u64.to_le_bytes());
        page[24..32].copy_from_slice(&19u64.to_le_bytes());
        page[32..40].copy_from_slice(&3u64.to_le_bytes());
        page[40..48].copy_from_slice(&5u64.to_le_bytes());
        page[48..64].fill(0x11);
        page[64..68].copy_from_slice(&5u32.to_le_bytes());
        page[72..76].copy_from_slice(&0xaa07_e109u32.to_le_bytes());
        page[76..81].copy_from_slice(&[1, 42, 0, 0, 0]);
        page
    }

    fn page_fixture(
        page_type: u8,
        cell_count: u16,
        free_start: u16,
        free_end: u16,
        left: u64,
        right: u64,
        checksum: u32,
    ) -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        page[0] = page_type;
        page[2..4].copy_from_slice(&cell_count.to_le_bytes());
        page[4..6].copy_from_slice(&free_start.to_le_bytes());
        page[6..8].copy_from_slice(&free_end.to_le_bytes());
        page[8..16].copy_from_slice(&left.to_le_bytes());
        page[16..24].copy_from_slice(&right.to_le_bytes());
        page[24..28].copy_from_slice(&checksum.to_le_bytes());
        page
    }

    #[test]
    fn format_constants_are_frozen() {
        assert_eq!(PAGE_SIZE, 4096);
        assert_eq!(STORAGE_VERSION_V1, 1);
        assert_eq!(STORAGE_VERSION_V2, 2);
        assert_eq!(STORAGE_VERSION_V3, 3);
        assert_eq!(MAX_ENCODED_KEY_LEN, 2004);
        assert_eq!(MAX_CELL_FOOTPRINT, 2032);
        assert_eq!(MAX_INLINE_STORED_VALUE, 512);
        assert_eq!(OVERFLOW_PAYLOAD_LEN, 4056);
        assert_eq!(PAGE_HEADER_LEN, 32);
        assert_eq!(SLOT_LEN, 4);
        assert_eq!(INTERNAL_PREAMBLE_LEN, 8);
        assert_eq!(INTERNAL_CELL_PREFIX_LEN, 12);
        assert_eq!(LEAF_CELL_PREFIX_LEN, 24);
        assert_eq!(OVERFLOW_HEADER_LEN, 8);
        assert_eq!(MAGIC, b"BTRE");
    }

    #[test]
    fn database_header_golden_bytes_round_trip() -> io::Result<()> {
        let header = DatabaseHeader {
            root_page_id: 7,
            next_page_id: 19,
            free_page_head: 3,
            generation: 5,
            database_id: [0x11; 16],
            metadata: BPlusTreeMetadata::TargetIdMapping(42),
        };

        let encoded = header.encode()?;
        assert_eq!(encoded, golden_database_header());
        assert_eq!(&encoded[0..4], b"BTRE");
        assert_eq!(u32_at(&encoded, 4)?, STORAGE_VERSION_V3);
        assert_eq!(u32_at(&encoded, 8)?, PAGE_SIZE_U32);
        assert_eq!(u32_at(&encoded, 72)?, 0xaa07_e109);
        assert_eq!(DatabaseHeader::decode(&encoded)?, header);
        Ok(())
    }

    #[test]
    fn database_header_empty_metadata_round_trip() -> io::Result<()> {
        let header = DatabaseHeader {
            root_page_id: 1,
            next_page_id: 2,
            free_page_head: 0,
            generation: 1,
            database_id: [0x22; 16],
            metadata: BPlusTreeMetadata::Empty,
        };
        let encoded = header.encode()?;
        assert_eq!(u32_at(&encoded, 64)?, 0);
        assert!(encoded[76..].iter().all(|byte| *byte == 0));
        assert_eq!(DatabaseHeader::decode(&encoded)?, header);
        Ok(())
    }

    #[test]
    fn database_header_rejects_corrupt_fields() -> io::Result<()> {
        let mut page = golden_database_header();
        page[0] = b'X';
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[4..8].copy_from_slice(&2u32.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[8..12].copy_from_slice(&8192u32.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[12] = 1;
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[68] = 1;
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[76] = 2;
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[64..68].copy_from_slice(&4u32.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[81] = 1;
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[40] ^= 1;
        invalid_data(DatabaseHeader::decode(&page))?;
        Ok(())
    }

    #[test]
    fn database_header_rejects_invalid_structural_state() -> io::Result<()> {
        for range in [16..24, 40..48, 48..64] {
            let mut page = golden_database_header();
            page[range].fill(0);
            write_crc(&mut page, 72)?;
            invalid_data(DatabaseHeader::decode(&page))?;
        }

        let mut page = golden_database_header();
        page[24..32].copy_from_slice(&7u64.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let mut page = golden_database_header();
        page[32..40].copy_from_slice(&19u64.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;

        let header = DatabaseHeader {
            root_page_id: 7,
            next_page_id: 19,
            free_page_head: 7,
            generation: 5,
            database_id: [0x11; 16],
            metadata: BPlusTreeMetadata::Empty,
        };
        invalid_data(header.encode())?;

        let mut page = golden_database_header();
        page[32..40].copy_from_slice(&7u64.to_le_bytes());
        write_crc(&mut page, 72)?;
        invalid_data(DatabaseHeader::decode(&page))?;
        Ok(())
    }

    #[test]
    fn leaf_page_header_golden_bytes_round_trip() -> io::Result<()> {
        let expected = page_fixture(1, 2, 40, 4000, 6, 8, 0x88c9_dd55);
        let header =
            PageHeader { page_type: PageType::Leaf, cell_count: 2, free_start: 40, free_end: 4000, left: 6, right: 8 };
        let mut encoded = [0u8; PAGE_SIZE];
        header.encode_into(&mut encoded, PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(encoded, expected);
        assert_eq!(page_checksum(&encoded)?, 0x88c9_dd55);
        assert_eq!(PageHeader::decode(&encoded, PAGE_ID, NEXT_PAGE_ID)?, header);
        Ok(())
    }

    #[test]
    fn internal_page_header_and_preamble_golden_bytes_round_trip() -> io::Result<()> {
        let mut expected = page_fixture(2, 1, 44, 4000, 0, 0, 0x82b3_f78a);
        expected[32..40].copy_from_slice(&3u64.to_le_bytes());
        let header = PageHeader {
            page_type: PageType::Internal,
            cell_count: 1,
            free_start: 44,
            free_end: 4000,
            left: 0,
            right: 0,
        };
        let preamble = InternalPreamble { leftmost_child: 3 };
        let mut encoded = [0u8; PAGE_SIZE];
        preamble.encode_into(&mut encoded, PAGE_ID, NEXT_PAGE_ID)?;
        header.encode_into(&mut encoded, PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(encoded, expected);
        assert_eq!(PageHeader::decode(&encoded, PAGE_ID, NEXT_PAGE_ID)?, header);
        assert_eq!(InternalPreamble::decode(&encoded, PAGE_ID, NEXT_PAGE_ID)?, preamble);
        Ok(())
    }

    #[test]
    fn overflow_page_header_golden_bytes_round_trip() -> io::Result<()> {
        let mut expected = page_fixture(3, 0, 0, 0, 0, 8, 0x8731_d58d);
        expected[32..34].copy_from_slice(&3u16.to_le_bytes());
        expected[40..43].copy_from_slice(b"abc");
        let header =
            PageHeader { page_type: PageType::Overflow, cell_count: 0, free_start: 0, free_end: 0, left: 0, right: 8 };
        let overflow = OverflowHeader { payload_length: 3 };
        let mut encoded = [0u8; PAGE_SIZE];
        encoded[40..43].copy_from_slice(b"abc");
        overflow.encode_into(&mut encoded)?;
        header.encode_into(&mut encoded, PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(encoded, expected);
        assert_eq!(PageHeader::decode(&encoded, PAGE_ID, NEXT_PAGE_ID)?, header);
        assert_eq!(OverflowHeader::decode(&encoded)?, overflow);
        Ok(())
    }

    #[test]
    fn free_page_header_golden_bytes_round_trip() -> io::Result<()> {
        let expected = page_fixture(4, 0, 0, 0, 0, 8, 0x2864_f2d3);
        let header =
            PageHeader { page_type: PageType::Free, cell_count: 0, free_start: 0, free_end: 0, left: 0, right: 8 };
        let mut encoded = [0u8; PAGE_SIZE];
        encode_free_body(&mut encoded)?;
        header.encode_into(&mut encoded, PAGE_ID, NEXT_PAGE_ID)?;
        assert_eq!(encoded, expected);
        assert_eq!(PageHeader::decode(&encoded, PAGE_ID, NEXT_PAGE_ID)?, header);
        validate_free_body(&encoded)?;
        Ok(())
    }

    #[test]
    fn page_header_rejects_unknown_type_flags_reserved_checksum_and_references() -> io::Result<()> {
        let expected = page_fixture(1, 2, 40, 4000, 6, 8, 0x88c9_dd55);

        for (offset, value) in [(0, 9), (1, 1), (28, 1)] {
            let mut page = expected;
            page[offset] = value;
            write_crc(&mut page, 24)?;
            invalid_data(PageHeader::decode(&page, PAGE_ID, NEXT_PAGE_ID))?;
        }

        let mut page = expected;
        page[16..24].copy_from_slice(&PAGE_ID.to_le_bytes());
        write_crc(&mut page, 24)?;
        invalid_data(PageHeader::decode(&page, PAGE_ID, NEXT_PAGE_ID))?;

        let mut page = expected;
        page[2] ^= 1;
        invalid_data(PageHeader::decode(&page, PAGE_ID, NEXT_PAGE_ID))?;
        Ok(())
    }

    #[test]
    fn overflow_and_free_bodies_reject_reserved_or_nonzero_tail_bytes() -> io::Result<()> {
        let mut overflow = [0u8; PAGE_SIZE];
        overflow[32..34].copy_from_slice(&1u16.to_le_bytes());
        overflow[40] = 7;
        overflow[34] = 1;
        invalid_data(OverflowHeader::decode(&overflow))?;

        let mut overflow = [0u8; PAGE_SIZE];
        overflow[32..34].copy_from_slice(&1u16.to_le_bytes());
        overflow[40] = 7;
        overflow[41] = 1;
        invalid_data(OverflowHeader::decode(&overflow))?;

        let mut free = [0u8; PAGE_SIZE];
        free[32] = 1;
        invalid_data(validate_free_body(&free))?;
        Ok(())
    }

    #[test]
    fn overflow_payload_accepts_4056_and_rejects_4057() -> io::Result<()> {
        let mut page = [0u8; PAGE_SIZE];
        OverflowHeader { payload_length: 4056 }.encode_into(&mut page)?;
        assert_eq!(OverflowHeader::decode(&page)?.payload_length, 4056);

        invalid_input(OverflowHeader { payload_length: 4057 }.encode_into(&mut page))?;
        page[32..34].copy_from_slice(&4057u16.to_le_bytes());
        invalid_data(OverflowHeader::decode(&page))?;
        Ok(())
    }

    #[test]
    fn slot_and_cell_prefixes_have_golden_bytes() -> io::Result<()> {
        let slot = Slot { offset: 0x1234, length: 0x5678 };
        let slot_bytes = [0x34, 0x12, 0x78, 0x56];
        assert_eq!(slot.encode(), slot_bytes);
        assert_eq!(Slot::decode(&slot_bytes)?, slot);

        let internal = InternalCellPrefix { key_length: 3, right_child: 9 };
        let internal_bytes = [3, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(internal.encode(PAGE_ID, NEXT_PAGE_ID)?, internal_bytes);
        assert_eq!(InternalCellPrefix::decode(&internal_bytes, PAGE_ID, NEXT_PAGE_ID)?, internal);

        let leaf = LeafCellPrefix {
            key_length: 3,
            value_kind: ValueKind::Inline,
            compression: Compression::Lz4,
            logical_length: 128,
            stored_length: 16,
            overflow_head: 0,
            stored_crc32: 0x1122_3344,
        };
        let leaf_bytes = [3, 0, 0, 1, 128, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44, 0x33, 0x22, 0x11];
        assert_eq!(leaf.encode(PAGE_ID, NEXT_PAGE_ID)?, leaf_bytes);
        assert_eq!(LeafCellPrefix::decode(&leaf_bytes, PAGE_ID, NEXT_PAGE_ID)?, leaf);
        assert_eq!(stored_value_checksum(b"abc"), 0x3524_41c2);
        Ok(())
    }

    #[test]
    fn cell_prefixes_reject_reserved_unknown_modes_and_invalid_lengths() -> io::Result<()> {
        let mut internal = [3, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0];
        internal[2] = 1;
        invalid_data(InternalCellPrefix::decode(&internal, PAGE_ID, NEXT_PAGE_ID))?;

        let mut leaf = [3, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
        leaf[2] = 9;
        invalid_data(LeafCellPrefix::decode(&leaf, PAGE_ID, NEXT_PAGE_ID))?;
        leaf[2] = 0;
        leaf[3] = 9;
        invalid_data(LeafCellPrefix::decode(&leaf, PAGE_ID, NEXT_PAGE_ID))?;
        leaf[3] = 0;
        leaf[8..12].copy_from_slice(&257u32.to_le_bytes());
        invalid_data(LeafCellPrefix::decode(&leaf, PAGE_ID, NEXT_PAGE_ID))?;
        Ok(())
    }

    #[test]
    fn leaf_key_and_complete_cell_footprint_boundaries() -> io::Result<()> {
        let overflow_at_key_limit = LeafCellPrefix {
            key_length: 2004,
            value_kind: ValueKind::Overflow,
            compression: Compression::None,
            logical_length: 4096,
            stored_length: 4096,
            overflow_head: 9,
            stored_crc32: 0x1234_5678,
        };
        overflow_at_key_limit.encode(PAGE_ID, NEXT_PAGE_ID)?;

        let mut overflow_above_key_limit = overflow_at_key_limit;
        overflow_above_key_limit.key_length = 2005;
        invalid_input(overflow_above_key_limit.encode(PAGE_ID, NEXT_PAGE_ID))?;

        let inline_2032 = LeafCellPrefix {
            key_length: 1748,
            value_kind: ValueKind::Inline,
            compression: Compression::None,
            logical_length: 256,
            stored_length: 256,
            overflow_head: 0,
            stored_crc32: 0x1234_5678,
        };
        inline_2032.encode(PAGE_ID, NEXT_PAGE_ID)?;

        let mut inline_2033 = inline_2032;
        inline_2033.key_length = 1749;
        invalid_input(inline_2033.encode(PAGE_ID, NEXT_PAGE_ID))?;
        Ok(())
    }

    #[test]
    fn known_size_prepended_lz4_block_decodes_to_golden_raw_bytes() -> io::Result<()> {
        let block = [6, 0, 0, 0, 0x60, b'g', b'o', b'l', b'd', b'e', b'n'];
        let mut scratch = Vec::new();
        assert_eq!(decompress_value_into(&block, 6, 6, &mut scratch)?, b"golden");
        Ok(())
    }

    #[test]
    fn decompression_reuses_caller_scratch() -> io::Result<()> {
        let block = [6, 0, 0, 0, 0x60, b'g', b'o', b'l', b'd', b'e', b'n'];
        let mut scratch = Vec::with_capacity(64);
        let initial_capacity = scratch.capacity();
        let initial_pointer = scratch.as_ptr();

        assert_eq!(decompress_value_into(&block, 6, 64, &mut scratch)?, b"golden");
        assert_eq!(scratch.capacity(), initial_capacity);
        assert_eq!(scratch.as_ptr(), initial_pointer);

        assert_eq!(decompress_value_into(&block, 6, 64, &mut scratch)?, b"golden");
        assert_eq!(scratch.capacity(), initial_capacity);
        assert_eq!(scratch.as_ptr(), initial_pointer);
        Ok(())
    }

    #[test]
    fn compression_policy_obeys_size_and_ratio_boundaries() -> io::Result<()> {
        let mut scratch = Vec::new();
        let short = [0u8; 63];
        let stored = encode_value(&short, &mut scratch)?;
        assert_eq!(stored.compression(), Compression::None);
        assert_eq!(stored.as_slice(), short);

        let incompressible = (0u8..64).collect::<Vec<_>>();
        {
            let stored = encode_value(&incompressible, &mut scratch)?;
            assert_eq!(stored.compression(), Compression::None);
            assert_eq!(stored.as_slice(), incompressible);
        }
        assert!(scratch.len() >= incompressible.len() * 85 / 100);

        let compressible = [0u8; 128];
        let expected = lz4_flex::compress_prepend_size(&compressible);
        let stored = encode_value(&compressible, &mut scratch)?;
        assert_eq!(stored.compression(), Compression::Lz4);
        assert!(stored.as_slice().len() < compressible.len() * 85 / 100);
        assert_eq!(stored.as_slice(), expected);
        let mut decompression_scratch = Vec::new();
        assert_eq!(decompress_value_into(stored.as_slice(), 128, 128, &mut decompression_scratch)?, compressible);
        Ok(())
    }

    #[test]
    fn lz4_ratio_requires_strictly_less_than_85_percent() -> io::Result<()> {
        assert!(compression_is_beneficial(100, 84)?);
        assert!(!compression_is_beneficial(100, 85)?);
        assert!(!compression_is_beneficial(100, 86)?);
        Ok(())
    }

    #[test]
    fn decompression_rejects_corrupt_sizes_before_allocation() -> io::Result<()> {
        let mut scratch = Vec::new();
        let corrupt = [0xff, 0xff, 0xff, 0xff, 0x00];
        invalid_data(decompress_value_into(&corrupt, u32::MAX, 1024, &mut scratch))?;
        assert_eq!(scratch.capacity(), 0);

        let block = [6, 0, 0, 0, 0x60, b'g', b'o', b'l', b'd', b'e', b'n'];
        invalid_data(decompress_value_into(&block, 7, 7, &mut scratch))?;
        assert_eq!(scratch.capacity(), 0);
        Ok(())
    }
}
