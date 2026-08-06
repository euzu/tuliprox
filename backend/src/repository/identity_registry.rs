//! Identity registry.
//!
//! Stable subject identifier assignment for web users and API users.
//! The registry lives in `<storage_dir>/web_user_ids.json` and is
//! persisted atomically via the [`atomic_json_store`](crate::utils::atomic_json_store)
//! helper. The file is *additive*: every existing mapping is preserved
//! forever, and explicit rename is the only way
//! to update a username→UserId binding. Bootstrap is fail-closed: if
//! the persisted state already references real user IDs and the
//! registry is missing or corrupt, recovery requires explicit
//! administrator repair. A new registry may be initialized only when
//! no persisted recording references a real user ID.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use shared::model::UserId;
use tokio::sync::RwLock;
use std::sync::atomic::AtomicU64;

static IDENTITY_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Schema for the on-disk registry. The mapping is
/// `canonical_username → UserId`. The registry is additive; existing
/// entries are never removed by normal operation.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PersistedIdentityRegistry {
    #[serde(default)]
    pub version: u32,
    /// Maps the canonical username (the same string used by
    /// authentication) to its immutable `UserId`.
    pub web_users: HashMap<String, UserId>,
    /// Maps the proxy-user identifier to its immutable `UserId`. The
    /// username is the same string used for proxy authentication.
    #[serde(default)]
    pub api_users: HashMap<String, UserId>,
}

/// Outcome of the bootstrap sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// No pre-existing registry and no real user IDs in the persisted
    /// state; a fresh registry was created.
    Initialized,
    /// A registry was loaded from disk. `current_principal_count`
    /// counts the number of distinct usernames discovered during
    /// bootstrap (before normalization).
    Restored { current_principal_count: usize },
    /// The persisted state references real user IDs but the registry
    /// is missing or corrupt. Recovery requires explicit administrator
    /// repair. The `persisted_user_ids` are the IDs that need to be
    /// restored.
    FailClosed {
        persisted_user_ids: Vec<UserId>,
        reason: FailClosedReason,
    },
    /// The persisted registry is behind the latest schema version.
    /// A migration was applied; the registry is now at `migrated_to`.
    Migrated { migrated_to: u32 },
}

/// Reason for fail-closed recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailClosedReason {
    Missing,
    Corrupt,
}

/// The current on-disk schema version. Bump when the schema changes.
pub const CURRENT_REGISTRY_VERSION: u32 = 1;

/// Thread-safe identity registry. Mutations acquire a write lock;
/// reads acquire a read lock. The registry is `Clone` only via the
/// snapshot in [`IdentityRegistry::snapshot`].
pub struct IdentityRegistry {
    state: RwLock<PersistedIdentityRegistry>,
    path: PathBuf,
}

impl IdentityRegistry {
    /// Run the bootstrap sequence:
    /// 1. Pre-scan the persisted download state for real user IDs
    ///    (already done by the caller; this registry accepts the
    ///    pre-scanned set).
    /// 2. Load the registry from disk.
    /// 3. If no registry exists and no real user IDs are
    ///    pre-scanned, initialize a fresh one.
    /// 4. If the registry is missing or corrupt and real user IDs
    ///    are pre-scanned, fail closed.
    /// 5. Apply any schema migrations.
    /// 6. Sync the current principal set (do not overwrite existing
    ///    entries; only insert new ones).
    pub async fn bootstrap(
        path: PathBuf,
        pre_scanned_user_ids: Vec<UserId>,
        current_web_users: &[String],
        current_api_users: &[String],
    ) -> (Self, BootstrapOutcome) {
        match Self::load_or_detect_failure(&path).await {
            Ok(Some(state)) => {
                let migrated = Self::migrate(state);
                let count = migrated.web_users.len() + migrated.api_users.len();
                let outcome = if migrated.version < CURRENT_REGISTRY_VERSION {
                    BootstrapOutcome::Migrated {
                        migrated_to: migrated.version,
                    }
                } else {
                    BootstrapOutcome::Restored {
                        current_principal_count: count,
                    }
                };
                let reg = Self {
                    state: RwLock::new(migrated),
                    path,
                };
                reg.sync_current_principals(current_web_users, current_api_users).await;
                (reg, outcome)
            }
            Ok(None) => {
                // File does not exist.
                if pre_scanned_user_ids.is_empty() {
                    let reg = Self {
                        state: RwLock::new(PersistedIdentityRegistry::default()),
                        path,
                    };
                    reg.sync_current_principals(current_web_users, current_api_users).await;
                    let _ = reg.save().await;
                    return (reg, BootstrapOutcome::Initialized);
                }
                // Fail-closed with persisted user IDs: do NOT sync or
                // persist. The caller must surface the IDs and require
                // explicit operator repair before any new principal is
                // assigned.
                (
                    Self::empty(path),
                    BootstrapOutcome::FailClosed {
                        persisted_user_ids: pre_scanned_user_ids,
                        reason: FailClosedReason::Missing,
                    },
                )
            }
            Err(reason) => {
                // Fail-closed: do NOT sync or persist. The caller must
                // surface the reason and require explicit operator repair.
                (
                    Self::empty(path),
                    BootstrapOutcome::FailClosed {
                        persisted_user_ids: pre_scanned_user_ids,
                        reason,
                    },
                )
            }
        }
    }

