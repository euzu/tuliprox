use super::{HlsOriginSource, HlsSession, HlsSessionKey, ProxySessionId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};
use tokio::sync::{RwLock, RwLockReadGuard};

pub type HlsSessionHandle = Arc<RwLock<HlsSession>>;

/// Monotonic identity of one concrete session handle created by the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HlsSessionIncarnation(u64);

#[cfg(any(test, feature = "test-support"))]
impl HlsSessionIncarnation {
    pub const fn for_test(generation: u64) -> Self { Self(generation) }
}

/// Result of a non-blocking session-incarnation transaction.
pub enum HlsCurrentProxySessionAccess<R> {
    Acquired(R),
    Superseded,
    LockBusy,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSessionStoreOutcome {
    Created,
    Reused,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsExpiredSessionReason {
    SessionIdleTimeout,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsExpiredSessionMarker {
    pub proxy_session_id: ProxySessionId,
    pub session_key: HlsSessionKey,
    pub username: Option<String>,
    pub expired_at_ms: u64,
    pub reason: HlsExpiredSessionReason,
}

/// In-memory lookup store for HLS sessions by stable key and public proxy ID.
#[derive(Default)]
pub struct HlsSessionStore {
    indexes: RwLock<SessionIndexes>,
    incarnations: Mutex<HlsSessionIncarnationRegistry>,
}

#[derive(Default)]
struct SessionIndexes {
    by_key: HashMap<HlsSessionKey, HlsSessionHandle>,
    by_proxy_session_id: HashMap<ProxySessionId, HlsSessionHandle>,
    expired_by_proxy_session_id: HashMap<ProxySessionId, HlsExpiredSessionMarker>,
}

#[derive(Default)]
struct HlsSessionIncarnationRegistry {
    next: u64,
    // Weak entries retain ordering only for still-live handles and are pruned
    // on every lookup/registration, so removed session history cannot leak.
    entries: Vec<HlsSessionIncarnationEntry>,
}

struct HlsSessionIncarnationEntry {
    incarnation: HlsSessionIncarnation,
    session: Weak<RwLock<HlsSession>>,
}

/// Keeps one exact proxy-session incarnation indexed for a cross-store transaction.
pub(crate) struct HlsCurrentProxySessionGuard<'a> {
    _indexes: RwLockReadGuard<'a, SessionIndexes>,
    session: HlsSessionHandle,
}

impl HlsCurrentProxySessionGuard<'_> {
    pub(crate) fn session(&self) -> &HlsSessionHandle { &self.session }
}

#[cfg(any(test, feature = "test-support"))]
pub struct HlsSessionIndexWriteGuardForTest<'a> {
    _guard: tokio::sync::RwLockWriteGuard<'a, SessionIndexes>,
}

impl HlsSessionStore {
    pub fn new() -> Self { Self::default() }

    pub async fn get_by_key(&self, key: &HlsSessionKey) -> Option<HlsSessionHandle> {
        self.indexes.read().await.by_key.get(key).map(Arc::clone)
    }

    pub async fn get_by_proxy_session_id(&self, proxy_session_id: &ProxySessionId) -> Option<HlsSessionHandle> {
        self.indexes.read().await.by_proxy_session_id.get(proxy_session_id).map(Arc::clone)
    }

