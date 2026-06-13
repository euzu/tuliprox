use super::{CacheAccessState, ProxySessionId, TransientObjectCacheKey};
use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;

type HmacSha256 = Hmac<Sha256>;
const TRANSIENT_RESOURCE_ID_LEN: usize = 16;
const DEFAULT_TRANSIENT_RESOURCE_TTL_MS: u64 = 300_000;

/// Opaque ID for a transient passthrough resource.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TransientResourceId(pub String);

/// Builds a deterministic opaque transient resource ID from the resolved origin URI.
///
/// # Panics
///
/// Panics only if the HMAC implementation rejects the rewrite secret. HMAC-SHA256 accepts keys of any length.
pub fn build_transient_resource_id(
    resolved_origin_uri: &str,
    reverse_proxy_rewrite_secret: &[u8],
) -> TransientResourceId {
    let mut mac = HmacSha256::new_from_slice(reverse_proxy_rewrite_secret)
        .expect("HMAC-SHA256 accepts rewrite secrets of any length");
    mac.update(resolved_origin_uri.as_bytes());
    let digest = mac.finalize().into_bytes();
    let token = general_purpose::URL_SAFE_NO_PAD.encode(digest);
    TransientResourceId(token[..TRANSIENT_RESOURCE_ID_LEN].to_string())
}

/// Transient origin resource category used for direct passthrough streaming.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransientResourceKind {
    Segment,
    Key,
    Map,
    Other,
}

/// Per-session mapping from an opaque transient ID to one resolved origin resource.
#[derive(Clone, Eq, PartialEq)]
pub struct TransientResourceRef {
    pub id: TransientResourceId,
    pub kind: TransientResourceKind,
    pub resolved_origin_uri: String,
    pub content_type_hint: Option<String>,
    pub file_ext_hint: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub access: Arc<CacheAccessState>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransientObjectCacheStatus {
    Fetching { started_at_ms: u64 },
    Ready { content_length: u64, ready_at_ms: u64 },
    Failed { failed_at_ms: u64 },
}

#[derive(Clone, Eq, PartialEq)]
pub struct TransientObjectCacheEntry {
    pub key: TransientObjectCacheKey,
    pub status: TransientObjectCacheStatus,
    pub content_type: String,
    pub created_at_ms: u64,
    pub last_accessed_at_ms: u64,
    pub expires_at_ms: u64,
    pub access: Arc<CacheAccessState>,
}

#[derive(Clone)]
pub enum TransientObjectFetchDecision {
    Ready,
    Fetch(TransientObjectCacheKey),
    Wait(Arc<Notify>),
}

impl TransientObjectCacheEntry {
    fn new_fetching(key: TransientObjectCacheKey, now_ms: u64, expires_at_ms: u64, content_type: String) -> Self {
        Self {
            key,
            status: TransientObjectCacheStatus::Fetching { started_at_ms: now_ms },
            content_type,
            created_at_ms: now_ms,
            last_accessed_at_ms: now_ms,
            expires_at_ms,
            access: Arc::new(CacheAccessState::new()),
        }
    }

    pub fn is_ready_at(&self, now_ms: u64) -> bool {
        matches!(self.status, TransientObjectCacheStatus::Ready { .. }) && self.expires_at_ms >= now_ms
    }

