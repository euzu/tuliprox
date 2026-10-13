//! Inputs an operator has taken out of service.
//!
//! Read on every new stream request and every background provider job, written
//! a few times a day. Readers load one atomic pointer; writers serialize, persist
//! and only then publish, so a failed write never changes what readers see.

use arc_swap::ArcSwapOption;
use log::{error, warn};
use shared::model::{InputMaintenance, InputMaintenanceEntries};
use std::{path::PathBuf, sync::Arc};

pub const INPUT_MAINTENANCE_FILE: &str = "maintenance.json";

/// The read side, for crates that must not depend on the registry itself.
pub trait InputMaintenanceGate: Send + Sync {
    fn is_in_maintenance(&self, input_name: &str) -> bool;
}

/// `Ok(Some(state))` when the call changed something, `Ok(None)` for a no-op.
pub type MaintenanceWrite = std::io::Result<Option<Arc<InputMaintenanceEntries>>>;

const REFUSE_UNREADABLE: &str = "maintenance state could not be read; it is not overwritten";
const REFUSE_READ_ONLY: &str = "maintenance state was loaded read-only";

pub struct InputMaintenanceRegistry {
    /// `None` while no input is in maintenance, which keeps the common check to one load.
    entries: ArcSwapOption<InputMaintenanceEntries>,
    write_lock: tokio::sync::Mutex<()>,
    /// Where the state is persisted; `None` keeps it in memory only.
    path: Option<PathBuf>,
    /// Why writes are refused, if they are.
    refuse_writes: Option<&'static str>,
}

impl InputMaintenanceRegistry {
    #[must_use]
    pub fn new(path: PathBuf) -> Self { Self::with(Some(path), InputMaintenanceEntries::new(), None) }

    /// A registry that never touches the file system.
    #[must_use]
    pub fn in_memory() -> Self { Self::with(None, InputMaintenanceEntries::new(), None) }

    fn with(path: Option<PathBuf>, entries: InputMaintenanceEntries, refuse_writes: Option<&'static str>) -> Self {
        Self {
            entries: ArcSwapOption::new(non_empty(entries)),
            write_lock: tokio::sync::Mutex::new(()),
            path,
            refuse_writes,
        }
    }

