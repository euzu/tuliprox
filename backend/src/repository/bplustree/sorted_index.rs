pub(crate) mod v4 {
    use crate::{
        repository::bplustree::v3::{BPlusTreeQuery, Locator},
        utils::{binary_deserialize, binary_serialize},
    };
    use serde::{Deserialize, Serialize};
    use std::{
        fs::{File, OpenOptions},
        io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
        marker::PhantomData,
        num::TryFromIntError,
        path::Path,
    };

    const MAGIC: &[u8; 4] = b"SIDX";
    const VERSION: u32 = 4;
    const TREE_VERSION: u32 = 3;
    const HEADER_LEN: usize = 64;
    const HEADER_LEN_U32: u32 = 64;
    const HEADER_CRC_OFFSET: usize = 56;
    const ENTRY_PREFIX_LEN: usize = 8;
    const ENTRY_PREFIX_LEN_U64: u64 = 8;
    const ENTRY_FIXED_BODY_LEN: usize = 24;

    fn invalid_data(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }

    fn invalid_integer(error: TryFromIntError) -> io::Error { invalid_data(error.to_string()) }

    fn header(database_id: [u8; 16], generation: u64, count: u64) -> [u8; HEADER_LEN] {
        let mut bytes = [0; HEADER_LEN];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&TREE_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&HEADER_LEN_U32.to_le_bytes());
        bytes[16..32].copy_from_slice(&database_id);
        bytes[32..40].copy_from_slice(&generation.to_le_bytes());
        bytes[40..48].copy_from_slice(&count.to_le_bytes());
        let checksum = crc32fast::hash(&bytes);
        bytes[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
        bytes
            .get(offset..offset + 4)
            .and_then(|value| value.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| invalid_data("sorted-index u32 field is truncated"))
    }

    fn read_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
        bytes
            .get(offset..offset + 8)
            .and_then(|value| value.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or_else(|| invalid_data("sorted-index u64 field is truncated"))
    }

    fn decode_header(
        mut bytes: [u8; HEADER_LEN],
        expected_database_id: [u8; 16],
        expected_generation: u64,
    ) -> io::Result<u64> {
        if bytes[0..4] != *MAGIC
            || read_u32(&bytes, 4)? != VERSION
            || read_u32(&bytes, 8)? != TREE_VERSION
            || read_u32(&bytes, 12)? != HEADER_LEN_U32
        {
            return Err(invalid_data("unsupported sorted-index header"));
        }
        if bytes[48..56].iter().chain(&bytes[60..64]).any(|byte| *byte != 0) {
            return Err(invalid_data("sorted-index reserved header bytes must be zero"));
        }
        let expected_checksum = read_u32(&bytes, HEADER_CRC_OFFSET)?;
        bytes[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].fill(0);
        if crc32fast::hash(&bytes) != expected_checksum {
            return Err(invalid_data("sorted-index header checksum mismatch"));
        }
        if bytes[16..32] != expected_database_id || read_u64(&bytes, 32)? != expected_generation {
            return Err(invalid_data("sorted-index tree identity or generation mismatch"));
        }
        read_u64(&bytes, 40)
    }

    pub(crate) struct Writer<SortKey, K> {
        output: BufWriter<File>,
        database_id: [u8; 16],
        generation: u64,
        count: u64,
        body: Vec<u8>,
        _marker: PhantomData<(SortKey, K)>,
    }

    impl<SortKey, K> Writer<SortKey, K>
    where
        SortKey: Serialize,
        K: Serialize,
    {
        pub(crate) fn new(path: &Path, database_id: [u8; 16], generation: u64) -> io::Result<Self> {
            let file = OpenOptions::new().write(true).create_new(true).open(path)?;
            let mut output = BufWriter::new(file);
            output.write_all(&header(database_id, generation, 0))?;
            Ok(Self { output, database_id, generation, count: 0, body: Vec::new(), _marker: PhantomData })
        }

        pub(crate) fn push(&mut self, sort_key: &SortKey, primary_key: &K, locator: Locator) -> io::Result<()> {
            let sort_key = binary_serialize(sort_key)?;
            let primary_key = binary_serialize(primary_key)?;
            let sort_key_len = u32::try_from(sort_key.len()).map_err(invalid_integer)?;
            let primary_key_len = u32::try_from(primary_key.len()).map_err(invalid_integer)?;
            self.body.clear();
            self.body.extend_from_slice(&sort_key_len.to_le_bytes());
            self.body.extend_from_slice(&primary_key_len.to_le_bytes());
            self.body.extend_from_slice(&locator.encode());
            self.body.extend_from_slice(&sort_key);
            self.body.extend_from_slice(&primary_key);
            let body_len = u32::try_from(self.body.len()).map_err(invalid_integer)?;
            self.output.write_all(&body_len.to_le_bytes())?;
            self.output.write_all(&crc32fast::hash(&self.body).to_le_bytes())?;
            self.output.write_all(&self.body)?;
            self.count = self.count.checked_add(1).ok_or_else(|| invalid_data("sorted-index count overflow"))?;
            Ok(())
        }

        pub(crate) fn finish(mut self) -> io::Result<u64> {
            self.output.flush()?;
            let mut file = self.output.into_inner().map_err(std::io::IntoInnerError::into_error)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&header(self.database_id, self.generation, self.count))?;
            file.sync_all()?;
            Ok(self.count)
        }
    }

    #[cfg(test)]
    pub(crate) struct Entry<SortKey, K> {
        pub(crate) sort_key: SortKey,
        pub(crate) primary_key: K,
        pub(crate) locator: Locator,
    }

    struct BorrowedEntry<'a, SortKey, K> {
        sort_key: SortKey,
        primary_key: K,
        serialized_primary_key: &'a [u8],
        locator: Locator,
    }

    pub(crate) struct Reader<SortKey, K> {
        input: BufReader<File>,
        remaining_entries: u64,
        remaining_bytes: u64,
        failed: bool,
        body: Vec<u8>,
        _marker: PhantomData<(SortKey, K)>,
    }

    impl<SortKey, K> Reader<SortKey, K>
    where
        SortKey: for<'de> Deserialize<'de>,
        K: for<'de> Deserialize<'de>,
    {
        pub(crate) fn open(
            path: &Path,
            expected_database_id: [u8; 16],
            expected_generation: u64,
        ) -> io::Result<Self> {
            let file = File::open(path)?;
            let length = file.metadata()?.len();
            if length < HEADER_LEN as u64 {
                return Err(invalid_data("sorted-index header is truncated"));
            }
            let mut input = BufReader::new(file);
            let mut encoded = [0; HEADER_LEN];
            input.read_exact(&mut encoded)?;
            let remaining_entries = decode_header(encoded, expected_database_id, expected_generation)?;
            Ok(Self {
                input,
                remaining_entries,
                remaining_bytes: length - HEADER_LEN as u64,
                failed: false,
                body: Vec::new(),
                _marker: PhantomData,
            })
        }

        #[cfg(test)]
        pub(crate) fn remaining(&self) -> u64 { self.remaining_entries }

        #[cfg(test)]
        pub(crate) fn read_next(&mut self) -> io::Result<Option<Entry<SortKey, K>>> {
            let result = match self.read_next_borrowed() {
                Ok(entry) => Ok(entry.map(|entry| Entry {
                    sort_key: entry.sort_key,
                    primary_key: entry.primary_key,
                    locator: entry.locator,
                })),
                Err(error) => Err(error),
            };
            if result.is_err() {
                self.failed = true;
                self.remaining_entries = 0;
            }
            result
        }

        fn read_next_borrowed(&mut self) -> io::Result<Option<BorrowedEntry<'_, SortKey, K>>> {
            if self.failed {
                return Ok(None);
            }
            if self.remaining_entries == 0 {
                if self.remaining_bytes == 0 {
                    return Ok(None);
                }
                self.failed = true;
                self.remaining_bytes = 0;
                return Err(invalid_data("sorted-index contains trailing bytes"));
            }
            self.read_next_inner()
        }

        fn read_next_inner(&mut self) -> io::Result<Option<BorrowedEntry<'_, SortKey, K>>> {
            if self.remaining_bytes < ENTRY_PREFIX_LEN_U64 {
                self.failed = true;
                return Err(invalid_data("sorted-index entry prefix is truncated"));
            }
            let mut prefix = [0; ENTRY_PREFIX_LEN];
            if let Err(error) = self.input.read_exact(&mut prefix) {
                self.failed = true;
                return Err(error);
            }
            self.remaining_bytes -= ENTRY_PREFIX_LEN_U64;
            let body_len = u64::from(read_u32(&prefix, 0)?);
            let expected_crc = read_u32(&prefix, 4)?;
            if body_len < ENTRY_FIXED_BODY_LEN as u64 || body_len > self.remaining_bytes {
                self.failed = true;
                return Err(invalid_data("sorted-index entry length is invalid"));
            }
            let body_len = match usize::try_from(body_len) {
                Ok(body_len) => body_len,
                Err(error) => {
                    self.failed = true;
                    return Err(invalid_integer(error));
                }
            };
            self.body.resize(body_len, 0);
            if let Err(error) = self.input.read_exact(&mut self.body) {
                self.failed = true;
                return Err(error);
            }
            self.remaining_bytes -= u64::try_from(body_len).map_err(invalid_integer)?;
            self.remaining_entries -= 1;
            if crc32fast::hash(&self.body) != expected_crc {
                return Err(invalid_data("sorted-index entry checksum mismatch"));
            }
            let sort_key_len = usize::try_from(read_u32(&self.body, 0)?).map_err(invalid_integer)?;
            let primary_key_len = usize::try_from(read_u32(&self.body, 4)?).map_err(invalid_integer)?;
            let expected_len = ENTRY_FIXED_BODY_LEN
                .checked_add(sort_key_len)
                .and_then(|length| length.checked_add(primary_key_len))
                .ok_or_else(|| invalid_data("sorted-index entry length overflow"))?;
            if expected_len != body_len {
                return Err(invalid_data("sorted-index body length mismatch"));
            }
            let locator = Locator::decode(
                self.body.get(8..24).ok_or_else(|| invalid_data("sorted-index locator is truncated"))?,
            )?;
            let sort_key_end = 24 + sort_key_len;
            let sort_key = binary_deserialize(
                self.body.get(24..sort_key_end).ok_or_else(|| invalid_data("sort key is truncated"))?,
            )?;
            let serialized_primary_key = self
                .body
                .get(sort_key_end..expected_len)
                .ok_or_else(|| invalid_data("primary key is truncated"))?;
            let primary_key = binary_deserialize(serialized_primary_key)?;
            Ok(Some(BorrowedEntry { sort_key, primary_key, serialized_primary_key, locator }))
        }
    }

    pub(crate) struct OwnedIterator<K, V, SortKey> {
        reader: Reader<SortKey, K>,
        query: BPlusTreeQuery<K, V>,
        previous_sort_key: Option<SortKey>,
        finished: bool,
    }

    impl<K, V, SortKey> OwnedIterator<K, V, SortKey>
    where
        K: Ord + for<'de> Deserialize<'de>,
        V: for<'de> Deserialize<'de>,
        SortKey: Ord + for<'de> Deserialize<'de>,
    {
        pub(crate) fn open(query: BPlusTreeQuery<K, V>, index_path: &Path) -> io::Result<Self> {
            let (database_id, generation) = query.snapshot_identity();
            let reader = Reader::open(index_path, database_id, generation)?;
            Ok(Self { reader, query, previous_sort_key: None, finished: false })
        }

        #[cfg(test)]
        pub(crate) fn remaining(&self) -> u64 { self.reader.remaining() }
    }

    impl<K, V, SortKey> Iterator for OwnedIterator<K, V, SortKey>
    where
        K: Ord + for<'de> Deserialize<'de>,
        V: for<'de> Deserialize<'de>,
        SortKey: Ord + for<'de> Deserialize<'de>,
    {
        type Item = io::Result<(K, V)>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.finished {
                return None;
            }
            let result = match self.reader.read_next_borrowed() {
                Ok(Some(entry)) => {
                    if self.previous_sort_key.as_ref().is_some_and(|previous| previous > &entry.sort_key) {
                        Err(invalid_data("sorted-index entries are out of order"))
                    } else {
                        let BorrowedEntry { sort_key, primary_key, serialized_primary_key, locator } = entry;
                        self.previous_sort_key = Some(sort_key);
                        self.query
                            .read_locator_value(locator, serialized_primary_key)
                            .map(|value| (primary_key, value))
                    }
                }
                Ok(None) => {
                    self.finished = true;
                    return None;
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(entry) => Some(Ok(entry)),
                Err(error) => Some(Err(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::bplustree::v3::{BPlusTree as V3Tree, BPlusTreeQuery as V3Query, Locator};
    use crate::utils::binary_serialize;
    use std::{fs, io, io::Write};
    use tempfile::tempdir;

    #[test]
    fn v4_header_entries_and_identity_are_exact_and_checked() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("v4.idx");
        let database_id = [0x5a; 16];
        let locator = Locator { leaf_page_id: 7, slot_index: 3, serialized_key_crc32: 0x1122_3344 };
        let mut writer = v4::Writer::<String, u32>::new(&path, database_id, 9)?;
        writer.push(&String::from("sort"), &42, locator)?;
        assert_eq!(writer.finish()?, 1);

        let bytes = fs::read(&path)?;
        assert_eq!(&bytes[0..4], b"SIDX");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().map_err(io::Error::other)?), 4);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().map_err(io::Error::other)?), 3);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().map_err(io::Error::other)?), 64);
        assert_eq!(&bytes[16..32], &database_id);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().map_err(io::Error::other)?), 9);
        assert_eq!(u64::from_le_bytes(bytes[40..48].try_into().map_err(io::Error::other)?), 1);
        assert!(bytes[48..56].iter().chain(&bytes[60..64]).all(|byte| *byte == 0));

        let mut reader = v4::Reader::<String, u32>::open(&path, database_id, 9)?;
        let entry = reader.read_next()?.ok_or_else(|| io::Error::other("v4 entry missing"))?;
        assert_eq!(entry.sort_key, "sort");
        assert_eq!(entry.primary_key, 42);
        assert_eq!(entry.locator, locator);
        assert!(reader.read_next()?.is_none());
        assert!(v4::Reader::<String, u32>::open(&path, [0; 16], 9).is_err());
        assert!(v4::Reader::<String, u32>::open(&path, database_id, 10).is_err());
        Ok(())
    }

    #[test]
    fn v4_entry_corruption_is_reported_once_then_reader_fuses() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("v4-corrupt.idx");
        let database_id = [7; 16];
        let mut writer = v4::Writer::<u32, u32>::new(&path, database_id, 1)?;
        writer.push(&1, &2, Locator::for_key(3, 0, &binary_serialize(&2u32)?)?)?;
        writer.finish()?;
        let mut bytes = fs::read(&path)?;
        *bytes.get_mut(72).ok_or_else(|| io::Error::other("v4 body missing"))? ^= 1;
        fs::write(&path, bytes)?;

        let mut reader = v4::Reader::<u32, u32>::open(&path, database_id, 1)?;
        assert!(reader.read_next().is_err());
        assert!(reader.read_next()?.is_none());
        Ok(())
    }

    #[test]
    fn v4_trailing_bytes_are_reported_once_then_reader_fuses() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("v4-trailing.idx");
        let database_id = [8; 16];
        let mut writer = v4::Writer::<u32, u32>::new(&path, database_id, 1)?;
        writer.push(&1, &2, Locator::for_key(3, 0, &binary_serialize(&2u32)?)?)?;
        writer.finish()?;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
        file.write_all(&[0xff])?;
        file.sync_all()?;

        let mut reader = v4::Reader::<u32, u32>::open(&path, database_id, 1)?;
        assert!(reader.read_next()?.is_some());
        assert!(reader.read_next().is_err());
        assert!(reader.read_next()?.is_none());
        Ok(())
    }

    #[test]
    fn v4_sorted_iterator_validates_locators_and_fuses_after_a_late_error() -> io::Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("tree.db");
        let index = dir.path().join("tree.idx");
        let mut tree = V3Tree::new();
        tree.insert(1u32, String::from("one"));
        tree.insert(2u32, String::from("two"));
        tree.store(&database)?;
        let mut query = V3Query::<u32, String>::try_new(&database)?;
        let entries = query.collect_with_locators()?;
        let (database_id, generation) = query.snapshot_identity();
        drop(query);

        let mut writer = v4::Writer::<u32, u32>::new(&index, database_id, generation)?;
        writer.push(&1, &entries[0].0, entries[0].2)?;
        writer.push(
            &2,
            &entries[1].0,
            Locator { slot_index: u16::MAX, ..entries[1].2 },
        )?;
        writer.finish()?;

        let query = V3Query::<u32, String>::try_new(&database)?;
        let mut iterator = v4::OwnedIterator::<u32, String, u32>::open(query, &index)?;
        assert_eq!(iterator.next().transpose()?, Some((1, String::from("one"))));
        assert!(iterator.next().is_some_and(|entry| entry.is_err()));
        assert!(iterator.next().is_none());
        Ok(())
    }

    #[test]
    fn v4_sorted_iterator_rejects_out_of_order_sort_keys() -> io::Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("tree.db");
        let index = dir.path().join("tree.idx");
        let mut tree = V3Tree::new();
        tree.insert(1u32, String::from("one"));
        tree.insert(2u32, String::from("two"));
        tree.store(&database)?;
        let mut query = V3Query::<u32, String>::try_new(&database)?;
        let entries = query.collect_with_locators()?;
        let (database_id, generation) = query.snapshot_identity();
        drop(query);

        let mut writer = v4::Writer::<u32, u32>::new(&index, database_id, generation)?;
        writer.push(&2, &entries[1].0, entries[1].2)?;
        writer.push(&1, &entries[0].0, entries[0].2)?;
        writer.finish()?;

        let query = V3Query::<u32, String>::try_new(&database)?;
        let mut iterator = v4::OwnedIterator::<u32, String, u32>::open(query, &index)?;
        assert_eq!(iterator.next().transpose()?, Some((2, String::from("two"))));
        assert!(iterator.next().is_some_and(|entry| entry.is_err()));
        assert!(iterator.next().is_none());
        Ok(())
    }
}