    pub fn ready_content_length(&self) -> Option<u64> {
        match self.status {
            TransientObjectCacheStatus::Ready { content_length, .. } => Some(content_length),
            TransientObjectCacheStatus::Fetching { .. } | TransientObjectCacheStatus::Failed { .. } => None,
        }
    }
}

impl fmt::Debug for TransientObjectCacheEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientObjectCacheEntry")
            .field("key", &self.key)
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("created_at_ms", &self.created_at_ms)
            .field("last_accessed_at_ms", &self.last_accessed_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("active_readers", &self.access.active_readers())
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TransientObjectRemoval {
    pub key: TransientObjectCacheKey,
    pub content_length: u64,
}

impl TransientResourceRef {
    pub fn new(
        kind: TransientResourceKind,
        resolved_origin_uri: impl Into<String>,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
        ttl_ms: u64,
        file_ext_hint: Option<String>,
    ) -> Self {
        let resolved_origin_uri = resolved_origin_uri.into();
        let id = build_transient_resource_id(&resolved_origin_uri, reverse_proxy_rewrite_secret);
        Self {
            id,
            kind,
            resolved_origin_uri,
            content_type_hint: file_ext_hint
                .as_deref()
                .and_then(default_content_type_for_transient_ext)
                .map(str::to_string),
            file_ext_hint,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            access: Arc::new(CacheAccessState::new()),
        }
    }

    pub fn is_valid_at(&self, now_ms: u64) -> bool { now_ms <= self.expires_at_ms }

    fn refresh_from(&mut self, next: TransientResourceRef) {
        self.kind = next.kind;
        self.resolved_origin_uri = next.resolved_origin_uri;
        self.content_type_hint = next.content_type_hint;
        self.file_ext_hint = next.file_ext_hint;
        self.expires_at_ms = next.expires_at_ms;
    }

    pub fn active_readers(&self) -> u32 { self.access.active_readers() }
}

impl fmt::Debug for TransientResourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientResourceRef")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("resolved_origin_uri", &"<redacted>")
            .field("content_type_hint", &self.content_type_hint)
            .field("file_ext_hint", &self.file_ext_hint)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("active_readers", &self.access.active_readers())
            .finish()
    }
}

/// Per-session transient passthrough manifest and resource mappings.
#[derive(Clone)]
pub struct TransientPassthroughState {
    pub resources: HashMap<TransientResourceId, TransientResourceRef>,
    pub object_cache: HashMap<TransientObjectCacheKey, TransientObjectCacheEntry>,
    object_fetch_notifiers: HashMap<TransientObjectCacheKey, Arc<Notify>>,
    pub last_manifest_body: Option<String>,
    pub last_manifest_rendered_at_ms: Option<u64>,
    pub resource_ttl_ms: u64,
}

impl TransientPassthroughState {
    pub fn new(resource_ttl_ms: u64) -> Self {
        Self {
            resources: HashMap::new(),
            object_cache: HashMap::new(),
            object_fetch_notifiers: HashMap::new(),
            last_manifest_body: None,
            last_manifest_rendered_at_ms: None,
            resource_ttl_ms,
        }
    }

    pub fn set_resource_ttl_ms(&mut self, resource_ttl_ms: u64) { self.resource_ttl_ms = resource_ttl_ms; }

    pub fn replace_manifest(&mut self, body: String, rendered_at_ms: u64) {
        self.last_manifest_body = Some(body);
        self.last_manifest_rendered_at_ms = Some(rendered_at_ms);
    }

    pub fn upsert_resources<I>(&mut self, resources: I)
    where
        I: IntoIterator<Item = TransientResourceRef>,
    {
        for resource in resources {
            match self.resources.get_mut(&resource.id) {
                Some(existing) => existing.refresh_from(resource),
                None => {
                    self.resources.insert(resource.id.clone(), resource);
                }
            }
        }
    }

    pub fn get_valid_resource(
        &mut self,
        resource_id: &TransientResourceId,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.prune_expired(now_ms);
        self.resources.get(resource_id).cloned()
    }

    pub fn prune_expired(&mut self, now_ms: u64) {
        let protected = self.protected_manifest_resource_ids();
        self.prune_expired_except(now_ms, &protected);
    }

    pub fn prune_expired_except(&mut self, now_ms: u64, protected: &HashSet<TransientResourceId>) {
        self.resources
            .retain(|id, resource| protected.contains(id) || resource.is_valid_at(now_ms) || resource.active_readers() > 0);
    }

    pub fn protected_manifest_resource_ids(&self) -> HashSet<TransientResourceId> {
        self.last_manifest_body.as_deref().map_or_else(HashSet::new, extract_transient_resource_ids)
    }

