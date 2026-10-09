use super::{BPlusTree, DatabaseHeader};
use crate::v3::format::{PageHeader, PageType, PAGE_HEADER_LEN, PAGE_SIZE};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::{
        mpsc,
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread,
    thread::JoinHandle,
    time::Duration,
};

pub(in crate::v3::tree::tests) const PAGE_ID: u64 = 7;

pub(in crate::v3::tree::tests) const NEXT_PAGE_ID: u64 = 20;

pub(in crate::v3::tree::tests) fn invalid_data<T>(result: io::Result<T>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::InvalidData => Ok(()),
        Err(err) => Err(io::Error::other(format!("expected InvalidData, got {err}"))),
        Ok(_) => Err(io::Error::other("expected InvalidData")),
    }
}

pub(in crate::v3::tree::tests) fn invalid_input<T>(result: io::Result<T>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
        Err(err) => Err(io::Error::other(format!("expected InvalidInput, got {err}"))),
        Ok(_) => Err(io::Error::other("expected InvalidInput")),
    }
}

pub(in crate::v3::tree::tests) fn database_header(path: &Path) -> io::Result<DatabaseHeader> {
    let bytes = fs::read(path)?;
    DatabaseHeader::decode(
        bytes
            .get(..PAGE_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "database header is truncated"))?,
    )
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::v3::tree::tests) struct ConditionalSerialize {
    pub(in crate::v3::tree::tests) value: String,
    pub(in crate::v3::tree::tests) fail: bool,
}

impl Serialize for ConditionalSerialize {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.fail {
            return Err(serde::ser::Error::custom("injected serialization failure"));
        }
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConditionalSerialize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|value| Self { value, fail: false })
    }
}

pub(in crate::v3::tree::tests) fn spawn_replacement_writer(
    path: PathBuf,
) -> io::Result<(Receiver<io::Result<u64>>, JoinHandle<()>)> {
    let (started_sender, started_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut replacement = BPlusTree::new();
        replacement.insert(2u32, String::from("replacement"));
        let _ = started_sender.send(());
        let _ = result_sender.send(replacement.store(&path));
    });
    started_receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| io::Error::other(format!("replacement writer did not start: {error}")))?;
    Ok((result_receiver, handle))
}

pub(in crate::v3::tree::tests) fn assert_writer_is_blocked(receiver: &Receiver<io::Result<u64>>) -> io::Result<()> {
    match receiver.recv_timeout(Duration::from_millis(100)) {
        Err(RecvTimeoutError::Timeout) => Ok(()),
        Err(RecvTimeoutError::Disconnected) => Err(io::Error::other("replacement writer disconnected")),
        Ok(result) => {
            let root = result?;
            Err(io::Error::other(format!("replacement writer completed early with root page {root}")))
        }
    }
}

pub(in crate::v3::tree::tests) fn finish_writer(
    receiver: &Receiver<io::Result<u64>>,
    handle: JoinHandle<()>,
) -> io::Result<()> {
    receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| io::Error::other(format!("replacement writer stayed blocked: {error}")))??;
    handle.join().map_err(|_| io::Error::other("replacement writer panicked"))
}

/// Waits for the sidecar lock to become free.
///
/// A writer reporting its result does not mean the lock is already released:
/// publication can finish on the engine's background thread, so the release
/// happens on a different thread from the one the test observes. Asserting
/// `try_exclusive_sidecar` immediately races with that hand-off and fails
/// intermittently. Waiting for the lock to become available tests what the
/// engine actually promises - that it is released - without asserting an
/// instantaneous guarantee it never made.
pub(in crate::v3::tree::tests) fn wait_for_exclusive_sidecar(database: &Path) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if try_exclusive_sidecar(database)? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub(in crate::v3::tree::tests) fn try_exclusive_sidecar(database: &Path) -> io::Result<bool> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(crate::common::sidecar_lock_path(database))?;
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(true)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

pub(in crate::v3::tree::tests) fn empty_leaf(page_id: u64, next_page_id: u64) -> io::Result<[u8; PAGE_SIZE]> {
    let mut page = [0; PAGE_SIZE];
    PageHeader {
        page_type: PageType::Leaf,
        cell_count: 0,
        free_start: u16::try_from(PAGE_HEADER_LEN).map_err(io::Error::other)?,
        free_end: u16::try_from(PAGE_SIZE).map_err(io::Error::other)?,
        left: 0,
        right: 0,
    }
    .encode_into(&mut page, page_id, next_page_id)?;
    Ok(page)
}

pub(in crate::v3::tree::tests) fn random_value() -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0u64;
    (0..12_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}
