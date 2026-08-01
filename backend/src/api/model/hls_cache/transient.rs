use super::{CacheAccessState, HlsSession, ProxySessionId, TransientObjectCacheKey};
use axum::http::StatusCode;
use base64::{engine::general_purpose, Engine as _};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;

const TRANSIENT_RESOURCE_ID_LEN: usize = 16;
const DEFAULT_TRANSIENT_RESOURCE_TTL_MS: u64 = 300_000;
const MAX_FAILED_TRANSIENT_OBJECT_ENTRIES: usize = 256;
const TRANSIENT_RESOURCE_ID_KEY_CONTEXT: &str = "tuliprox:hls-cache:transient-resource-id-key:v1";

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct TransientResourceRevision(u64);

/// Opaque ID for a transient passthrough resource.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TransientResourceId(pub String);

/// Builds a deterministic opaque transient resource ID from the concrete origin fetch URI.
///
/// `resolved_origin_uri` must be the concrete resource URI after manifest-relative URL resolution against the final
/// manifest URL. In provider-url-failover/redirect flows this may intentionally include the selected mirror or final
/// CDN/origin host. Do not make this ID input host-neutral unless `TransientResourceRef` keeps a separate concrete fetch
/// URI and tests prove relative segment/MAP/key downloads still use that concrete URI.
///
pub fn build_transient_resource_id(
    resolved_origin_uri: &str,
    reverse_proxy_rewrite_secret: &[u8],
) -> TransientResourceId {
    let key = blake3::derive_key(TRANSIENT_RESOURCE_ID_KEY_CONTEXT, reverse_proxy_rewrite_secret);
    let digest = blake3::keyed_hash(&key, resolved_origin_uri.as_bytes());
    let token = general_purpose::URL_SAFE_NO_PAD.encode(digest.as_bytes());
    TransientResourceId(token.chars().take(TRANSIENT_RESOURCE_ID_LEN).collect())
}

/// Transient origin resource category used for direct passthrough streaming.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransientResourceKind {
    Segment,
    Key,
    Map,
    Part,
    Other,
}

/// Per-session mapping from an opaque transient ID to one resolved origin resource.
#[derive(Clone, Eq, PartialEq)]
pub struct TransientResourceRef {
    pub id: TransientResourceId,
    pub kind: TransientResourceKind,
    /// True only for media bytes covered by an active origin encryption tag.
    pub encrypted_media: bool,
    /// Concrete origin fetch URI for this transient resource.
    ///
    /// This is request-local fetch metadata, not HLS session identity. It must remain the final concrete URI produced
    /// after resolving relative segment/MAP/key references against the final manifest URL.
    pub resolved_origin_uri: String,
    pub content_type_hint: Option<String>,
    pub file_ext_hint: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub access: Arc<CacheAccessState>,
    revision: TransientResourceRevision,
}