    pub fn active_resource_readers(&self) -> u32 {
        self.resources.values().map(TransientResourceRef::active_readers).sum()
    }

    pub fn has_active_resource_readers(&self) -> bool { self.active_resource_readers() > 0 }

    pub fn transient_object_key(
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
        file_ext: impl Into<String>,
    ) -> TransientObjectCacheKey {
        TransientObjectCacheKey::new(proxy_session_id.clone(), resource_id.clone(), file_ext)
    }

    pub fn ready_object(
        &mut self,
        key: &TransientObjectCacheKey,
        now_ms: u64,
    ) -> Option<TransientObjectCacheEntry> {
        let entry = self.object_cache.get_mut(key)?;
        if !entry.is_ready_at(now_ms) {
            return None;
        }
        entry.last_accessed_at_ms = now_ms;
        entry.access.reader_started(now_ms);
        entry.access.reader_finished();
        Some(entry.clone())
    }

    pub fn begin_object_fetch(
        &mut self,
        proxy_session_id: &ProxySessionId,
        resource: &TransientResourceRef,
        file_ext: &str,
        now_ms: u64,
        cache_duration_ms: u64,
    ) -> TransientObjectFetchDecision {
        let key = Self::transient_object_key(proxy_session_id, &resource.id, file_ext.to_string());
        match self.object_cache.get(&key) {
            Some(entry) if entry.is_ready_at(now_ms) => return TransientObjectFetchDecision::Ready,
            Some(entry) if matches!(entry.status, TransientObjectCacheStatus::Fetching { .. }) => {
                let notifier = self.object_fetch_notifiers.entry(key).or_insert_with(|| Arc::new(Notify::new())).clone();
                return TransientObjectFetchDecision::Wait(notifier);
            }
            Some(_) | None => {}
        }
        let expires_at_ms = now_ms.saturating_add(cache_duration_ms).min(resource.expires_at_ms);
        let content_type = resource.content_type_hint.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        self.object_cache
            .insert(key.clone(), TransientObjectCacheEntry::new_fetching(key.clone(), now_ms, expires_at_ms, content_type));
        self.object_fetch_notifiers.entry(key.clone()).or_insert_with(|| Arc::new(Notify::new()));
        TransientObjectFetchDecision::Fetch(key)
    }

    pub fn mark_object_ready(
        &mut self,
        key: &TransientObjectCacheKey,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) {
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        match self.object_cache.get_mut(key) {
            Some(entry) => {
                entry.status = TransientObjectCacheStatus::Ready { content_length, ready_at_ms: now_ms };
                entry.content_type = content_type;
                entry.last_accessed_at_ms = now_ms;
                entry.expires_at_ms = expires_at_ms;
            }
            None => {
                self.object_cache.insert(
                    key.clone(),
                    TransientObjectCacheEntry {
                        key: key.clone(),
                        status: TransientObjectCacheStatus::Ready { content_length, ready_at_ms: now_ms },
                        content_type,
                        created_at_ms: now_ms,
                        last_accessed_at_ms: now_ms,
                        expires_at_ms,
                        access: Arc::new(CacheAccessState::new()),
                    },
                );
            }
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
    }

    pub fn mark_object_failed(&mut self, key: &TransientObjectCacheKey, now_ms: u64) {
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        if let Some(entry) = self.object_cache.get_mut(key) {
            entry.status = TransientObjectCacheStatus::Failed { failed_at_ms: now_ms };
            entry.last_accessed_at_ms = now_ms;
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
    }

    pub fn ready_object_cache_size(&self) -> u64 {
        self.object_cache.values().filter_map(TransientObjectCacheEntry::ready_content_length).sum()
    }

    pub fn prune_expired_objects(&mut self, now_ms: u64) -> Vec<TransientObjectRemoval> {
        let keys = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                if entry.access.active_readers() == 0 && entry.expires_at_ms < now_ms {
                    return entry.ready_content_length().map(|content_length| (key.clone(), content_length));
                }
                None
            })
            .collect::<Vec<_>>();
        self.remove_object_keys(keys)
    }