    /// Loads the persisted state and drops inputs that no longer exist.
    ///
    /// A corrupt file must not stop the server: it is moved aside and the
    /// registry starts empty, which the dashboard card makes visible. A file that
    /// cannot be read at all stays where it is and is never overwritten.
    pub async fn load(path: PathBuf, known: &[&str]) -> Self {
        let (entries, refuse_writes) = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<InputMaintenanceEntries>(&bytes) {
                Ok(entries) => (entries, None),
                Err(err) => {
                    error!("Unreadable maintenance state {}: {err}", path.display());
                    let corrupt = path.with_extension("json.corrupt");
                    if let Err(err) = tokio::fs::rename(&path, &corrupt).await {
                        error!("Could not move {} aside: {err}", path.display());
                    }
                    (InputMaintenanceEntries::new(), None)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (InputMaintenanceEntries::new(), None),
            Err(err) => {
                error!("Could not read maintenance state {}: {err}; changes are refused", path.display());
                (InputMaintenanceEntries::new(), Some(REFUSE_UNREADABLE))
            }
        };
        let registry = Self::with(Some(path), entries, refuse_writes);
        if registry.refuse_writes.is_none() {
            if let Err(err) = registry.retain_known(known).await {
                warn!("Could not persist pruned maintenance state: {err}");
            }
        }
        registry
    }

    /// Loads the persisted state without ever changing the file, for a run that may
    /// share the storage directory with a running server.
    pub async fn load_read_only(path: PathBuf) -> Self {
        let entries = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<InputMaintenanceEntries>(&bytes).unwrap_or_else(|err| {
                warn!("Ignoring unreadable maintenance state {}: {err}", path.display());
                InputMaintenanceEntries::new()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => InputMaintenanceEntries::new(),
            Err(err) => {
                warn!("Could not read maintenance state {}: {err}", path.display());
                InputMaintenanceEntries::new()
            }
        };
        Self::with(Some(path), entries, Some(REFUSE_READ_ONLY))
    }

    #[inline]
    pub fn is_in_maintenance(&self, input_name: &str) -> bool {
        self.entries.load().as_ref().is_some_and(|entries| entries.contains_key(input_name))
    }

    /// The current state; an empty map when no input is in maintenance.
    pub fn snapshot(&self) -> Arc<InputMaintenanceEntries> { self.entries.load_full().unwrap_or_default() }

    pub async fn add(&self, name: &str, reason: Option<String>, now: i64) -> MaintenanceWrite {
        self.mutate(|entries| upsert(entries, name, reason, now)).await
    }

    pub async fn add_all(&self, names: &[&str], reason: Option<String>, now: i64) -> MaintenanceWrite {
        self.mutate(|entries| {
            let mut changed = false;
            for name in names {
                if !entries.contains_key(*name) {
                    entries.insert((*name).to_string(), InputMaintenance { since: now, reason: reason.clone() });
                    changed = true;
                }
            }
            changed
        })
        .await
    }

    pub async fn remove(&self, name: &str) -> MaintenanceWrite {
        if !self.is_in_maintenance(name) {
            return Ok(None);
        }
        self.mutate(|entries| entries.remove(name).is_some()).await
    }

    pub async fn remove_all(&self) -> MaintenanceWrite {
        if self.entries.load().is_none() {
            return Ok(None);
        }
        self.mutate(|entries| {
            let changed = !entries.is_empty();
            entries.clear();
            changed
        })
        .await
    }

    /// Drops entries whose input is gone. Checks the snapshot first, so a reload
    /// that keeps every input takes no lock and does no IO.
    pub async fn retain_known(&self, known: &[&str]) -> MaintenanceWrite {
        let has_orphans = self
            .entries
            .load()
            .as_ref()
            .is_some_and(|entries| entries.keys().any(|name| !known.contains(&name.as_str())));
        if !has_orphans {
            return Ok(None);
        }
        self.mutate(|entries| {
            let before = entries.len();
            entries.retain(|name, _| known.contains(&name.as_str()));
            entries.len() != before
        })
        .await
    }

    async fn mutate(&self, apply: impl FnOnce(&mut InputMaintenanceEntries) -> bool) -> MaintenanceWrite {
        if let Some(reason) = self.refuse_writes {
            return Err(std::io::Error::other(reason));
        }
        let _guard = self.write_lock.lock().await;
        let mut entries = self.entries.load().as_deref().cloned().unwrap_or_default();
        if !apply(&mut entries) {
            return Ok(None);
        }
        if let Some(path) = &self.path {
            let encoded = serde_json::to_vec(&entries).map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            crate::utils::write_json_atomic(path, &encoded).await?;
        }
        let published = Arc::new(entries);
        self.entries.store((!published.is_empty()).then(|| Arc::clone(&published)));
        Ok(Some(published))
    }
}

impl InputMaintenanceGate for InputMaintenanceRegistry {
    #[inline]
    fn is_in_maintenance(&self, input_name: &str) -> bool { Self::is_in_maintenance(self, input_name) }
}

fn non_empty(entries: InputMaintenanceEntries) -> Option<Arc<InputMaintenanceEntries>> {
    (!entries.is_empty()).then(|| Arc::new(entries))
}

fn upsert(entries: &mut InputMaintenanceEntries, name: &str, reason: Option<String>, now: i64) -> bool {
    match entries.get_mut(name) {
        Some(existing) if existing.reason == reason => false,
        Some(existing) => {
            existing.reason = reason;
            true
        }
        None => {
            entries.insert(name.to_string(), InputMaintenance { since: now, reason });
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry_in(dir: &TempDir) -> InputMaintenanceRegistry {
        InputMaintenanceRegistry::new(dir.path().join(INPUT_MAINTENANCE_FILE))
    }

    #[tokio::test]
    async fn empty_registry_reports_nothing_in_maintenance() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        assert!(!registry.is_in_maintenance("provider_a"));
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn add_persists_and_publishes() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        let changed = registry.add("provider_a", Some("down".into()), 100).await.unwrap();
        assert!(changed.is_some());
        assert!(registry.is_in_maintenance("provider_a"));
        assert!(!registry.is_in_maintenance("provider_b"));

        let reloaded = InputMaintenanceRegistry::load(dir.path().join(INPUT_MAINTENANCE_FILE), &["provider_a"]).await;
        let entry = reloaded.snapshot().get("provider_a").cloned().unwrap();
        assert_eq!(entry, InputMaintenance { since: 100, reason: Some("down".into()) });
    }

    #[tokio::test]
    async fn re_add_updates_reason_and_keeps_since() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        registry.add("provider_a", Some("first".into()), 100).await.unwrap();
        registry.add("provider_a", Some("second".into()), 200).await.unwrap();
        let entry = registry.snapshot().get("provider_a").cloned().unwrap();
        assert_eq!(entry, InputMaintenance { since: 100, reason: Some("second".into()) });
    }

    #[tokio::test]
    async fn re_add_with_same_reason_is_a_no_op() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        registry.add("provider_a", None, 100).await.unwrap();
        assert!(registry.add("provider_a", None, 200).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn add_all_keeps_existing_entries() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        registry.add("provider_a", Some("old".into()), 100).await.unwrap();
        registry.add_all(&["provider_a", "provider_b"], Some("bulk".into()), 200).await.unwrap();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot["provider_a"], InputMaintenance { since: 100, reason: Some("old".into()) });
        assert_eq!(snapshot["provider_b"], InputMaintenance { since: 200, reason: Some("bulk".into()) });
    }

    #[tokio::test]
    async fn remove_and_remove_all_normalize_to_empty() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        registry.add_all(&["provider_a", "provider_b"], None, 100).await.unwrap();
        registry.remove("provider_a").await.unwrap();
        assert!(!registry.is_in_maintenance("provider_a"));
        assert!(registry.remove("provider_a").await.unwrap().is_none(), "second remove is a no-op");
        let cleared = registry.remove_all().await.unwrap().unwrap();
        assert!(cleared.is_empty());
        assert!(registry.entries.load().is_none(), "empty state is stored as None");
    }