#[derive(Clone, Eq, PartialEq)]
struct TransientObjectResourceBinding {
    resource_id: TransientResourceId,
    revision: TransientResourceRevision,
    kind: TransientResourceKind,
    encrypted_media: bool,
    resolved_origin_uri: String,
    file_extension: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TransientObjectCacheStatus {
    Fetching { started_at_ms: u64 },
    Ready { content_length: u64, ready_at_ms: u64 },
    FailedRetryable { failed_at_ms: u64, retry_after_ms: u64 },
    FailedPermanent { failed_at_ms: u64, status: Option<StatusCode> },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TransientObjectUnavailableState {
    Missing,
    Fetching,
    FailedRetryable { retry_after_ms: u64 },
    FailedPermanent,
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
    binding: TransientObjectResourceBinding,
}

#[derive(Clone)]
pub enum TransientObjectFetchDecision {
    Ready,
    Fetch(Box<TransientObjectFetchToken>),
    Wait(Arc<Notify>),
}

/// Generation-safe ownership of one transient full-object cache fill.
#[derive(Clone)]
pub struct TransientObjectFetchToken {
    lookup_key: TransientObjectCacheKey,
    cache_key: TransientObjectCacheKey,
    entry_access: Arc<CacheAccessState>,
    binding: TransientObjectResourceBinding,
    superseded_object: Option<TransientObjectRemoval>,
}

impl TransientObjectFetchToken {
    pub fn cache_key(&self) -> &TransientObjectCacheKey { &self.cache_key }

    pub(crate) fn superseded_object(&self) -> Option<&TransientObjectRemoval> { self.superseded_object.as_ref() }
}

impl TransientObjectResourceBinding {
    fn from_resource(resource: &TransientResourceRef, file_extension: &str) -> Self {
        Self {
            resource_id: resource.id.clone(),
            revision: resource.revision,
            kind: resource.kind,
            encrypted_media: resource.encrypted_media,
            resolved_origin_uri: resource.resolved_origin_uri.clone(),
            file_extension: file_extension.to_string(),
        }
    }

    fn matches_resource_identity(&self, resource: &TransientResourceRef) -> bool {
        self.resource_id == resource.id
            && self.revision == resource.revision
            && self.kind == resource.kind
            && self.encrypted_media == resource.encrypted_media
            && self.resolved_origin_uri == resource.resolved_origin_uri
            && resource.file_ext_hint.as_deref().is_none_or(|extension| extension == self.file_extension)
    }

    fn matches_valid_resource(&self, resource: &TransientResourceRef, now_ms: u64) -> bool {
        self.matches_resource_identity(resource) && resource.is_valid_at(now_ms)
    }
}

impl TransientObjectCacheEntry {
    fn new_fetching(
        key: TransientObjectCacheKey,
        binding: TransientObjectResourceBinding,
        now_ms: u64,
        expires_at_ms: u64,
        content_type: String,
    ) -> Self {
        Self {
            key,
            status: TransientObjectCacheStatus::Fetching { started_at_ms: now_ms },
            content_type,
            created_at_ms: now_ms,
            last_accessed_at_ms: now_ms,
            expires_at_ms,
            access: Arc::new(CacheAccessState::new()),
            binding,
        }
    }

    pub fn is_ready_at(&self, now_ms: u64) -> bool {
        matches!(self.status, TransientObjectCacheStatus::Ready { .. }) && self.expires_at_ms >= now_ms
    }

    pub fn ready_content_length(&self) -> Option<u64> {
        match self.status {
            TransientObjectCacheStatus::Ready { content_length, .. } => Some(content_length),
            TransientObjectCacheStatus::Fetching { .. }
            | TransientObjectCacheStatus::FailedRetryable { .. }
            | TransientObjectCacheStatus::FailedPermanent { .. } => None,
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
            .field("resource_revision", &self.binding.revision)
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
            encrypted_media: false,
            resolved_origin_uri,
            content_type_hint: file_ext_hint
                .as_deref()
                .and_then(default_content_type_for_transient_ext)
                .map(str::to_string),
            file_ext_hint,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            access: Arc::new(CacheAccessState::new()),
            revision: TransientResourceRevision::default(),
        }
    }

    pub fn is_valid_at(&self, now_ms: u64) -> bool { now_ms <= self.expires_at_ms }

    fn refresh_from(&mut self, next: TransientResourceRef) {
        self.kind = next.kind;
        // A URI reused across key transitions is ambiguous while an older cache object can
        // still exist. Preserve the conservative encrypted classification until eviction.
        self.encrypted_media |= next.encrypted_media;
        self.resolved_origin_uri = next.resolved_origin_uri;
        self.content_type_hint = next.content_type_hint;
        self.file_ext_hint = next.file_ext_hint;
        self.expires_at_ms = next.expires_at_ms;
        self.revision = next.revision;
    }

    fn has_same_cache_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.encrypted_media == (self.encrypted_media || other.encrypted_media)
            && self.resolved_origin_uri == other.resolved_origin_uri
            && self.content_type_hint == other.content_type_hint
            && self.file_ext_hint == other.file_ext_hint
    }

    pub fn active_readers(&self) -> u32 { self.access.active_readers() }
}

impl fmt::Debug for TransientResourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientResourceRef")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("encrypted_media", &self.encrypted_media)
            .field("resolved_origin_uri", &"<redacted>")
            .field("content_type_hint", &self.content_type_hint)
            .field("file_ext_hint", &self.file_ext_hint)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("active_readers", &self.access.active_readers())
            .field("revision", &self.revision)
            .finish()
    }
}

/// Per-session transient passthrough manifest and resource mappings.
#[derive(Clone)]
pub struct TransientPassthroughState {
    pub resources: HashMap<TransientResourceId, TransientResourceRef>,
    pub object_cache: HashMap<TransientObjectCacheKey, TransientObjectCacheEntry>,
    object_fetch_notifiers: HashMap<TransientObjectCacheKey, Arc<Notify>>,
    next_resource_revision: u64,
    next_object_fetch_generation: u64,
    pub last_manifest_body: Option<String>,
    pub last_manifest_rendered_at_ms: Option<u64>,
    pub last_manifest_playlist_duration_ms: Option<u64>,
    pub last_manifest_valid_until_ms: Option<u64>,
    pub resource_ttl_ms: u64,
}

impl TransientPassthroughState {
    pub fn new(resource_ttl_ms: u64) -> Self {
        Self {
            resources: HashMap::new(),
            object_cache: HashMap::new(),
            object_fetch_notifiers: HashMap::new(),
            next_resource_revision: 1,
            next_object_fetch_generation: 0,
            last_manifest_body: None,
            last_manifest_rendered_at_ms: None,
            last_manifest_playlist_duration_ms: None,
            last_manifest_valid_until_ms: None,
            resource_ttl_ms,
        }
    }

    pub fn set_resource_ttl_ms(&mut self, resource_ttl_ms: u64) { self.resource_ttl_ms = resource_ttl_ms; }