    /// Holds the current proxy-session identity stable until the returned guard is dropped.
    pub(crate) async fn hold_current_proxy_session(
        &self,
        proxy_session_id: &ProxySessionId,
    ) -> Option<HlsCurrentProxySessionGuard<'_>> {
        let indexes = self.indexes.read().await;
        let session = indexes.by_proxy_session_id.get(proxy_session_id).map(Arc::clone)?;
        Some(HlsCurrentProxySessionGuard { _indexes: indexes, session })
    }

    pub fn session_incarnation(&self, session: &HlsSessionHandle) -> Option<HlsSessionIncarnation> {
        let mut incarnations = self.lock_incarnations();
        incarnations.entries.retain(|entry| entry.session.strong_count() > 0);
        incarnations.entries.iter().find_map(|entry| {
            entry.session.upgrade().filter(|candidate| Arc::ptr_eq(candidate, session)).map(|_| entry.incarnation)
        })
    }

    /// Non-blocking transaction for deadline-bound state changes. The index
    /// guard is held through `operation`; the closure must not await or perform
    /// I/O.
    pub fn try_with_current_proxy_session<R>(
        &self,
        proxy_session_id: &ProxySessionId,
        expected: &HlsSessionHandle,
        operation: impl FnOnce() -> R,
    ) -> HlsCurrentProxySessionAccess<R> {
        let Ok(indexes) = self.indexes.try_read() else {
            return HlsCurrentProxySessionAccess::LockBusy;
        };
        let Some(current) = indexes.by_proxy_session_id.get(proxy_session_id) else {
            return HlsCurrentProxySessionAccess::Superseded;
        };
        if !Arc::ptr_eq(current, expected) {
            return HlsCurrentProxySessionAccess::Superseded;
        }
        let result = operation();
        drop(indexes);
        HlsCurrentProxySessionAccess::Acquired(result)
    }

    pub async fn list_sessions(&self) -> Vec<HlsSessionHandle> {
        self.indexes.read().await.by_key.values().map(Arc::clone).collect()
    }

    pub async fn get_or_create_session(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> HlsSessionHandle {
        self.get_or_create_session_with_outcome(key, reverse_proxy_rewrite_secret, now_ms).await.0
    }

    pub async fn get_or_create_session_with_outcome(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let origin_source = HlsOriginSource::from_session_key(&key);
        self.get_or_create_session_with_source_and_outcome(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
            .await
    }

    pub async fn get_or_create_session_with_source_and_outcome(
        &self,
        key: HlsSessionKey,
        origin_source: HlsOriginSource,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let mut indexes = self.indexes.write().await;
        if let Some(session) = indexes.by_key.get(&key) {
            return (Arc::clone(session), HlsSessionStoreOutcome::Reused);
        }

        let session =
            HlsSession::new_with_origin_source(key.clone(), origin_source, reverse_proxy_rewrite_secret, now_ms);
        let proxy_session_id = session.proxy_session_id.clone();
        let session = Arc::new(RwLock::new(session));
        let _incarnation = self.register_session_incarnation(&session);
        indexes.by_key.insert(key, Arc::clone(&session));
        indexes.expired_by_proxy_session_id.remove(&proxy_session_id);
        indexes.by_proxy_session_id.insert(proxy_session_id, Arc::clone(&session));
        (session, HlsSessionStoreOutcome::Created)
    }

    pub async fn remove_session(
        &self,
        key: &HlsSessionKey,
        _proxy_session_id: &ProxySessionId,
    ) -> Option<HlsSessionHandle> {
        let mut indexes = self.indexes.write().await;
        let (proxy_session_id, removed) = remove_indexed_session(&mut indexes, key)?;
        indexes.expired_by_proxy_session_id.remove(&proxy_session_id);
        Some(removed)
    }

    pub async fn remove_session_marking_expired(
        &self,
        key: &HlsSessionKey,
        _proxy_session_id: &ProxySessionId,
        now_ms: u64,
        reason: HlsExpiredSessionReason,
        username: Option<String>,
    ) -> Option<HlsSessionHandle> {
        let mut indexes = self.indexes.write().await;
        let (proxy_session_id, removed) = remove_indexed_session(&mut indexes, key)?;
        indexes.expired_by_proxy_session_id.insert(
            proxy_session_id.clone(),
            HlsExpiredSessionMarker {
                proxy_session_id: proxy_session_id.clone(),
                session_key: key.clone(),
                username,
                expired_at_ms: now_ms,
                reason,
            },
        );
        Some(removed)
    }

    pub async fn expired_session_marker(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        retention_ms: u64,
    ) -> Option<HlsExpiredSessionMarker> {
        let mut indexes = self.indexes.write().await;
        let marker = indexes.expired_by_proxy_session_id.get(proxy_session_id)?;
        if marker.expired_at_ms.saturating_add(retention_ms) <= now_ms {
            indexes.expired_by_proxy_session_id.remove(proxy_session_id);
            return None;
        }
        Some(marker.clone())
    }

    pub async fn update_expired_session_marker_username(
        &self,
        proxy_session_id: &ProxySessionId,
        username: Option<String>,
    ) {
        let Some(username) = username else {
            return;
        };
        if let Some(marker) = self.indexes.write().await.expired_by_proxy_session_id.get_mut(proxy_session_id) {
            marker.username.get_or_insert(username);
        }
    }

    pub async fn clear(&self) {
        let mut indexes = self.indexes.write().await;
        indexes.by_key.clear();
        indexes.by_proxy_session_id.clear();
        indexes.expired_by_proxy_session_id.clear();
        self.lock_incarnations().entries.clear();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn hold_index_write_for_test(&self) -> HlsSessionIndexWriteGuardForTest<'_> {
        HlsSessionIndexWriteGuardForTest { _guard: self.indexes.write().await }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn index_write_is_blocked_for_test(&self) -> bool { self.indexes.try_write().is_err() }

    #[cfg(any(test, feature = "test-support"))]
    async fn proxy_session_index_len(&self) -> usize { self.indexes.read().await.by_proxy_session_id.len() }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn len(&self) -> usize { self.indexes.read().await.by_key.len() }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn is_empty(&self) -> bool { self.indexes.read().await.by_key.is_empty() }

    fn register_session_incarnation(&self, session: &HlsSessionHandle) -> Option<HlsSessionIncarnation> {
        let mut incarnations = self.lock_incarnations();
        incarnations.entries.retain(|entry| entry.session.strong_count() > 0);
        let generation = incarnations.next.checked_add(1)?;
        incarnations.next = generation;
        let incarnation = HlsSessionIncarnation(generation);
        incarnations.entries.push(HlsSessionIncarnationEntry { incarnation, session: Arc::downgrade(session) });
        Some(incarnation)
    }

    fn lock_incarnations(&self) -> MutexGuard<'_, HlsSessionIncarnationRegistry> {
        self.incarnations.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn remove_indexed_session(
    indexes: &mut SessionIndexes,
    key: &HlsSessionKey,
) -> Option<(ProxySessionId, HlsSessionHandle)> {
    let session = indexes.by_key.get(key)?;
    let proxy_session_id = indexes
        .by_proxy_session_id
        .iter()
        .find_map(|(proxy_session_id, indexed)| Arc::ptr_eq(session, indexed).then(|| proxy_session_id.clone()))?;
    let removed = indexes.by_key.remove(key)?;
    indexes.by_proxy_session_id.remove(&proxy_session_id);
    Some((proxy_session_id, removed))
}

#[cfg(test)]
mod tests {
    use super::{HlsCurrentProxySessionAccess, HlsExpiredSessionReason, HlsSessionStore};
    use crate::{HlsSessionKey, ProxySessionId};
    use std::sync::Arc;

    #[tokio::test]
    async fn get_or_create_session_reuses_existing_session_for_same_key() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "12345");

        let first = store.get_or_create_session(key.clone(), b"0011223344556677", 100).await;
        let second = store.get_or_create_session(key, b"0011223344556677", 200).await;

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.read().await.last_client_access_at_ms, 100);
    }

    #[tokio::test]
    async fn proxy_session_id_lookup_finds_created_session() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "12345");
        let created = store.get_or_create_session(key, b"0011223344556677", 100).await;
        let proxy_session_id = created.read().await.proxy_session_id.clone();

        let found = store
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("session should be indexed by proxy_session_id");

        assert!(Arc::ptr_eq(&created, &found));
    }

    #[tokio::test]
    async fn hls_terminal_commit_session_index_check_reports_lock_busy_without_waiting() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "terminal-commit");
        let session = store.get_or_create_session(key, b"0011223344556677", 100).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let index_guard = store.indexes.write().await;

        assert!(matches!(
            store.try_with_current_proxy_session(&proxy_session_id, &session, || ()),
            HlsCurrentProxySessionAccess::LockBusy
        ));
        drop(index_guard);
    }

    #[tokio::test]
    async fn remove_session_marking_expired_retains_marker_until_recreated_or_expired() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "12345");
        let created = store.get_or_create_session(key.clone(), b"0011223344556677", 100).await;
        let proxy_session_id = created.read().await.proxy_session_id.clone();

        let removed = store
            .remove_session_marking_expired(
                &key,
                &proxy_session_id,
                1_000,
                HlsExpiredSessionReason::SessionIdleTimeout,
                Some("viewer".to_string()),
            )
            .await;
        assert!(removed.is_some());

        let marker = store
            .expired_session_marker(&proxy_session_id, 1_500, 10_000)
            .await
            .expect("expired marker should remain within retention");
        assert_eq!(marker.username.as_deref(), Some("viewer"));
        assert_eq!(marker.reason, HlsExpiredSessionReason::SessionIdleTimeout);

        let expired = store.expired_session_marker(&proxy_session_id, 11_000, 10_000).await;
        assert!(expired.is_none(), "expired marker should be pruned after retention");

        let recreated = store.get_or_create_session(key, b"0011223344556677", 12_000).await;
        assert_eq!(recreated.read().await.proxy_session_id, proxy_session_id);
        assert!(store.expired_session_marker(&proxy_session_id, 12_100, 10_000).await.is_none());
    }

    #[tokio::test]
    async fn remove_session_cleans_indexes_by_actual_session_id() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "12345");
        let created = store.get_or_create_session(key.clone(), b"0011223344556677", 100).await;
        let proxy_session_id = created.read().await.proxy_session_id.clone();
        let stale_proxy_session_id = ProxySessionId("stale".to_string());

        let removed = store.remove_session(&key, &stale_proxy_session_id).await;

        assert!(removed.is_some());
        assert!(store.get_by_proxy_session_id(&proxy_session_id).await.is_none());
        assert!(store.get_by_proxy_session_id(&stale_proxy_session_id).await.is_none());
    }

    #[tokio::test]
    async fn remove_session_marking_expired_uses_actual_session_id_for_marker() {
        let store = HlsSessionStore::new();
        let key = HlsSessionKey::new(1, "12345");
        let created = store.get_or_create_session(key.clone(), b"0011223344556677", 100).await;
        let proxy_session_id = created.read().await.proxy_session_id.clone();
        let stale_proxy_session_id = ProxySessionId("stale".to_string());

        let removed = store
            .remove_session_marking_expired(
                &key,
                &stale_proxy_session_id,
                1_000,
                HlsExpiredSessionReason::SessionIdleTimeout,
                None,
            )
            .await;

        assert!(removed.is_some());
        assert!(store.expired_session_marker(&proxy_session_id, 1_500, 10_000).await.is_some());
        assert!(store.expired_session_marker(&stale_proxy_session_id, 1_500, 10_000).await.is_none());
    }

    #[tokio::test]
    async fn parallel_get_or_create_session_creates_single_index_entry() {
        let store = Arc::new(HlsSessionStore::new());
        let key = HlsSessionKey::new(1, "12345");
        let mut tasks = Vec::new();

        for now_ms in 100..108 {
            let store = Arc::clone(&store);
            let key = key.clone();
            tasks
                .push(tokio::spawn(async move { store.get_or_create_session(key, b"0011223344556677", now_ms).await }));
        }

        let first = tasks.remove(0).await.expect("task should not panic");
        for task in tasks {
            let session = task.await.expect("task should not panic");
            assert!(Arc::ptr_eq(&first, &session));
        }

        assert_eq!(store.proxy_session_index_len().await, 1);
    }

    #[tokio::test]
    async fn removal_does_not_remove_a_concurrently_recreated_session() {
        let store = Arc::new(HlsSessionStore::new());
        let key = HlsSessionKey::new(1, "12345");
        let old = store.get_or_create_session(key.clone(), b"0011223344556677", 100).await;
        let proxy_session_id = old.read().await.proxy_session_id.clone();
        let old_guard = old.write().await;
        let removal_store = Arc::clone(&store);
        let removal_key = key.clone();
        let removal_proxy_session_id = proxy_session_id.clone();
        let removal = tokio::spawn(async move {
            removal_store
                .remove_session_marking_expired(
                    &removal_key,
                    &removal_proxy_session_id,
                    1_000,
                    HlsExpiredSessionReason::SessionIdleTimeout,
                    None,
                )
                .await
        });

        while store.get_by_key(&key).await.is_some() {
            tokio::task::yield_now().await;
        }
        let recreated = store.get_or_create_session(key, b"0011223344556677", 1_001).await;
        drop(old_guard);
        assert!(matches!(removal.await, Ok(Some(_))));

        let indexed = store.get_by_proxy_session_id(&proxy_session_id).await;
        assert!(indexed.as_ref().is_some_and(|session| Arc::ptr_eq(session, &recreated)));
        assert!(store.expired_session_marker(&proxy_session_id, 1_002, 10_000).await.is_none());
    }
}