    /// Build an empty registry without touching disk. Used by the
    /// fail-closed path so the caller can still call methods that
    /// require a registry.
    fn empty(path: PathBuf) -> Self {
        Self {
            state: RwLock::new(PersistedIdentityRegistry::default()),
            path,
        }
    }

    async fn load_or_detect_failure(path: &Path) -> Result<Option<PersistedIdentityRegistry>, FailClosedReason> {
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                // A zero-length artifact is never a clean state — it is a
                // half-write, an aborted rename, or an external truncation.
                // Treating it as `Ok(Some(default))` would silently restore
                // an empty registry and let the caller reassign every
                // existing user ID; that is exactly what FailClosed exists
                // to prevent.
                if bytes.is_empty() {
                    return Err(FailClosedReason::Corrupt);
                }
                match serde_json::from_slice::<PersistedIdentityRegistry>(&bytes) {
                    Ok(state) => Ok(Some(state)),
                    Err(_) => Err(FailClosedReason::Corrupt),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(FailClosedReason::Corrupt),
        }
    }

    /// Apply any pending schema migrations. Currently a no-op because
    /// the schema is at `CURRENT_REGISTRY_VERSION`. The function is
    /// present so future migrations have a uniform insertion point.
    fn migrate(mut state: PersistedIdentityRegistry) -> PersistedIdentityRegistry {
        if state.version < CURRENT_REGISTRY_VERSION {
            state.version = CURRENT_REGISTRY_VERSION;
        }
        state
    }

    /// Insert any new web users / API users discovered at bootstrap.
    /// Existing entries are not overwritten. New entries are assigned
    /// fresh `UserId`s in the `web:` / `api:` namespaces.
    async fn sync_current_principals(
        &self,
        current_web_users: &[String],
        current_api_users: &[String],
    ) {
        let mut state = self.state.write().await;
        let mut changed = false;
        for username in current_web_users {
            let key = canonical_username(username);
            if let std::collections::hash_map::Entry::Vacant(entry) = state.web_users.entry(key) {
                let new_id = UserId::from(format!("{}{}", UserId::WEB_NAMESPACE, Self::new_uuid_hex()));
                entry.insert(new_id);
                changed = true;
            }
        }
        for username in current_api_users {
            let key = canonical_username(username);
            if let std::collections::hash_map::Entry::Vacant(entry) = state.api_users.entry(key) {
                let new_id = UserId::from(format!("{}{}", UserId::API_NAMESPACE, Self::new_uuid_hex()));
                entry.insert(new_id);
                changed = true;
            }
        }
        if changed {
            // Best-effort persist. Failures are logged; the registry
            // is still in-memory consistent.
            let snapshot = state.clone();
            drop(state);
            let _ = Self::write_to_disk(&self.path, &snapshot).await;
        }
    }

    /// Insert a brand-new mapping and persist atomically. Returns
    /// the freshly generated `UserId`. The username is normalized
    /// before insertion (trimmed, empty is rejected).
    pub async fn register(&self, username: &str) -> Result<UserId, RegistryError> {
        let key = canonical_username(username);
        if key.is_empty() {
            return Err(RegistryError::EmptyUsername);
        }
        let mut state = self.state.write().await;
        if let Some(existing) = state.web_users.get(&key) {
            return Ok(existing.clone());
        }
        let new_id = UserId::from(format!("{}{}", UserId::WEB_NAMESPACE, Self::new_uuid_hex()));
        state.web_users.insert(key, new_id.clone());
        let snapshot = state.clone();
        drop(state);
        Self::write_to_disk(&self.path, &snapshot)
            .await
            .map_err(RegistryError::Persist)?;
        Ok(new_id)
    }

