use shared::{error::TuliproxError, utils::normalize_container_extension};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
};

/// File name of an input's sampling ledger, inside its Stalker storage directory.
pub const CONTAINER_EXTENSION_LEDGER_FILE: &str = "container_extension_ledger.json";

/// What the portal's playback answer revealed about a Stalker VOD item: `(provider stream id) -> extension`.
///
/// The ledger is independent of the import, so a playlist update never rewrites it and a sampling run
/// never touches the catalog. Delivery reads the published copy; the file is only read here, by the
/// background run, and rewritten atomically after it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerExtensionLedger {
    entries: BTreeMap<u32, Arc<str>>,
}

impl ContainerExtensionLedger {
    /// The sampled extension of one item, canonical as stored.
    #[must_use]
    pub fn get(&self, stream_id: u32) -> Option<&str> { self.entries.get(&stream_id).map(AsRef::as_ref) }

    #[must_use]
    pub fn contains(&self, stream_id: u32) -> bool { self.entries.contains_key(&stream_id) }

    #[must_use]
    pub fn len(&self) -> usize { self.entries.len() }

    #[must_use]
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// Record one sampled extension. A value that is not a container extension is ignored, so a
    /// malformed portal answer never reaches a client. Returns whether the ledger changed.
    pub fn insert(&mut self, stream_id: u32, extension: &str) -> bool {
        let Some(extension) = normalize_container_extension(extension) else { return false };
        let extension: Arc<str> = extension.as_ref().into();
        match self.entries.insert(stream_id, Arc::clone(&extension)) {
            Some(previous) => previous != extension,
            None => true,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &str)> { self.entries.iter().map(|(id, ext)| (*id, ext.as_ref())) }

    /// The entries as a map the delivery path can publish.
    #[must_use]
    pub fn to_map(&self) -> HashMap<u32, Arc<str>> {
        self.entries.iter().map(|(stream_id, extension)| (*stream_id, Arc::clone(extension))).collect()
    }

    /// Drop entries of items the catalog no longer holds. Returns how many were removed.
    pub fn prune_to(&mut self, existing: &HashSet<u32>) -> usize {
        let before = self.entries.len();
        self.entries.retain(|stream_id, _| existing.contains(stream_id));
        before - self.entries.len()
    }

    /// Read an input's ledger. A missing file is an empty ledger; an unreadable one is an error the
    /// caller reports and then treats as empty, so a damaged file costs one sampling run instead of
    /// blocking every future one.
    pub async fn load(path: &Path) -> Result<Self, TuliproxError> {
        let content = match tokio::fs::read_to_string(path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(TuliproxError::Io(format!(
                    "failed to read container extension ledger {}: {err}",
                    path.display()
                )));
            }
        };
        let stored: BTreeMap<u32, String> = serde_json::from_str(&content).map_err(|err| {
            TuliproxError::Io(format!("failed to parse container extension ledger {}: {err}", path.display()))
        })?;
        let mut ledger = Self::default();
        for (stream_id, extension) in stored {
            ledger.insert(stream_id, &extension);
        }
        Ok(ledger)
    }

    /// Write the ledger atomically: a reader sees either the previous file or the complete new one,
    /// never a half-written map.
    pub async fn store(&self, path: &Path) -> Result<(), TuliproxError> {
        let document: BTreeMap<u32, &str> = self.iter().collect();
        let content = serde_json::to_vec(&document)
            .map_err(|err| TuliproxError::Io(format!("failed to encode container extension ledger: {err}")))?;
        let temporary = path.with_extension("json.tmp");
        tokio::fs::write(&temporary, content).await.map_err(|err| {
            TuliproxError::Io(format!("failed to write container extension ledger {}: {err}", temporary.display()))
        })?;
        tokio::fs::rename(&temporary, path).await.map_err(|err| {
            TuliproxError::Io(format!("failed to replace container extension ledger {}: {err}", path.display()))
        })
    }
}
