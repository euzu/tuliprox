//! Where a [`ProviderCapabilities`] snapshot survives a restart.
//!
//! Split from the value type so the knowledge is useful before anyone decides where it
//! lives: the in-memory store is enough for a single run, and the JSON store makes it
//! outlast one. Both are held as `impl CapabilityStore` rather than a trait object.

use crate::capabilities::ProviderCapabilities;
use log::warn;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
};

pub trait CapabilityStore: Send + Sync {
    /// The snapshot for `input`, or the default when nothing is stored. A store that
    /// cannot be read reports the default rather than failing: capability knowledge is an
    /// optimisation, and losing it must never stop a refresh.
    fn load(&self, input: &str) -> impl Future<Output = ProviderCapabilities> + Send;

    /// Persist the snapshot for `input`.
    fn save(&self, input: &str, capabilities: &ProviderCapabilities) -> impl Future<Output = ()> + Send;
}

/// Keeps snapshots for the lifetime of the process. What the client falls back to when no
/// storage directory is configured.
#[derive(Debug, Default)]
pub struct InMemoryCapabilityStore {
    entries: RwLock<HashMap<String, ProviderCapabilities>>,
}

impl InMemoryCapabilityStore {
    #[must_use]
    pub fn new() -> Self { Self::default() }
}

impl CapabilityStore for InMemoryCapabilityStore {
    fn load(&self, input: &str) -> impl Future<Output = ProviderCapabilities> + Send {
        let stored = self.entries.read().get(input).cloned().unwrap_or_default();
        async move { stored }
    }

    fn save(&self, input: &str, capabilities: &ProviderCapabilities) -> impl Future<Output = ()> + Send {
        self.entries.write().insert(input.to_string(), capabilities.clone());
        async {}
    }
}

/// One JSON file per input, written atomically through the workspace's existing helper so
/// a crash mid-write cannot leave a half-file that fails to parse on the next start.
#[derive(Debug, Clone)]
pub struct JsonCapabilityStore {
    dir: PathBuf,
}

impl JsonCapabilityStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self { Self { dir: dir.into() } }

    /// The file for `input`. The name is derived rather than taken verbatim: input names
    /// come from user config and can contain separators.
    #[must_use]
    pub fn path_for(&self, input: &str) -> PathBuf {
        self.dir.join(format!("{}.capabilities.json", sanitize_component(input)))
    }
}

impl CapabilityStore for JsonCapabilityStore {
    fn load(&self, input: &str) -> impl Future<Output = ProviderCapabilities> + Send {
        let path = self.path_for(input);
        async move { read_snapshot(&path).await }
    }

    fn save(&self, input: &str, capabilities: &ProviderCapabilities) -> impl Future<Output = ()> + Send {
        let path = self.path_for(input);
        let dir = self.dir.clone();
        let encoded = serde_json::to_vec(capabilities);
        async move {
            let encoded = match encoded {
                Ok(encoded) => encoded,
                Err(err) => {
                    warn!("Could not encode provider capabilities for {}: {err}", path.display());
                    return;
                }
            };
            if let Err(err) = tokio::fs::create_dir_all(&dir).await {
                warn!("Could not create capability directory {}: {err}", dir.display());
                return;
            }
            if let Err(err) = tuliprox_core::utils::write_json_atomic(&path, &encoded).await {
                warn!("Could not persist provider capabilities to {}: {err}", path.display());
            }
        }
    }
}

async fn read_snapshot(path: &Path) -> ProviderCapabilities {
    let Ok(bytes) = tokio::fs::read(path).await else {
        // A missing file is the normal first-run case, not a problem worth logging.
        return ProviderCapabilities::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(capabilities) => capabilities,
        Err(err) => {
            // Corrupt state must not be fatal: re-probing is always available.
            warn!("Ignoring unreadable provider capabilities at {}: {err}", path.display());
            ProviderCapabilities::default()
        }
    }
}

#[inline]
fn sanitize_component(value: &str) -> String { crate::redaction::sanitize_path_component(value, false) }

#[cfg(test)]
mod tests {
    use super::{CapabilityStore, InMemoryCapabilityStore, JsonCapabilityStore};
    use crate::capabilities::ProviderCapabilities;

    const NOW: u64 = 1_700_000_000;

    fn observed() -> ProviderCapabilities {
        let mut capabilities = ProviderCapabilities::default();
        capabilities.record_unsupported("get_all_channels", NOW);
        capabilities.record_handshake("StrictMag", "portal.php", NOW);
        capabilities
    }

    #[tokio::test]
    async fn an_unknown_input_loads_the_default_rather_than_failing() {
        let store = InMemoryCapabilityStore::new();
        assert_eq!(store.load("never-seen").await, ProviderCapabilities::default());
    }

    #[tokio::test]
    async fn in_memory_snapshots_survive_within_the_run() {
        let store = InMemoryCapabilityStore::new();
        store.save("provider", &observed()).await;
        assert_eq!(store.load("provider").await, observed());
        assert_eq!(store.load("other").await, ProviderCapabilities::default(), "inputs must not share state");
    }

    #[tokio::test]
    async fn a_json_snapshot_survives_a_restart() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = JsonCapabilityStore::new(dir.path());

        store.save("provider one", &observed()).await;
        // A second store over the same directory stands in for the next process.
        assert_eq!(JsonCapabilityStore::new(dir.path()).load("provider one").await, observed());
        Ok(())
    }

    #[tokio::test]
    async fn an_input_name_with_separators_cannot_escape_the_directory() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = JsonCapabilityStore::new(dir.path());

        let path = store.path_for("../../etc/passwd");

        assert_eq!(path.parent(), Some(dir.path()));
        assert!(!path.to_string_lossy().contains(".."));
        Ok(())
    }

    #[tokio::test]
    async fn a_corrupt_snapshot_is_ignored_rather_than_fatal() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = JsonCapabilityStore::new(dir.path());
        tokio::fs::write(store.path_for("provider"), b"{ this is not json").await?;

        assert_eq!(store.load("provider").await, ProviderCapabilities::default());
        Ok(())
    }
}