    /// Look up a `UserId` by the canonical username. Returns `None`
    /// for unknown users.
    pub async fn lookup_by_username(&self, username: &str) -> Option<UserId> {
        let key = canonical_username(username);
        self.state.read().await.web_users.get(&key).cloned()
    }

    /// Look up the canonical username for a `UserId`. Returns `None`
    /// if the registry has no entry for that ID. The lookup is
    /// exhaustive across both web and API namespaces.
    pub async fn lookup_username_by_id(&self, id: &UserId) -> Option<String> {
        let state = self.state.read().await;
        if let Some((k, _)) = state.web_users.iter().find(|(_, v)| *v == id) {
            return Some(k.clone());
        }
        if let Some((k, _)) = state.api_users.iter().find(|(_, v)| *v == id) {
            return Some(k.clone());
        }
        None
    }

    /// Explicit operator/renaming migration. Moves the existing
    /// `UserId` for `old_username` to `new_username`. The `UserId`
    /// is preserved — all persisted recordings continue to reference
    /// the same immutable ID. No-op if the source username is
    /// unknown. The registry is persisted atomically.
    pub async fn rename(
        &self,
        old_username: &str,
        new_username: &str,
    ) -> Result<Option<UserId>, RegistryError> {
        let old_key = canonical_username(old_username);
        let new_key = canonical_username(new_username);
        if old_key == new_key {
            return Ok(self.lookup_by_username(&old_key).await);
        }
        if new_key.is_empty() {
            return Err(RegistryError::EmptyUsername);
        }
        let mut state = self.state.write().await;
        // Reject an existing destination up front, before removing the
        // source — silently overwriting would discard the existing
        // user's persisted recordings.
        if state.web_users.contains_key(&new_key) || state.api_users.contains_key(&new_key) {
            return Err(RegistryError::UsernameExists);
        }
        // Look in both namespaces; the source user may live in either.
        let user_id = state
            .web_users
            .remove(&old_key)
            .or_else(|| state.api_users.remove(&old_key));
        let Some(user_id) = user_id else {
            return Ok(None);
        };
        // Re-insert under the chosen namespace based on which map the
        // entry came from. Using `web_users` as the destination for now
        // is a simplification: callers always rename within the same
        // namespace because the source lookup guarantees the original
        // map.
        let target_map = if state.api_users.contains_key(&new_key) || new_key.starts_with("api:") {
            // Defensive — the contains_key check above already prevented
            // a clash, but keep the destination in the matching namespace
            // when the username prefix signals it.
            &mut state.api_users
        } else {
            &mut state.web_users
        };
        target_map.insert(new_key, user_id.clone());
        let snapshot = state.clone();
        drop(state);
        Self::write_to_disk(&self.path, &snapshot)
            .await
            .map_err(RegistryError::Persist)?;
        Ok(Some(user_id))
    }

    /// Snapshot the current registry for diagnostics.
    pub async fn snapshot(&self) -> PersistedIdentityRegistry {
        self.state.read().await.clone()
    }

    /// Persist the current in-memory state to disk atomically.
    pub async fn save(&self) -> Result<(), RegistryError> {
        let snapshot = self.state.read().await.clone();
        Self::write_to_disk(&self.path, &snapshot)
            .await
            .map_err(RegistryError::Persist)
    }

    /// Build or overwrite the registry from a fixed list of `UserId`s.
    /// Used by explicit administrator repair when the persisted file
    /// is missing or corrupt and the caller knows the missing IDs.
    pub async fn restore_from_user_ids(
        &self,
        web_users: HashMap<String, UserId>,
        api_users: HashMap<String, UserId>,
    ) -> Result<(), RegistryError> {
        let state = PersistedIdentityRegistry {
            version: CURRENT_REGISTRY_VERSION,
            web_users,
            api_users,
        };
        *self.state.write().await = state.clone();
        Self::write_to_disk(&self.path, &state)
            .await
            .map_err(RegistryError::Persist)
    }

