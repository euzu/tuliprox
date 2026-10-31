pub mod v4 {
    use crate::{
        codec::{binary_deserialize, binary_serialize},
        v3::{publish_database, sync_parent_directory, temporary_path, BPlusTreeQuery, Locator},
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

    pub(crate) fn header(database_id: [u8; 16], generation: u64, count: u64) -> [u8; HEADER_LEN] {
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct HeaderIdentity {
        database_id: [u8; 16],
        generation: u64,
        count: u64,
    }

    fn decode_header_identity(mut bytes: [u8; HEADER_LEN]) -> io::Result<HeaderIdentity> {
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
        let mut database_id = [0u8; 16];
        database_id.copy_from_slice(&bytes[16..32]);
        let generation = read_u64(&bytes, 32)?;
        let count = read_u64(&bytes, 40)?;
        Ok(HeaderIdentity { database_id, generation, count })
    }

    fn decode_header(
        bytes: [u8; HEADER_LEN],
        expected_database_id: [u8; 16],
        expected_generation: u64,
    ) -> io::Result<(u64, bool)> {
        let identity = decode_header_identity(bytes)?;
        if identity.database_id != expected_database_id {
            return Err(invalid_data("sorted-index tree identity mismatch"));
        }
        if identity.generation > expected_generation {
            return Err(invalid_data("sorted-index generation is newer than tree generation"));
        }
        let is_stale = identity.generation < expected_generation;
        Ok((identity.count, is_stale))
    }

    pub struct Writer<SortKey, K> {
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
        pub fn new(path: &Path, database_id: [u8; 16], generation: u64) -> io::Result<Self> {
            let file = OpenOptions::new().write(true).create_new(true).open(path)?;
            let mut output = BufWriter::new(file);
            output.write_all(&header(database_id, generation, 0))?;
            Ok(Self { output, database_id, generation, count: 0, body: Vec::new(), _marker: PhantomData })
        }

        pub fn push(&mut self, sort_key: &SortKey, primary_key: &K, locator: Locator) -> io::Result<()> {
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

        pub fn finish(mut self) -> io::Result<u64> {
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
        stale: bool,
        body: Vec<u8>,
        _marker: PhantomData<(SortKey, K)>,
    }

    impl<SortKey, K> Reader<SortKey, K>
    where
        SortKey: for<'de> Deserialize<'de>,
        K: for<'de> Deserialize<'de>,
    {
        pub(crate) fn open(path: &Path, expected_database_id: [u8; 16], expected_generation: u64) -> io::Result<Self> {
            let file = File::open(path)?;
            let length = file.metadata()?.len();
            if length < HEADER_LEN as u64 {
                return Err(invalid_data("sorted-index header is truncated"));
            }
            let mut input = BufReader::new(file);
            let mut encoded = [0; HEADER_LEN];
            input.read_exact(&mut encoded)?;
            let (remaining_entries, stale) = decode_header(encoded, expected_database_id, expected_generation)?;
            let remaining_bytes = length - HEADER_LEN as u64;
            Ok(Self::from_reader(input, remaining_bytes, remaining_entries, stale))
        }

        fn from_reader(input: BufReader<File>, remaining_bytes: u64, remaining_entries: u64, stale: bool) -> Self {
            Self {
                input,
                remaining_entries,
                remaining_bytes,
                failed: false,
                stale,
                body: Vec::new(),
                _marker: PhantomData,
            }
        }

        #[inline]
        pub(crate) fn is_stale(&self) -> bool { self.stale }

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
            let serialized_primary_key =
                self.body.get(sort_key_end..expected_len).ok_or_else(|| invalid_data("primary key is truncated"))?;
            let primary_key = binary_deserialize(serialized_primary_key)?;
            Ok(Some(BorrowedEntry { sort_key, primary_key, serialized_primary_key, locator }))
        }
    }

    pub struct OwnedIterator<K, V, SortKey> {
        reader: Reader<SortKey, K>,
        query: BPlusTreeQuery<K, V>,
        previous_sort_key: Option<SortKey>,
        is_stale: bool,
        finished: bool,
    }

    impl<K, V, SortKey> OwnedIterator<K, V, SortKey>
    where
        K: Ord + for<'de> Deserialize<'de>,
        V: for<'de> Deserialize<'de>,
        SortKey: Ord + for<'de> Deserialize<'de>,
    {
        pub fn open(query: BPlusTreeQuery<K, V>, index_path: &Path) -> io::Result<Self> {
            let (database_id, generation) = query.snapshot_identity();
            let reader = Reader::open(index_path, database_id, generation)?;
            let is_stale = reader.is_stale();
            Ok(Self { reader, query, previous_sort_key: None, is_stale, finished: false })
        }

        #[cfg(test)]
        pub fn remaining(&self) -> u64 { self.reader.remaining() }
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
            loop {
                let entry = match self.reader.read_next_borrowed() {
                    Ok(Some(entry)) => entry,
                    Ok(None) => {
                        self.finished = true;
                        return None;
                    }
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                };

                if self.previous_sort_key.as_ref().is_some_and(|previous| previous > &entry.sort_key) {
                    self.finished = true;
                    return Some(Err(invalid_data("sorted-index entries are out of order")));
                }

                let BorrowedEntry { sort_key, primary_key, serialized_primary_key, locator } = entry;
                self.previous_sort_key = Some(sort_key);

                if self.is_stale {
                    match self.query.query_io(&primary_key) {
                        Ok(Some(value)) => return Some(Ok((primary_key, value))),
                        Ok(None) => {}
                        Err(error) => {
                            self.finished = true;
                            return Some(Err(error));
                        }
                    }
                    continue;
                }

                let result =
                    self.query.read_locator_value(locator, serialized_primary_key).map(|value| (primary_key, value));
                return Some(result);
            }
        }
    }

    /// In-memory snapshot of a sorted index for read-modify-write workflows.
    ///
    /// Every `BPlusTreeUpdate` commit deletes the sorted index file. Therefore, a stale
    /// index only exists when the caller holding the database write lock rewrote it via
    /// `write_merged` right after its own commit, and it must contain every key of the tree.
    #[derive(Debug)]
    pub struct IndexSnapshot<SortKey, K> {
        database_id: [u8; 16],
        generation: u64,
        entries: Vec<(SortKey, K, Locator)>,
    }

    impl<SortKey, K> IndexSnapshot<SortKey, K>
    where
        SortKey: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
        K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    {
        pub fn read(index_path: &Path) -> io::Result<Option<Self>> {
            let file = match File::open(index_path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err),
            };
            let length = file.metadata()?.len();
            if length < HEADER_LEN as u64 {
                return Err(invalid_data("sorted-index header is truncated"));
            }
            let mut input = BufReader::new(file);
            let mut encoded = [0; HEADER_LEN];
            input.read_exact(&mut encoded)?;
            let identity = decode_header_identity(encoded)?;
            let remaining_bytes = length - HEADER_LEN as u64;
            let min_entry_len = (ENTRY_PREFIX_LEN + ENTRY_FIXED_BODY_LEN) as u64;
            if identity.count > remaining_bytes / min_entry_len {
                return Err(invalid_data("sorted-index count exceeds remaining file bytes"));
            }
            let mut reader = Reader::<SortKey, K>::from_reader(input, remaining_bytes, identity.count, false);
            let count = usize::try_from(identity.count).map_err(invalid_integer)?;
            let mut entries = Vec::with_capacity(count);
            while let Some(entry) = reader.read_next_borrowed()? {
                entries.push((entry.sort_key, entry.primary_key, entry.locator));
            }
            Ok(Some(Self { database_id: identity.database_id, generation: identity.generation, entries }))
        }

        pub fn write_merged(mut self, index_path: &Path, changed: &[(SortKey, K)]) -> io::Result<()> {
            let mut changed_map = std::collections::BTreeMap::new();
            for (sort_key, primary_key) in changed {
                changed_map.insert(primary_key, sort_key);
            }
            if !changed_map.is_empty() {
                self.entries.retain(|(_, primary_key, _)| !changed_map.contains_key(primary_key));
                let dummy_locator = Locator { leaf_page_id: 1, slot_index: 0, serialized_key_crc32: 0 };
                for (primary_key, sort_key) in changed_map {
                    self.entries.push((sort_key.clone(), primary_key.clone(), dummy_locator));
                }
                self.entries.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
            }

            publish_index(index_path, self.database_id, self.generation, &self.entries)
        }
    }

    pub(crate) fn publish_index<'a, SortKey, K>(
        destination: &Path,
        database_id: [u8; 16],
        generation: u64,
        entries: impl IntoIterator<Item = &'a (SortKey, K, Locator)>,
    ) -> io::Result<()>
    where
        SortKey: Serialize + 'a,
        K: Serialize + 'a,
    {
        let temporary = temporary_path(destination)?;
        let write_result = (|| -> io::Result<()> {
            let mut writer = Writer::<SortKey, K>::new(&temporary, database_id, generation)?;
            for (sort_key, primary_key, locator) in entries {
                writer.push(sort_key, primary_key, *locator)?;
            }
            writer.finish()?;
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }

        publish_database(&temporary, destination, sync_parent_directory).map_err(io::Error::from)
    }
}

pub(crate) use v4::publish_index;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::binary_serialize,
        v3::{BPlusTree as V3Tree, BPlusTreeQuery as V3Query, Locator},
    };
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
        assert!(v4::Reader::<String, u32>::open(&path, database_id, 8).is_err());
        let stale_reader = v4::Reader::<String, u32>::open(&path, database_id, 10)?;
        assert!(stale_reader.is_stale());
        Ok(())
    }

    #[test]
    fn stale_generation_by_key_read_skips_missing_keys_and_returns_sorted() -> io::Result<()> {
        use crate::BPlusTreeUpdate;

        let dir = tempdir()?;
        let database = dir.path().join("stale_test.db");
        let index = dir.path().join("stale_test.idx");

        // Key 1 -> sort 30, Key 2 -> sort 10, Key 3 -> sort 20
        let mut tree = V3Tree::new();
        tree.insert(1u32, (30u32, String::from("one")));
        tree.insert(2u32, (10u32, String::from("two")));
        tree.insert(3u32, (20u32, String::from("three")));
        tree.store_with_index(&database, |v| v.0)?;

        let snapshot = v4::IndexSnapshot::<u32, u32>::read(&index)?.expect("snapshot exists");

        // Update database in place: modify key 2 and delete key 3, incrementing generation
        let mut updater = BPlusTreeUpdate::<u32, (u32, String)>::try_new(&database)?;
        updater.upsert(&2, &(10, String::from("two-updated")))?;
        updater.delete(&3)?;
        updater.commit()?;

        // Restore index snapshot (written with stale generation)
        snapshot.write_merged(&index, &[])?;

        // Index generation is now older than DB generation
        let query = V3Query::<u32, (u32, String)>::try_new(&database)?;
        let iterator = v4::OwnedIterator::<u32, (u32, String), u32>::open(query, &index)?;
        let items: Vec<(u32, (u32, String))> = iterator.collect::<io::Result<Vec<_>>>()?;

        // Expected order: key 2 (sort 10), key 1 (sort 30); key 3 (deleted) is skipped
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], (2, (10, String::from("two-updated"))));
        assert_eq!(items[1], (1, (30, String::from("one"))));

        Ok(())
    }

    #[test]
    fn merge_with_replacement_of_existing_primary_key_maintains_canonical_order() -> io::Result<()> {
        let dir = tempdir()?;
        let index = dir.path().join("merge_test.idx");
        let database_id = [0x42; 16];
        let generation = 5;

        let mut writer = v4::Writer::<u32, u32>::new(&index, database_id, generation)?;
        let dummy_loc = Locator { leaf_page_id: 1, slot_index: 0, serialized_key_crc32: 0 };
        writer.push(&10, &1, dummy_loc)?;
        writer.push(&30, &3, dummy_loc)?;
        writer.finish()?;

        let snapshot = v4::IndexSnapshot::<u32, u32>::read(&index)?.expect("snapshot exists");
        snapshot.write_merged(&index, &[(5, 0), (50, 1)])?;

        let mut reader = v4::Reader::<u32, u32>::open(&index, database_id, generation)?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.read_next()? {
            entries.push((entry.sort_key, entry.primary_key));
        }

        assert_eq!(entries, vec![(5, 0), (30, 3), (50, 1)]);
        Ok(())
    }

    #[test]
    fn newer_index_generation_rejected() -> io::Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("newer_gen.db");
        let index = dir.path().join("newer_gen.idx");

        let mut tree = V3Tree::new();
        tree.insert(1u32, String::from("one"));
        tree.store(&database)?;

        let query = V3Query::<u32, String>::try_new(&database)?;
        let (database_id, generation) = query.snapshot_identity();
        drop(query);

        let mut writer = v4::Writer::<u32, u32>::new(&index, database_id, generation + 1)?;
        writer.push(&1, &1, Locator { leaf_page_id: 1, slot_index: 0, serialized_key_crc32: 0 })?;
        writer.finish()?;

        let query = V3Query::<u32, String>::try_new(&database)?;
        let Err(err) = v4::OwnedIterator::<u32, String, u32>::open(query, &index) else {
            panic!("expected newer generation error");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("newer than tree generation"));
        Ok(())
    }

    #[test]
    fn database_id_mismatch_rejected() -> io::Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("mismatch.db");
        let index = dir.path().join("mismatch.idx");

        let mut tree = V3Tree::new();
        tree.insert(1u32, String::from("one"));
        tree.store(&database)?;

        let mut writer = v4::Writer::<u32, u32>::new(&index, [0xaa; 16], 1)?;
        writer.push(&1, &1, Locator { leaf_page_id: 1, slot_index: 0, serialized_key_crc32: 0 })?;
        writer.finish()?;

        let query = V3Query::<u32, String>::try_new(&database)?;
        assert!(v4::OwnedIterator::<u32, String, u32>::open(query, &index).is_err());
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
        writer.push(&2, &entries[1].0, Locator { slot_index: u16::MAX, ..entries[1].2 })?;
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

    #[test]
    fn oversized_header_count_rejected() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("oversized.idx");
        let database_id = [0x55; 16];
        let generation = 1;
        let encoded = v4::header(database_id, generation, 100);
        fs::write(&path, encoded)?;

        let Err(err) = v4::IndexSnapshot::<u32, u32>::read(&path) else {
            panic!("oversized count must fail");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("sorted-index count"));
        Ok(())
    }
}