    pub fn remove_oldest_ready_object(&mut self) -> Option<TransientObjectRemoval> {
        let candidate = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                if entry.access.active_readers() == 0 {
                    return entry.ready_content_length().map(|content_length| {
                        (key.clone(), content_length, entry.last_accessed_at_ms, entry.created_at_ms)
                    });
                }
                None
            })
            .min_by_key(|(_, _, last_accessed_at_ms, created_at_ms)| (*last_accessed_at_ms, *created_at_ms))?;
        self.remove_object_keys(vec![(candidate.0, candidate.1)]).into_iter().next()
    }

    fn remove_object_keys(&mut self, keys: Vec<(TransientObjectCacheKey, u64)>) -> Vec<TransientObjectRemoval> {
        keys.into_iter()
            .filter_map(|(key, content_length)| {
                self.object_cache.remove(&key)?;
                Some(TransientObjectRemoval { key, content_length })
            })
            .collect()
    }
}

impl Default for TransientPassthroughState {
    fn default() -> Self { Self::new(DEFAULT_TRANSIENT_RESOURCE_TTL_MS) }
}

impl fmt::Debug for TransientPassthroughState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientPassthroughState")
            .field("resources_len", &self.resources.len())
            .field("object_cache_len", &self.object_cache.len())
            .field("object_fetch_notifiers_len", &self.object_fetch_notifiers.len())
            .field("last_manifest_body_len", &self.last_manifest_body.as_ref().map(String::len))
            .field("last_manifest_rendered_at_ms", &self.last_manifest_rendered_at_ms)
            .field("resource_ttl_ms", &self.resource_ttl_ms)
            .finish()
    }
}

fn extract_transient_resource_ids(body: &str) -> HashSet<TransientResourceId> {
    body.split("/r/")
        .skip(1)
        .filter_map(|tail| {
            let file_name: String = tail
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
                .collect();
            let (resource_id, extension) = file_name.rsplit_once('.')?;
            if resource_id.is_empty() || extension.is_empty() {
                return None;
            }
            Some(TransientResourceId(resource_id.to_string()))
        })
        .collect()
}

/// Empty runtime store for future transient passthrough resources.
#[derive(Default)]
pub struct TransientResourceStore;

impl TransientResourceStore {
    pub fn new() -> Self { Self }
}

fn default_content_type_for_transient_ext(extension: &str) -> Option<&'static str> {
    match extension {
        "ts" => Some("video/MP2T"),
        "m4a" => Some("audio/mp4"),
        "m4s" | "mp4" => Some("video/mp4"),
        "key" => Some("application/octet-stream"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_transient_resource_id, TransientPassthroughState, TransientResourceKind, TransientResourceRef};

    #[test]
    fn transient_resource_id_is_stable_and_opaque() {
        let first = build_transient_resource_id("http://origin.example.com/live/key.bin", b"secret");
        let second = build_transient_resource_id("http://origin.example.com/live/key.bin", b"secret");
        let other = build_transient_resource_id("http://origin.example.com/live/seg.ts", b"secret");

        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(first.0.len(), 16);
        assert!(!first.0.contains("origin"));
    }

    #[test]
    fn transient_state_updates_existing_resource_ttl() {
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            10,
            100,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        let updated = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            20,
            200,
            Some("ts".to_string()),
        );
        state.upsert_resources([updated]);

        assert_eq!(state.resources.len(), 1);
        assert_eq!(state.get_valid_resource(&resource_id, 150).expect("resource remains valid").expires_at_ms, 220);
        assert!(state.get_valid_resource(&resource_id, 221).is_none());
    }

    #[test]
    fn transient_prune_keeps_resources_referenced_by_last_manifest() {
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        state.replace_manifest(
            format!("#EXTM3U\n#EXTINF:1,\n/proxy/hls/live/session/lease/r/{}.ts\n", resource_id.0),
            0,
        );

        state.prune_expired(20);

        assert!(state.resources.contains_key(&resource_id));
    }
}
