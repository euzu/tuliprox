use super::{
    default_content_type_for_transient_ext, CacheAccessState, HlsAccessLeaseId, HlsManifestLimitViolation,
    HlsPublishedTransientResourceIds, HlsTransientManifestTemplate, TransientManifestGeneration,
    TransientManifestIdentity, TransientObjectCacheEntry, TransientObjectCacheKey, TransientObjectFetchToken,
    TransientObjectResourceBinding, TransientResourceFile, TransientResourceId, TransientResourceRef,
    TransientResourceRevision, TRANSIENT_RESOURCE_ID_KEY_CONTEXT, TRANSIENT_RESOURCE_ID_LEN,
};
use crate::transient_manifest::TransientRewriteCheckpoint;
use axum::http::StatusCode;
use base64::{engine::general_purpose, Engine as _};
use std::{collections::HashSet, fmt, sync::Arc};
use tokio::sync::Notify;
use tuliprox_parser::hls::origin_manifest::{HlsManifestLifecycle, HlsManifestWindowPolicy, ParsedManifestSemantics};

impl HlsPublishedTransientResourceIds {
    pub fn from_manifest_body(body: &str) -> Self { Self(Arc::new(extract_transient_resource_ids(body))) }

    pub(super) fn from_shared(resource_ids: Arc<HashSet<TransientResourceId>>) -> Self { Self(resource_ids) }

    pub fn contains(&self, resource_id: &TransientResourceId) -> bool { self.0.contains(resource_id) }
}

impl TransientManifestGeneration {
    #[cfg(test)]
    pub(crate) const fn for_test(generation: u64) -> Self { Self(generation) }
}

/// Exact lease incarnation that currently publishes one finalized transient manifest generation.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) struct TransientManifestLeaseBinding {
    pub(crate) lease_id: HlsAccessLeaseId,
    pub(crate) lease_issued_at_ms: u64,
    pub(crate) manifest_generation: TransientManifestGeneration,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct HlsTransientManifestFootprint {
    pub(crate) resources: usize,
    pub(crate) resource_entries: usize,
    pub(crate) rewritten_bytes: usize,
    pub(crate) origin_uri_bytes: usize,
    pub(crate) retained_finalized_generations: usize,
    pub(crate) finalized_generation_memberships: usize,
    pub(crate) estimated_metadata_bytes: usize,
}

/// Rejects a transient commit before publication when its complete client representation is unavailable.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum HlsTransientManifestCommitError {
    LocalRepresentationLimit(HlsManifestLimitViolation),
    SnapshotUnavailable,
    CommitGenerationExhausted,
}

impl From<HlsManifestLimitViolation> for HlsTransientManifestCommitError {
    fn from(violation: HlsManifestLimitViolation) -> Self { Self::LocalRepresentationLimit(violation) }
}

pub(crate) struct RollingEventAppendContext {
    pub(crate) suffix_offset: usize,
    pub(crate) rewritten_prefix: Arc<str>,
    pub(crate) template: Arc<HlsTransientManifestTemplate>,
    pub(crate) checkpoint: TransientRewriteCheckpoint,
}

impl TransientManifestLeaseBinding {
    pub(crate) const fn new(
        lease_id: HlsAccessLeaseId,
        lease_issued_at_ms: u64,
        manifest_generation: TransientManifestGeneration,
    ) -> Self {
        Self { lease_id, lease_issued_at_ms, manifest_generation }
    }
}

#[derive(Clone, Copy)]
pub(super) enum TransientResourceValidityScope<'a> {
    PublishedSession,
    AccessLease {
        lease_id: &'a HlsAccessLeaseId,
        lease_issued_at_ms: u64,
        published_resource_ids: &'a HlsPublishedTransientResourceIds,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum CommittedTransientManifestValidity {
    Rolling { valid_until_ms: Option<u64>, window_policy: HlsManifestWindowPolicy },
    Finalized,
}

impl CommittedTransientManifestValidity {
    pub(super) const fn from_semantics(
        semantics: ParsedManifestSemantics,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
    ) -> Self {
        match semantics.lifecycle() {
            HlsManifestLifecycle::Rolling => Self::Rolling {
                valid_until_ms: match playlist_duration_ms {
                    Some(duration_ms) => Some(rendered_at_ms.saturating_add(duration_ms)),
                    None => None,
                },
                window_policy: semantics.window_policy(),
            },
            HlsManifestLifecycle::Finalized => Self::Finalized,
        }
    }

    pub(super) const fn is_finalized(self) -> bool { matches!(self, Self::Finalized) }

