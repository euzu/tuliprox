use super::{HlsOriginSource, HlsSession, HlsSessionKey, ProxySessionId};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

pub type HlsSessionHandle = Arc<RwLock<HlsSession>>;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSessionStoreOutcome {
    Created,
    Reused,
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
        indexes.by_proxy_session_id.insert(proxy_session_id, Arc::clone(&session));
        (session, HlsSessionStoreOutcome::Created)
    }

    pub async fn remove_session(
        &self,
        key: &HlsSessionKey,
        proxy_session_id: &ProxySessionId,
    ) -> Option<HlsSessionHandle> {
        let mut indexes = self.indexes.write().await;
        indexes.by_proxy_session_id.remove(proxy_session_id);
        indexes.by_key.remove(key)
    }

    pub async fn clear(&self) {
        let mut indexes = self.indexes.write().await;
        indexes.by_key.clear();
        indexes.by_proxy_session_id.clear();
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
    use super::HlsSessionStore;
    use crate::api::model::HlsSessionKey;
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