    #[tokio::test]
    async fn retain_known_prunes_orphans_and_is_free_when_nothing_changes() {
        let dir = TempDir::new().unwrap();
        let registry = registry_in(&dir);
        registry.add_all(&["provider_a", "provider_b"], None, 100).await.unwrap();
        assert!(registry.retain_known(&["provider_a", "provider_b", "provider_c"]).await.unwrap().is_none());
        let pruned = registry.retain_known(&["provider_a"]).await.unwrap().unwrap();
        assert_eq!(pruned.keys().collect::<Vec<_>>(), vec!["provider_a"]);
    }

    #[tokio::test]
    async fn failed_persist_leaves_runtime_unchanged() {
        let dir = TempDir::new().unwrap();
        // A non-empty directory where the file should be makes the atomic rename fail.
        let path = dir.path().join(INPUT_MAINTENANCE_FILE);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("blocker"), b"x").unwrap();
        let registry = InputMaintenanceRegistry::new(path);
        assert!(registry.add("provider_a", None, 100).await.is_err());
        assert!(!registry.is_in_maintenance("provider_a"));
    }

    #[tokio::test]
    async fn load_of_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let registry = InputMaintenanceRegistry::load(dir.path().join(INPUT_MAINTENANCE_FILE), &["provider_a"]).await;
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn load_moves_corrupt_file_aside() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INPUT_MAINTENANCE_FILE);
        std::fs::write(&path, b"{ not json").unwrap();
        let registry = InputMaintenanceRegistry::load(path.clone(), &["provider_a"]).await;
        assert!(registry.snapshot().is_empty());
        assert!(!path.exists());
        assert!(dir.path().join("maintenance.json.corrupt").exists());
    }

    #[tokio::test]
    async fn load_prunes_unknown_inputs_and_persists_the_result() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INPUT_MAINTENANCE_FILE);
        std::fs::write(&path, br#"{"gone":{"since":1},"provider_a":{"since":2}}"#).unwrap();
        let registry = InputMaintenanceRegistry::load(path.clone(), &["provider_a"]).await;
        assert!(!registry.is_in_maintenance("gone"));
        assert!(registry.is_in_maintenance("provider_a"));
        let persisted: InputMaintenanceEntries = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.keys().collect::<Vec<_>>(), vec!["provider_a"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_file_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INPUT_MAINTENANCE_FILE);
        std::fs::write(&path, br#"{"provider_a":{"since":1}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Running as root: permissions do not stop reads, nothing to test.
            return;
        }

        let registry = InputMaintenanceRegistry::load(path.clone(), &["provider_a"]).await;
        let write = registry.add("provider_b", None, 2).await;

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(write.is_err(), "state that could not be read must not be replaced");
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"provider_a":{"since":1}}"#);
    }

    #[tokio::test]
    async fn read_only_load_never_touches_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(INPUT_MAINTENANCE_FILE);
        std::fs::write(&path, br#"{"gone":{"since":1},"provider_a":{"since":2}}"#).unwrap();

        let registry = InputMaintenanceRegistry::load_read_only(path.clone()).await;

        assert!(registry.is_in_maintenance("provider_a"));
        assert!(registry.add("provider_b", None, 3).await.is_err(), "a read-only registry refuses writes");
        assert_eq!(std::fs::read(&path).unwrap(), br#"{"gone":{"since":1},"provider_a":{"since":2}}"#);

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{ not json").unwrap();
        let registry = InputMaintenanceRegistry::load_read_only(corrupt.clone()).await;
        assert!(registry.snapshot().is_empty());
        assert!(corrupt.exists(), "a read-only load never moves the file aside");
    }

    #[tokio::test]
    async fn in_memory_registry_works_without_any_file() {
        let registry = InputMaintenanceRegistry::in_memory();
        assert!(registry.add("provider_a", None, 1).await.unwrap().is_some());
        assert!(registry.is_in_maintenance("provider_a"));
        assert!(registry.remove_all().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn gate_trait_delegates() {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(registry_in(&dir));
        registry.add("provider_a", None, 1).await.unwrap();
        let gate: Arc<dyn InputMaintenanceGate> = registry;
        assert!(gate.is_in_maintenance("provider_a"));
    }
}