    pub fn replace_manifest(&mut self, body: String, rendered_at_ms: u64) {
        self.last_manifest_body = Some(body);
        self.last_manifest_rendered_at_ms = Some(rendered_at_ms);
        self.last_manifest_playlist_duration_ms = None;
        self.last_manifest_valid_until_ms = None;
    }

    pub fn replace_manifest_with_validity(&mut self, body: String, rendered_at_ms: u64, playlist_duration_ms: u64) {
        self.last_manifest_body = Some(body);
        self.last_manifest_rendered_at_ms = Some(rendered_at_ms);
        self.last_manifest_playlist_duration_ms = Some(playlist_duration_ms);
        self.last_manifest_valid_until_ms = Some(rendered_at_ms.saturating_add(playlist_duration_ms));
    }

    pub fn upsert_resources<I>(&mut self, resources: I)
    where
        I: IntoIterator<Item = TransientResourceRef>,
    {
        for mut resource in resources {
            let resource_id = resource.id.clone();
            let expires_at_ms = resource.expires_at_ms;
            let extend_cached_object_ttl = resource.kind != TransientResourceKind::Key
                && self.resources.get(&resource_id).is_none_or(|existing| existing.kind != TransientResourceKind::Key);
            let preserve_revision = self
                .resources
                .get(&resource_id)
                .filter(|existing| existing.has_same_cache_identity(&resource))
                .map(|existing| existing.revision);
            resource.revision = if let Some(revision) = preserve_revision {
                revision
            } else {
                let revision = TransientResourceRevision(self.next_resource_revision);
                self.next_resource_revision = self.next_resource_revision.saturating_add(1);
                revision
            };
            match self.resources.get_mut(&resource.id) {
                Some(existing) => existing.refresh_from(resource),
                None => {
                    let _previous = self.resources.insert(resource.id.clone(), resource);
                }
            }
            if extend_cached_object_ttl {
                self.extend_object_ttl_for_resource(&resource_id, expires_at_ms);
            }
        }
    }

