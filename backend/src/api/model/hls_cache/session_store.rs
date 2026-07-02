use super::{HlsOriginSource, HlsSession, HlsSessionKey, ProxySessionId};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

pub type HlsSessionHandle = Arc<RwLock<HlsSession>>;

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
}

#[derive(Default)]
struct SessionIndexes {
    by_key: HashMap<HlsSessionKey, HlsSessionHandle>,
    by_proxy_session_id: HashMap<ProxySessionId, HlsSessionHandle>,
    expired_by_proxy_session_id: HashMap<ProxySessionId, HlsExpiredSessionMarker>,
}

impl HlsSessionStore {
    pub fn new() -> Self { Self::default() }

    pub async fn get_by_key(&self, key: &HlsSessionKey) -> Option<HlsSessionHandle> {
        self.indexes.read().await.by_key.get(key).map(Arc::clone)
    }

    pub async fn get_by_proxy_session_id(&self, proxy_session_id: &ProxySessionId) -> Option<HlsSessionHandle> {
        self.indexes.read().await.by_proxy_session_id.get(proxy_session_id).map(Arc::clone)
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
        let removed = {
            let mut indexes = self.indexes.write().await;
            indexes.by_key.remove(key)?
        };
        let proxy_session_id = removed.read().await.proxy_session_id.clone();
        let mut indexes = self.indexes.write().await;
        indexes.by_proxy_session_id.remove(&proxy_session_id);
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
        let removed = {
            let mut indexes = self.indexes.write().await;
            indexes.by_key.remove(key)?
        };
        let proxy_session_id = removed.read().await.proxy_session_id.clone();
        let mut indexes = self.indexes.write().await;
        indexes.by_proxy_session_id.remove(&proxy_session_id);
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
    }

    #[cfg(test)]
    async fn proxy_session_index_len(&self) -> usize { self.indexes.read().await.by_proxy_session_id.len() }

    #[cfg(test)]
    pub async fn len(&self) -> usize { self.indexes.read().await.by_key.len() }

    #[cfg(test)]
    pub async fn is_empty(&self) -> bool { self.indexes.read().await.by_key.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::{HlsExpiredSessionReason, HlsSessionStore};
    use crate::api::model::{HlsSessionKey, ProxySessionId};
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
}