    pub(super) const fn window_policy(self) -> HlsManifestWindowPolicy {
        match self {
            Self::Rolling { window_policy, .. } => window_policy,
            Self::Finalized => HlsManifestWindowPolicy::PreserveFullManifest,
        }
    }

    pub(super) const fn valid_until_ms(self) -> Option<u64> {
        match self {
            Self::Rolling { valid_until_ms, .. } => valid_until_ms,
            Self::Finalized => None,
        }
    }
}

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

#[derive(Clone)]
pub enum TransientObjectFetchDecision {
    Ready,
    Fetch(Box<TransientObjectFetchToken>),
    Wait(Arc<Notify>),
}

impl TransientObjectFetchToken {
    pub fn cache_key(&self) -> &TransientObjectCacheKey { &self.cache_key }

    pub fn superseded_object(&self) -> Option<&TransientObjectRemoval> { self.superseded_object.as_ref() }
}

impl TransientObjectResourceBinding {
    pub(super) fn from_resource(resource: &TransientResourceRef, file_extension: &str) -> Self {
        Self {
            resource_id: resource.id.clone(),
            revision: resource.revision,
            kind: resource.kind,
            encrypted_media: resource.encrypted_media,
            resolved_origin_uri: resource.resolved_origin_uri.clone(),
            file_extension: file_extension.to_string(),
        }
    }

    pub(super) fn matches_resource_identity(&self, resource: &TransientResourceRef) -> bool {
        self.resource_id == resource.id
            && self.revision == resource.revision
            && self.kind == resource.kind
            && self.encrypted_media == resource.encrypted_media
            && self.resolved_origin_uri == resource.resolved_origin_uri
            && resource.file_ext_hint.as_deref().is_none_or(|extension| extension == self.file_extension)
    }
}

impl TransientObjectCacheEntry {
    pub(super) fn new_fetching(
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

    pub(super) fn refresh_from(&mut self, next: TransientResourceRef) {
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

    pub(super) fn has_same_cache_identity(&self, other: &Self) -> bool {
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

pub(super) fn transient_manifest_identity(
    body: &str,
    resource_ids: &HashSet<TransientResourceId>,
) -> TransientManifestIdentity {
    let body_hash = *blake3::hash(body.as_bytes()).as_bytes();
    let mut ordered_resource_ids = resource_ids.iter().map(|resource_id| resource_id.0.as_str()).collect::<Vec<_>>();
    ordered_resource_ids.sort_unstable();
    let mut resource_hasher = blake3::Hasher::new();
    for resource_id in ordered_resource_ids {
        resource_hasher.update(&u64::try_from(resource_id.len()).unwrap_or(u64::MAX).to_le_bytes());
        resource_hasher.update(resource_id.as_bytes());
    }
    TransientManifestIdentity { body_hash, resource_set_hash: *resource_hasher.finalize().as_bytes() }
}

pub(super) fn complete_manifest_append_boundary(body: &str) -> bool {
    body.ends_with('\n')
        && body.lines().rev().map(str::trim).find(|line| !line.is_empty()).is_some_and(|line| !line.starts_with('#'))
}

pub(crate) fn extract_transient_resource_ids(body: &str) -> HashSet<TransientResourceId> {
    let mut resource_ids = HashSet::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            for uri in tag_uri_attributes(line) {
                if let Some(resource_id) = transient_resource_id_from_shared_route(uri) {
                    resource_ids.insert(resource_id);
                }
            }
        } else if let Some(resource_id) = transient_resource_id_from_shared_route(line) {
            resource_ids.insert(resource_id);
        }
    }
    resource_ids
}

/// Yields the quoted URI values carried by a tag line (`URI="..."`).
fn tag_uri_attributes(line: &str) -> impl Iterator<Item = &str> {
    line.split("URI=\"").skip(1).filter_map(|tail| tail.split('"').next())
}

/// Extracts one opaque transient resource ID from a canonical own `/r/{id}.{ext}` route.
///
/// Unlike a naive `split("/r/")`, this only recognises proxy-owned shared-HLS locators: it
/// ignores `/r/` occurrences inside comments (non-URI tag content is never yielded as a URI),
/// query strings or fragments, and rejects absolute foreign URIs by requiring a path-only
/// `/hls/shared/live/` prefix before the route segment.
fn transient_resource_id_from_shared_route(uri: &str) -> Option<TransientResourceId> {
    let path = uri.split(['?', '#']).next().unwrap_or("");
    if path.contains("://") {
        return None;
    }
    let (prefix, remainder) = path.rsplit_once("/r/")?;
    if !prefix.contains("/hls/shared/live/") {
        return None;
    }
    TransientResourceFile::parse(remainder).map(|file| file.resource_id)
}