    fn extend_object_ttl_for_resource(&mut self, resource_id: &TransientResourceId, expires_at_ms: u64) {
        for (key, entry) in &mut self.object_cache {
            if key.transient_resource_id() == resource_id
                && matches!(entry.status, TransientObjectCacheStatus::Ready { .. })
            {
                entry.expires_at_ms = entry.expires_at_ms.max(expires_at_ms);
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
        self.resources.retain(|id, resource| {
            protected.contains(id) || resource.is_valid_at(now_ms) || resource.active_readers() > 0
        });
    }

    pub fn protected_manifest_resource_ids(&self) -> HashSet<TransientResourceId> {
        self.last_manifest_body.as_deref().map_or_else(HashSet::new, extract_transient_resource_ids)
    }

    pub fn active_resource_readers(&self) -> u32 {
        self.resources.values().map(TransientResourceRef::active_readers).sum()
    }

    pub fn has_active_resource_readers(&self) -> bool { self.active_resource_readers() > 0 }

    /// Builds the object-cache key for an already registered transient resource.
    ///
    /// The key is intentionally based on the opaque resource ID. The concrete fetch URI remains in
    /// `TransientResourceRef::resolved_origin_uri` and must not be reconstructed from this key.
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
        resource_kind: TransientResourceKind,
        now_ms: u64,
        protected: bool,
    ) -> Option<TransientObjectCacheEntry> {
        let binding = {
            let entry = self.object_cache.get(key)?;
            let resource = self.resources.get(key.transient_resource_id())?;
            if resource.kind != resource_kind {
                return None;
            }
            let binding = TransientObjectResourceBinding::from_resource(resource, &entry.binding.file_extension);
            let resource_usable =
                resource.is_valid_at(now_ms) || (protected && resource_kind != TransientResourceKind::Key);
            (binding.matches_resource_identity(resource) && resource_usable).then_some(binding)?
        };
        let entry = self.object_cache.get_mut(key)?;
        let ttl_expired = entry.expires_at_ms < now_ms;
        if !matches!(entry.status, TransientObjectCacheStatus::Ready { .. })
            || entry.binding != binding
            || (resource_kind == TransientResourceKind::Key && entry.ready_content_length() != Some(16))
            || (ttl_expired && (resource_kind == TransientResourceKind::Key || !protected))
        {
            return None;
        }
        entry.last_accessed_at_ms = now_ms;
        entry.access.reader_started(now_ms);
        entry.access.reader_finished();
        Some(entry.clone())
    }

    fn current_resource_binding(
        &self,
        resource: &TransientResourceRef,
        file_extension: &str,
        now_ms: u64,
    ) -> Option<TransientObjectResourceBinding> {
        let current = self.resources.get(&resource.id)?;
        let binding = TransientObjectResourceBinding::from_resource(current, file_extension);
        (current.has_same_cache_identity(resource) && binding.matches_valid_resource(current, now_ms))
            .then_some(binding)
    }

    fn binding_is_current(&self, binding: &TransientObjectResourceBinding, now_ms: u64) -> bool {
        self.resources
            .get(&binding.resource_id)
            .is_some_and(|resource| binding.matches_valid_resource(resource, now_ms))
    }

    pub fn begin_object_fetch(
        &mut self,
        proxy_session_id: &ProxySessionId,
        resource: &TransientResourceRef,
        file_ext: &str,
        now_ms: u64,
        cache_duration_ms: u64,
    ) -> TransientObjectFetchDecision {
        let lookup_key = Self::transient_object_key(proxy_session_id, &resource.id, file_ext.to_string());
        let current_binding = self.current_resource_binding(resource, file_ext, now_ms);
        let binding_is_current = current_binding.is_some();
        let binding =
            current_binding.unwrap_or_else(|| TransientObjectResourceBinding::from_resource(resource, file_ext));
        match self.object_cache.get(&lookup_key) {
            Some(entry)
                if binding_is_current
                    && entry.is_ready_at(now_ms)
                    && entry.binding == binding
                    && (resource.kind != TransientResourceKind::Key || entry.ready_content_length() == Some(16)) =>
            {
                return TransientObjectFetchDecision::Ready;
            }
            Some(entry)
                if binding_is_current
                    && entry.binding == binding
                    && entry.expires_at_ms >= now_ms
                    && matches!(entry.status, TransientObjectCacheStatus::Fetching { .. }) =>
            {
                let notifier =
                    self.object_fetch_notifiers.entry(lookup_key).or_insert_with(|| Arc::new(Notify::new())).clone();
                return TransientObjectFetchDecision::Wait(notifier);
            }
            Some(_) | None => {}
        }
        let expires_at_ms = now_ms.saturating_add(cache_duration_ms).max(resource.expires_at_ms);
        let content_type = resource.content_type_hint.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        let fetch_generation = self.next_object_fetch_generation;
        self.next_object_fetch_generation = self.next_object_fetch_generation.saturating_add(1);
        let cache_key = Self::transient_object_key(
            proxy_session_id,
            &resource.id,
            format!("{file_ext}.fill-{fetch_generation:016x}"),
        );
        let superseded_object = self.remove_object_entry(&lookup_key);
        let _previous = self.object_cache.insert(
            lookup_key.clone(),
            TransientObjectCacheEntry::new_fetching(
                cache_key.clone(),
                binding.clone(),
                now_ms,
                expires_at_ms,
                content_type,
            ),
        );
        self.object_fetch_notifiers.entry(lookup_key.clone()).or_insert_with(|| Arc::new(Notify::new()));
        let Some(entry) = self.object_cache.get(&lookup_key) else {
            return TransientObjectFetchDecision::Wait(
                self.object_fetch_notifiers.entry(lookup_key).or_insert_with(|| Arc::new(Notify::new())).clone(),
            );
        };
        TransientObjectFetchDecision::Fetch(Box::new(TransientObjectFetchToken {
            lookup_key,
            cache_key,
            entry_access: Arc::clone(&entry.access),
            binding,
            superseded_object,
        }))
    }

    fn object_fetch_entry_matches(&self, token: &TransientObjectFetchToken) -> bool {
        self.object_cache.get(&token.lookup_key).is_some_and(|entry| {
            matches!(entry.status, TransientObjectCacheStatus::Fetching { .. })
                && entry.key == token.cache_key
                && Arc::ptr_eq(&entry.access, &token.entry_access)
                && entry.binding == token.binding
        })
    }

    pub(crate) fn object_fetch_token_matches(&self, token: &TransientObjectFetchToken) -> bool {
        self.object_fetch_entry_matches(token)
    }

    fn mark_object_ready(
        &mut self,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) {
        let notify_waiters = self.object_fetch_notifiers.remove(&token.lookup_key);
        if let Some(entry) = self.object_cache.get_mut(&token.lookup_key) {
            entry.status = TransientObjectCacheStatus::Ready { content_length, ready_at_ms: now_ms };
            entry.content_type = content_type;
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = expires_at_ms;
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
    }

    pub(crate) fn mark_object_ready_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_ready(token, content_type, content_length, now_ms, expires_at_ms);
        true
    }

    fn mark_object_failed_retryable(&mut self, key: &TransientObjectCacheKey, now_ms: u64, retry_after_ms: u64) {
        let failed_expires_at_ms = self.failed_object_metadata_expires_at(now_ms);
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        if let Some(entry) = self.object_cache.get_mut(key) {
            entry.status = TransientObjectCacheStatus::FailedRetryable { failed_at_ms: now_ms, retry_after_ms };
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = entry.expires_at_ms.min(failed_expires_at_ms);
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
        self.enforce_failed_object_metadata_bound();
    }

    fn mark_object_failed_permanent(&mut self, key: &TransientObjectCacheKey, now_ms: u64, status: Option<StatusCode>) {
        let failed_expires_at_ms = self.failed_object_metadata_expires_at(now_ms);
        let notify_waiters = self.object_fetch_notifiers.remove(key);
        if let Some(entry) = self.object_cache.get_mut(key) {
            entry.status = TransientObjectCacheStatus::FailedPermanent { failed_at_ms: now_ms, status };
            entry.last_accessed_at_ms = now_ms;
            entry.expires_at_ms = entry.expires_at_ms.min(failed_expires_at_ms);
        }
        if let Some(notifier) = notify_waiters {
            notifier.notify_waiters();
        }
        self.enforce_failed_object_metadata_bound();
    }

    fn failed_object_metadata_expires_at(&self, now_ms: u64) -> u64 { now_ms.saturating_add(self.resource_ttl_ms) }

    pub(crate) fn mark_object_failed_retryable_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        retry_after_ms: u64,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_failed_retryable(&token.lookup_key, now_ms, retry_after_ms);
        true
    }

    pub(crate) fn mark_object_failed_permanent_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        status: Option<StatusCode>,
    ) -> bool {
        if !self.object_fetch_entry_matches(token) {
            return false;
        }
        if !self.binding_is_current(&token.binding, now_ms) {
            let _removed = self.remove_object_entry(&token.lookup_key);
            return false;
        }
        self.mark_object_failed_permanent(&token.lookup_key, now_ms, status);
        true
    }