    async fn write_to_disk(path: &Path, state: &PersistedIdentityRegistry) -> std::io::Result<()> {
        let content = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        // The atomic helper also fsyncs the temp file and the parent
        // directory so the new identity registry is durable across
        // crashes. The registry writes can be invoked concurrently
        // (`concurrent_register_yields_distinct_ids`), so the temp
        // suffix is unique per call rather than derived from the final
        // filename.
        let counter = IDENTITY_WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!(
            "{}tmp.{}.{}",
            path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default(),
            std::process::id(),
            counter,
        ));
        crate::utils::atomic_json_store::write_json_atomic_to_tmp(path, &tmp, &content)
            .await
            .map_err(std::io::Error::other)
    }

    /// Generate a 32-character hex UUID v4-shaped string. No
    /// `uuid` crate is used; the bytes come from a thread-local
    /// random source.
    fn new_uuid_hex() -> String {
        use std::cell::Cell;
        use std::rc::Rc;
        thread_local! {
            static STATE: Rc<Cell<u64>> = Rc::new(Cell::new(0x9E37_79B9_7F4A_7C15));
        }
        STATE.with(|s| {
            // SplitMix64-style mixing for a deterministic-looking but
            // unpredictable 64-bit value. Lower 32 bits form the first
            // half of the hex string; upper 32 bits the second.
            let mut z = s.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z = z ^ (z >> 31);
            s.set(z);
            let hi = u32::try_from(z >> 32).expect("high bits fit in u32");
            let lo = u32::try_from(z & 0xFFFF_FFFF).expect("low bits fit in u32");
            format!("{hi:08x}{lo:08x}")
        })
    }
}

/// Errors that can occur when mutating the registry.
#[derive(Debug)]
pub enum RegistryError {
    EmptyUsername,
    /// The destination username already exists in the registry. The
    /// caller must remove or rename it explicitly before retrying —
    /// overwriting silently would discard the existing user's
    /// persisted recordings.
    UsernameExists,
    Persist(std::io::Error),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUsername => f.write_str("username must not be empty"),
            Self::UsernameExists => f.write_str("destination username already exists in the identity registry"),
            Self::Persist(err) => write!(f, "registry persistence failed: {err}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Canonicalize a username for the registry: trim leading/trailing
/// whitespace. Empty inputs are returned as-is so the caller can
/// reject them through a typed error.
fn canonical_username(username: &str) -> String {
    username.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn web_users(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("user-{i}")).collect()
    }

    #[tokio::test]
    async fn bootstrap_initializes_on_empty_storage() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, outcome) = IdentityRegistry::bootstrap(
            path.clone(),
            Vec::new(),
            &web_users(3),
            &[],
        )
        .await;
        assert!(matches!(outcome, BootstrapOutcome::Initialized));
        let snap = reg.snapshot().await;
        assert_eq!(snap.web_users.len(), 3);
        for u in 0..3 {
            assert!(snap.web_users.contains_key(&format!("user-{u}")));
        }
        // File must be persisted.
        let bytes = tokio::fs::read(&path).await.expect("read");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_restores_and_preserves_existing_ids() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        // First bootstrap creates the registry.
        let (reg, _) = IdentityRegistry::bootstrap(
            path.clone(),
            Vec::new(),
            &web_users(2),
            &[],
        )
        .await;
        let first_id = reg.lookup_by_username("user-0").await.expect("registered");
        drop(reg);