    pub fn object_status(&self, key: &TransientObjectCacheKey) -> Option<TransientObjectCacheStatus> {
        self.object_cache.get(key).map(|entry| entry.status.clone())
    }

    /// Returns the finite READY lifetime of an AES key dependency without touching access accounting.
    pub(crate) fn ready_key_object_valid_until_ms(
        &self,
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
        file_ext: &str,
        now_ms: u64,
    ) -> Option<u64> {
        let resource = self.resources.get(resource_id)?;
        if resource.kind != TransientResourceKind::Key
            || resource.file_ext_hint.as_deref() != Some(file_ext)
            || !resource.is_valid_at(now_ms)
        {
            return None;
        }
        let key = Self::transient_object_key(proxy_session_id, resource_id, file_ext.to_string());
        let entry = self.object_cache.get(&key)?;
        let binding = TransientObjectResourceBinding::from_resource(resource, file_ext);
        let valid_aes128_key = matches!(entry.status, TransientObjectCacheStatus::Ready { content_length: 16, .. });
        (valid_aes128_key && entry.binding == binding && entry.is_ready_at(now_ms))
            .then_some(entry.expires_at_ms.min(resource.expires_at_ms))
    }

    pub fn object_unavailable_state(
        &self,
        key: &TransientObjectCacheKey,
        now_ms: u64,
    ) -> TransientObjectUnavailableState {
        let Some(entry) = self.object_cache.get(key) else {
            return TransientObjectUnavailableState::Missing;
        };
        if entry.expires_at_ms < now_ms || !self.binding_is_current(&entry.binding, now_ms) {
            return TransientObjectUnavailableState::Missing;
        }
        match entry.status {
            TransientObjectCacheStatus::Fetching { .. } => TransientObjectUnavailableState::Fetching,
            TransientObjectCacheStatus::FailedRetryable { retry_after_ms, .. } => {
                TransientObjectUnavailableState::FailedRetryable { retry_after_ms }
            }
            TransientObjectCacheStatus::FailedPermanent { .. } => TransientObjectUnavailableState::FailedPermanent,
            TransientObjectCacheStatus::Ready { .. } => TransientObjectUnavailableState::Missing,
        }
    }

    pub fn ready_object_cache_size(&self) -> u64 {
        self.object_cache.values().filter_map(TransientObjectCacheEntry::ready_content_length).sum()
    }

    pub(crate) fn take_expired_object_removals_except(
        &mut self,
        now_ms: u64,
        protected: &HashSet<TransientResourceId>,
        limit: usize,
    ) -> Vec<TransientObjectRemoval> {
        let mut keys = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                let ready_is_protected =
                    entry.ready_content_length().is_some() && protected.contains(key.transient_resource_id());
                if !ready_is_protected && entry.access.active_readers() == 0 && entry.expires_at_ms < now_ms {
                    return Some((key.clone(), entry.expires_at_ms, entry.created_at_ms, key.stable_value()));
                }
                None
            })
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| (left.1, left.2, &left.3).cmp(&(right.1, right.2, &right.3)));
        keys.into_iter().take(limit).filter_map(|(key, _, _, _)| self.remove_object_entry(&key)).collect()
    }

    pub fn remove_oldest_ready_object_except(
        &mut self,
        protected: &HashSet<TransientResourceId>,
    ) -> Option<TransientObjectRemoval> {
        let candidate = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| {
                if !protected.contains(key.transient_resource_id()) && entry.access.active_readers() == 0 {
                    return entry.ready_content_length().map(|content_length| {
                        (key.clone(), content_length, entry.last_accessed_at_ms, entry.created_at_ms)
                    });
                }
                None
            })
            .min_by_key(|(_, _, last_accessed_at_ms, created_at_ms)| (*last_accessed_at_ms, *created_at_ms))?;
        self.remove_object_entry(&candidate.0)
    }

    fn remove_object_entry(&mut self, lookup_key: &TransientObjectCacheKey) -> Option<TransientObjectRemoval> {
        let entry = self.object_cache.remove(lookup_key)?;
        if let Some(notifier) = self.object_fetch_notifiers.remove(lookup_key) {
            notifier.notify_waiters();
        }
        let content_length = entry.ready_content_length().unwrap_or_default();
        Some(TransientObjectRemoval { key: entry.key, content_length })
    }

    fn enforce_failed_object_metadata_bound(&mut self) {
        let mut failed = self
            .object_cache
            .iter()
            .filter_map(|(key, entry)| match entry.status {
                TransientObjectCacheStatus::FailedRetryable { failed_at_ms, .. }
                | TransientObjectCacheStatus::FailedPermanent { failed_at_ms, .. } => {
                    Some((key.clone(), failed_at_ms, entry.created_at_ms, key.stable_value()))
                }
                TransientObjectCacheStatus::Fetching { .. } | TransientObjectCacheStatus::Ready { .. } => None,
            })
            .collect::<Vec<_>>();
        let overflow = failed.len().saturating_sub(MAX_FAILED_TRANSIENT_OBJECT_ENTRIES);
        if overflow == 0 {
            return;
        }
        failed.sort_by(|left, right| (left.1, left.2, &left.3).cmp(&(right.1, right.2, &right.3)));
        for (key, _, _, _) in failed.into_iter().take(overflow) {
            let _removed = self.remove_object_entry(&key);
        }
    }
}

impl Default for TransientPassthroughState {
    fn default() -> Self { Self::new(DEFAULT_TRANSIENT_RESOURCE_TTL_MS) }
}

impl HlsSession {
    pub(crate) fn commit_transient_object_ready_if_current(
        &mut self,
        resource_kind: TransientResourceKind,
        token: &TransientObjectFetchToken,
        content_type: String,
        content_length: u64,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> bool {
        let committed =
            self.transient.mark_object_ready_if_current(token, content_type, content_length, now_ms, expires_at_ms);
        if committed && resource_kind == TransientResourceKind::Key {
            self.advance_media_readiness_generation();
        }
        committed
    }

    pub(crate) fn fail_transient_object_retryable_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        retry_after_ms: u64,
    ) -> bool {
        self.transient.mark_object_failed_retryable_if_current(token, now_ms, retry_after_ms)
    }

    pub(crate) fn fail_transient_object_permanent_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        status: Option<StatusCode>,
    ) -> bool {
        self.transient.mark_object_failed_permanent_if_current(token, now_ms, status)
    }
}

impl fmt::Debug for TransientPassthroughState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientPassthroughState")
            .field("resources_len", &self.resources.len())
            .field("object_cache_len", &self.object_cache.len())
            .field("object_fetch_notifiers_len", &self.object_fetch_notifiers.len())
            .field("next_resource_revision", &self.next_resource_revision)
            .field("next_object_fetch_generation", &self.next_object_fetch_generation)
            .field("last_manifest_body_len", &self.last_manifest_body.as_ref().map(String::len))
            .field("last_manifest_rendered_at_ms", &self.last_manifest_rendered_at_ms)
            .field("last_manifest_playlist_duration_ms", &self.last_manifest_playlist_duration_ms)
            .field("last_manifest_valid_until_ms", &self.last_manifest_valid_until_ms)
            .field("resource_ttl_ms", &self.resource_ttl_ms)
            .finish()
    }
}