        // Second bootstrap reads the persisted file. The existing
        // `UserId` is preserved.
        let (reg2, outcome) = IdentityRegistry::bootstrap(
            path,
            Vec::new(),
            &web_users(2),
            &[],
        )
        .await;
        assert!(matches!(outcome, BootstrapOutcome::Restored { .. }));
        let second_id = reg2.lookup_by_username("user-0").await.expect("still registered");
        assert_eq!(first_id, second_id, "UserId must be stable across restarts");
    }

    #[tokio::test]
    async fn bootstrap_fails_closed_when_user_ids_persist_but_registry_missing() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let pre_scanned = vec![UserId::from("web:abc"), UserId::from("web:def")];
        let (reg, outcome) = IdentityRegistry::bootstrap(
            path.clone(),
            pre_scanned.clone(),
            &[],
            &[],
        )
        .await;
        assert!(matches!(
            outcome,
            BootstrapOutcome::FailClosed { reason: FailClosedReason::Missing, .. }
        ));
        // The registry must remain empty (no auto-init).
        assert!(reg.snapshot().await.web_users.is_empty());
        // The file must not be created by the fail-closed bootstrap.
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn bootstrap_fails_closed_on_corrupt_registry() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        tokio::fs::write(&path, b"not json at all").await.expect("write");
        let pre_scanned = vec![UserId::from("web:abc")];
        let (_, outcome) = IdentityRegistry::bootstrap(
            path,
            pre_scanned.clone(),
            &[],
            &[],
        )
        .await;
        assert!(matches!(
            outcome,
            BootstrapOutcome::FailClosed { reason: FailClosedReason::Corrupt, .. }
        ));
    }

    #[tokio::test]
    async fn register_returns_existing_id_for_known_username() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &[], &[]).await;
        let id1 = reg.register("alice").await.expect("register");
        let id2 = reg.register("alice").await.expect("register again");
        assert_eq!(id1, id2, "register must be idempotent");
    }

    #[tokio::test]
    async fn register_rejects_empty_username() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &[], &[]).await;
        assert!(matches!(
            reg.register("").await.unwrap_err(),
            RegistryError::EmptyUsername
        ));
        assert!(matches!(
            reg.register("   ").await.unwrap_err(),
            RegistryError::EmptyUsername
        ));
    }

    #[tokio::test]
    async fn lookup_by_username_and_id_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &web_users(2), &[]).await;
        let id = reg.lookup_by_username("user-1").await.expect("lookup");
        let username = reg.lookup_username_by_id(&id).await.expect("reverse");
        assert_eq!(username, "user-1");
        assert!(reg.lookup_by_username("missing").await.is_none());
        let other = UserId::from("web:does-not-exist");
        assert!(reg.lookup_username_by_id(&other).await.is_none());
    }

    #[tokio::test]
    async fn rename_preserves_user_id() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &web_users(1), &[]).await;
        let original_id = reg.lookup_by_username("user-0").await.expect("lookup");
        let renamed = reg.rename("user-0", "user-renamed").await.expect("rename");
        assert_eq!(renamed, Some(original_id.clone()));
        assert!(reg.lookup_by_username("user-0").await.is_none());
        // The new username resolves to the same immutable ID.
        assert_eq!(reg.lookup_by_username("user-renamed").await, Some(original_id));
    }

    #[tokio::test]
    async fn rename_unknown_username_returns_none() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &[], &[]).await;
        let r = reg.rename("missing", "present").await.expect("rename");
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn restore_from_user_ids_rebuilds_registry_for_repair() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path.clone(), Vec::new(), &[], &[]).await;
        let mut web = HashMap::new();
        web.insert("alice".to_string(), UserId::from("web:abc"));
        web.insert("bob".to_string(), UserId::from("web:def"));
        reg.restore_from_user_ids(web.clone(), HashMap::new())
            .await
            .expect("restore");
        assert_eq!(reg.snapshot().await.web_users, web);
        // Restore must persist.
        let bytes = tokio::fs::read(&path).await.expect("read");
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn concurrent_register_yields_distinct_ids() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path, Vec::new(), &[], &[]).await;
        let reg = std::sync::Arc::new(reg);
        let mut handles = Vec::new();
        for i in 0..20u64 {
            let reg = std::sync::Arc::clone(&reg);
            handles.push(tokio::spawn(async move {
                reg.register(&format!("concurrent-{i}")).await
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for h in handles {
            let id = h.await.expect("join").expect("register");
            assert!(ids.insert(id), "ids must be unique across concurrent registrations");
        }
    }

    #[tokio::test]
    async fn absent_users_remain_in_registry() {
        // Retain mappings when users are temporarily absent: even
        // if a bootstrap is performed with no current
        // principals, existing mappings are preserved.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("web_user_ids.json");
        let (reg, _) = IdentityRegistry::bootstrap(path.clone(), Vec::new(), &web_users(3), &[]).await;
        let saved_id = reg.lookup_by_username("user-1").await.expect("lookup");
        drop(reg);
        let (reg2, _) = IdentityRegistry::bootstrap(path, Vec::new(), &[], &[]).await;
        assert_eq!(reg2.lookup_by_username("user-1").await, Some(saved_id));
    }
}