pub(crate) fn extract_transient_resource_ids(body: &str) -> HashSet<TransientResourceId> {
    body.split("/r/")
        .skip(1)
        .filter_map(|tail| {
            let file_name: String =
                tail.chars().take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')).collect();
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
        "ts" => Some("video/mp2t"),
        "mp4" | "m4s" | "m4v" => Some("video/mp4"),
        "key" => Some("application/octet-stream"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_transient_resource_id, TransientObjectCacheStatus, TransientPassthroughState, TransientResourceKind,
        TransientResourceRef, MAX_FAILED_TRANSIENT_OBJECT_ENTRIES,
    };
    use crate::api::model::ProxySessionId;
    use std::{collections::HashSet, task::Poll};

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
        let mut resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            10,
            100,
            Some("ts".to_string()),
        );
        resource.encrypted_media = true;
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
        assert!(state.resources.get(&resource_id).expect("resource").encrypted_media);
        assert_eq!(state.get_valid_resource(&resource_id, 150).expect("resource remains valid").expires_at_ms, 220);
        assert!(state.get_valid_resource(&resource_id, 221).is_none());
    }

    #[test]
    fn transient_resource_render_extends_existing_object_ttl() {
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
        let key = TransientPassthroughState::transient_object_key(
            &ProxySessionId("proxy-session".to_string()),
            &resource_id,
            "ts",
        );
        state.upsert_resources([resource.clone()]);
        let token =
            match state.begin_object_fetch(&ProxySessionId("proxy-session".to_string()), &resource, "ts", 20, 50) {
                super::TransientObjectFetchDecision::Fetch(token) => token,
                super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                    panic!("new resource starts a cache fetch")
                }
            };
        assert!(state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 30, 110));
        assert_eq!(state.object_cache.get(&key).expect("object").expires_at_ms, 110);

        let updated = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            100,
            300,
            Some("ts".to_string()),
        );
        state.upsert_resources([updated]);

        let object = state.object_cache.get(&key).expect("object remains");
        assert!(matches!(object.status, TransientObjectCacheStatus::Ready { .. }));
        assert_eq!(object.expires_at_ms, 400);
    }

    #[test]
    fn transient_resource_refresh_does_not_extend_an_inflight_fill_deadline() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/inflight.ts",
            b"secret",
            10,
            100,
            Some("ts".to_string()),
        );
        state.upsert_resources([resource.clone()]);
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 20, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("new resource starts a cache fetch")
            }
        };

        state.upsert_resources([TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/inflight.ts",
            b"secret",
            100,
            300,
            Some("ts".to_string()),
        )]);

        assert_eq!(state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms), Some(110));
    }

    #[test]
    fn transient_media_resource_extensions_use_video_mp4_content_type() {
        for extension in ["mp4", "m4s", "m4v"] {
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                format!("http://origin.example.com/live/seg.{extension}"),
                b"secret",
                10,
                100,
                Some(extension.to_string()),
            );

            assert_eq!(resource.content_type_hint.as_deref(), Some("video/mp4"));
        }
    }

    #[test]
    fn aes_key_readiness_requires_exactly_sixteen_bytes_and_honors_the_inclusive_ttl_boundary() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        for invalid_size in [15, 17] {
            let mut state = TransientPassthroughState::default();
            let resource = TransientResourceRef::new(
                TransientResourceKind::Key,
                format!("http://origin.example.com/live/key-{invalid_size}.key"),
                b"secret",
                10,
                100,
                Some("key".to_string()),
            );
            let resource_id = resource.id.clone();
            state.upsert_resources([resource.clone()]);
            let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
                super::TransientObjectFetchDecision::Fetch(token) => token,
                super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                    panic!("new AES key starts a cache fetch")
                }
            };
            assert!(state.mark_object_ready_if_current(
                &token,
                "application/octet-stream".to_string(),
                invalid_size,
                30,
                110,
            ));
            assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 30), None);
        }

        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/valid.key",
            b"secret",
            10,
            100,
            Some("key".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource.clone()]);
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("new AES key starts a cache fetch")
            }
        };
        assert!(state.mark_object_ready_if_current(&token, "application/octet-stream".to_string(), 16, 30, 110,));

        assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 110), Some(110));
        assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 111), None);
    }

    #[test]
    fn manifest_refresh_does_not_extend_a_ready_key_revision_and_rotation_reports_the_displaced_object() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let first_resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/key.key",
            b"secret",
            10,
            100,
            Some("key".to_string()),
        );
        let resource_id = first_resource.id.clone();
        state.upsert_resources([first_resource.clone()]);
        let first = match state.begin_object_fetch(&proxy_session_id, &first_resource, "key", 20, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("first key revision starts a fetch")
            }
        };
        assert!(state.mark_object_ready_if_current(&first, "application/octet-stream".to_string(), 16, 30, 110,));

        let refreshed_resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/key.key",
            b"secret",
            100,
            100,
            Some("key".to_string()),
        );
        assert_eq!(refreshed_resource.id, resource_id);
        state.upsert_resources([refreshed_resource.clone()]);
        let lookup_key =
            TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "key".to_string());
        assert_eq!(state.object_cache.get(&lookup_key).map(|entry| entry.expires_at_ms), Some(110));

        let second = match state.begin_object_fetch(&proxy_session_id, &refreshed_resource, "key", 111, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("expired key revision starts a replacement fetch")
            }
        };
        let displaced = second.superseded_object().expect("replacement owns one bounded displaced object");
        assert_eq!(&displaced.key, first.cache_key());
        assert_eq!(displaced.content_length, 16);
        assert_ne!(second.cache_key(), first.cache_key());
    }

    #[test]
    fn stale_transient_fetch_token_cannot_overwrite_or_fail_a_new_generation() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/key.key",
            b"secret",
            10,
            100,
            Some("key".to_string()),
        );
        state.upsert_resources([resource.clone()]);
        let first = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 20, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("first key fetch starts")
            }
        };
        assert!(state.mark_object_failed_retryable_if_current(&first, 21, 0));
        let second = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 22, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("replacement key fetch starts")
            }
        };
        assert_ne!(first.cache_key(), second.cache_key());

        assert!(!state.mark_object_ready_if_current(&first, "application/octet-stream".to_string(), 16, 23, 110,));
        assert!(state.mark_object_ready_if_current(&second, "application/octet-stream".to_string(), 16, 24, 110,));
        assert!(!state.mark_object_failed_retryable_if_current(&first, 25, 0));
        assert!(matches!(
            state.object_status(&second.lookup_key),
            Some(TransientObjectCacheStatus::Ready { content_length: 16, .. })
        ));
    }

    #[tokio::test]
    async fn controlled_resource_mapping_replacement_invalidates_the_fetch_token_and_wakes_its_waiter() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let original = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin-a.example.com/live/segment.ts",
            b"secret",
            10,
            100,
            Some("ts".to_string()),
        );
        let resource_id = original.id.clone();
        state.upsert_resources([original]);
        let original = state.resources.get(&resource_id).expect("registered original mapping").clone();
        let token = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 20, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("new resource starts a cache fetch")
            }
        };
        let notifier = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 21, 50) {
            super::TransientObjectFetchDecision::Wait(notifier) => notifier,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Fetch(_) => {
                panic!("concurrent request waits for the owned fill")
            }
        };
        let waiter = notifier.notified();
        tokio::pin!(waiter);
        assert!(matches!(futures::poll!(&mut waiter), Poll::Pending));

        let mut replacement = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin-b.example.com/live/segment.ts",
            b"secret",
            22,
            100,
            Some("ts".to_string()),
        );
        replacement.id = resource_id;
        state.upsert_resources([replacement]);

        assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 23, 122));
        assert!(matches!(futures::poll!(&mut waiter), Poll::Ready(())));
        assert!(!state.object_cache.contains_key(&token.lookup_key));
        let replacement = state.resources.get(&token.binding.resource_id).expect("replacement mapping").clone();
        let replacement_token = match state.begin_object_fetch(&proxy_session_id, &replacement, "ts", 24, 50) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("replacement mapping owns a new cache fill")
            }
        };
        assert_ne!(token.cache_key(), replacement_token.cache_key());
        assert_ne!(token.binding.revision, replacement_token.binding.revision);
    }

    #[test]
    fn failed_transient_metadata_has_finite_ttl_and_a_hard_count_bound() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::new(100);
        let mut oldest_lookup_key = None;
        for index in 0..=MAX_FAILED_TRANSIENT_OBJECT_ENTRIES {
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                format!("http://origin.example.com/live/{index}.ts"),
                b"secret",
                0,
                10_000,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            state.upsert_resources([resource]);
            let resource = state.resources.get(&resource_id).expect("registered resource").clone();
            let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", index as u64, 10_000) {
                super::TransientObjectFetchDecision::Fetch(token) => token,
                super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                    panic!("unique resource starts a cache fill")
                }
            };
            if index == 0 {
                oldest_lookup_key = Some(token.lookup_key.clone());
            }
            assert!(state.mark_object_failed_permanent_if_current(&token, index as u64, None));
        }

        assert_eq!(
            state
                .object_cache
                .values()
                .filter(|entry| matches!(entry.status, TransientObjectCacheStatus::FailedPermanent { .. }))
                .count(),
            MAX_FAILED_TRANSIENT_OBJECT_ENTRIES
        );
        assert!(!state.object_cache.contains_key(&oldest_lookup_key.expect("oldest lookup key")));

        let mut state = TransientPassthroughState::new(100);
        let expiring = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/expiring.ts",
            b"secret",
            1_000,
            10_000,
            Some("ts".to_string()),
        );
        let expiring_id = expiring.id.clone();
        state.upsert_resources([expiring]);
        let expiring = state.resources.get(&expiring_id).expect("registered expiring resource").clone();
        let token = match state.begin_object_fetch(&proxy_session_id, &expiring, "ts", 1_010, 10_000) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("expiring resource starts a cache fill")
            }
        };
        assert!(state.mark_object_failed_retryable_if_current(&token, 1_020, 10));
        assert_eq!(state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms), Some(1_120));
        state.upsert_resources([TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/expiring.ts",
            b"secret",
            1_100,
            20_000,
            Some("ts".to_string()),
        )]);
        assert_eq!(
            state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms),
            Some(1_120),
            "manifest refresh must not make failed metadata immortal"
        );
        let removals = state.take_expired_object_removals_except(1_121, &HashSet::from([expiring_id]), 1);
        assert_eq!(removals.len(), 1);
        assert_eq!(&removals[0].key, token.cache_key());
        assert!(!state.object_cache.contains_key(&token.lookup_key));
    }

    #[test]
    fn expired_resource_validity_rejects_a_late_fetch_commit() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/short.ts",
            b"secret",
            10,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        let resource = state.resources.get(&resource_id).expect("registered resource").clone();
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 15, 100) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("valid resource starts a cache fill")
            }
        };

        assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 115));
        assert!(!state.object_cache.contains_key(&token.lookup_key));
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
            format!("#EXTM3U\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.ts\n", resource_id.0),
            0,
        );

        state.prune_expired(20);

        assert!(state.resources.contains_key(&resource_id));
    }
}
