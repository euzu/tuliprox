use super::{
    manifest_limits::{
        HlsManifestLimitKind, HlsManifestLimitViolation, MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
        MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS, MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
        MAX_TRANSIENT_MANIFEST_RESOURCES, MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
        MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION, MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
    },
    manifest_snapshot::{
        extend_hls_transient_manifest_template, parse_hls_transient_manifest_template, HlsTransientManifestTemplate,
    },
    CacheAccessState, HlsAccessLeaseId, HlsSession, ProxySessionId, TransientObjectCacheKey, TransientResourceFile,
};
use crate::transient_manifest::TransientRewriteCheckpoint;
use axum::http::StatusCode;
use base64::{engine::general_purpose, Engine as _};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;
use tuliprox_parser::hls::origin_manifest::{HlsManifestLifecycle, HlsManifestWindowPolicy, ParsedManifestSemantics};

const TRANSIENT_RESOURCE_ID_LEN: usize = 16;
const DEFAULT_TRANSIENT_RESOURCE_TTL_MS: u64 = 300_000;
const MAX_FAILED_TRANSIENT_OBJECT_ENTRIES: usize = 256;
const TRANSIENT_RESOURCE_ID_KEY_CONTEXT: &str = "tuliprox:hls-cache:transient-resource-id-key:v1";
const ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES: usize = 24;
const ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES: usize = 16;

pub(crate) const fn transient_object_expires_at(now_ms: u64, cache_duration_ms: u64) -> u64 {
    now_ms.saturating_add(cache_duration_ms)
}

fn ensure_manifest_limit(
    kind: HlsManifestLimitKind,
    actual: usize,
    limit: usize,
) -> Result<(), HlsManifestLimitViolation> {
    if actual > limit {
        return Err(HlsManifestLimitViolation::new(kind, actual, limit));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct TransientResourceRevision(u64);

/// Opaque ID for a transient passthrough resource.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TransientResourceId(pub String);

/// Transient resource locators published in one access lease's manifest view.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct HlsPublishedTransientResourceIds(Arc<HashSet<TransientResourceId>>);

impl HlsPublishedTransientResourceIds {
    pub fn from_manifest_body(body: &str) -> Self { Self(Arc::new(extract_transient_resource_ids(body))) }

    fn from_shared(resource_ids: Arc<HashSet<TransientResourceId>>) -> Self { Self(resource_ids) }

    pub fn contains(&self, resource_id: &TransientResourceId) -> bool { self.0.contains(resource_id) }
}

/// Monotonic identity of one committed transient manifest body.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TransientManifestGeneration(u64);

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

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct TransientManifestIdentity {
    body_hash: [u8; 32],
    resource_set_hash: [u8; 32],
}

#[derive(Clone)]
struct RetainedFinalizedManifestGeneration {
    identity: TransientManifestIdentity,
    resource_ids: Arc<HashSet<TransientResourceId>>,
    template: Option<Arc<HlsTransientManifestTemplate>>,
}

#[derive(Clone)]
struct RollingEventRewriteSeed {
    origin_body: Arc<str>,
    final_manifest_url_hash: [u8; 32],
    rewrite_secret_hash: [u8; 32],
    rewritten_body: Arc<str>,
    template: Arc<HlsTransientManifestTemplate>,
    checkpoint: TransientRewriteCheckpoint,
}

struct TransientManifestReplacement {
    body: Arc<str>,
    rendered_at_ms: u64,
    playlist_duration_ms: Option<u64>,
    semantics: ParsedManifestSemantics,
    resource_ids: Arc<HashSet<TransientResourceId>>,
    identity: TransientManifestIdentity,
    reusable_finalized_generation: Option<TransientManifestGeneration>,
    template: Option<Arc<HlsTransientManifestTemplate>>,
}

struct TransientManifestFootprintCandidate<'a> {
    rewritten_bytes: usize,
    resource_count: usize,
    resources: &'a [TransientResourceRef],
    lifecycle: HlsManifestLifecycle,
    resource_ids: &'a HashSet<TransientResourceId>,
    reusable_finalized_generation: Option<TransientManifestGeneration>,
    template: Option<&'a HlsTransientManifestTemplate>,
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
enum TransientResourceValidityScope<'a> {
    PublishedSession,
    AccessLease {
        lease_id: &'a HlsAccessLeaseId,
        lease_issued_at_ms: u64,
        published_resource_ids: &'a HlsPublishedTransientResourceIds,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CommittedTransientManifestValidity {
    Rolling { valid_until_ms: Option<u64>, window_policy: HlsManifestWindowPolicy },
    Finalized,
}

impl CommittedTransientManifestValidity {
    const fn from_semantics(
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

    const fn is_finalized(self) -> bool { matches!(self, Self::Finalized) }

    const fn window_policy(self) -> HlsManifestWindowPolicy {
        match self {
            Self::Rolling { window_policy, .. } => window_policy,
            Self::Finalized => HlsManifestWindowPolicy::PreserveFullManifest,
        }
    }

    const fn valid_until_ms(self) -> Option<u64> {
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

    pub fn superseded_object(&self) -> Option<&TransientObjectRemoval> { self.superseded_object.as_ref() }
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
    manifest_generation: u64,
    last_manifest_commit_identity: Option<super::media_reserve::HlsManifestCommitIdentity>,
    current_finalized_manifest_generation: Option<TransientManifestGeneration>,
    last_manifest_validity: CommittedTransientManifestValidity,
    last_manifest_resource_ids: Arc<HashSet<TransientResourceId>>,
    last_manifest_template: Option<Arc<HlsTransientManifestTemplate>>,
    rolling_event_rewrite_seed: Option<RollingEventRewriteSeed>,
    finalized_manifest_generations: HashMap<TransientManifestGeneration, RetainedFinalizedManifestGeneration>,
    finalized_manifest_lease_bindings: HashSet<TransientManifestLeaseBinding>,
    protected_finalized_resource_refcounts: HashMap<TransientResourceId, u16>,
    pub last_manifest_body: Option<Arc<str>>,
    pub last_manifest_rendered_at_ms: Option<u64>,
    pub last_manifest_playlist_duration_ms: Option<u64>,
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
            manifest_generation: 0,
            last_manifest_commit_identity: None,
            current_finalized_manifest_generation: None,
            last_manifest_validity: CommittedTransientManifestValidity::Rolling {
                valid_until_ms: None,
                window_policy: HlsManifestWindowPolicy::ApplyLiveWindow,
            },
            last_manifest_resource_ids: Arc::new(HashSet::new()),
            last_manifest_template: None,
            rolling_event_rewrite_seed: None,
            finalized_manifest_generations: HashMap::new(),
            finalized_manifest_lease_bindings: HashSet::new(),
            protected_finalized_resource_refcounts: HashMap::new(),
            last_manifest_body: None,
            last_manifest_rendered_at_ms: None,
            last_manifest_playlist_duration_ms: None,
            resource_ttl_ms,
        }
    }

    pub fn set_resource_ttl_ms(&mut self, resource_ttl_ms: u64) { self.resource_ttl_ms = resource_ttl_ms; }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_manifest_with_semantics(
        &mut self,
        body: String,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
    ) {
        let semantics = tuliprox_parser::hls::origin_manifest::parse_manifest_semantics(&body);
        if self.replace_manifest_common(body, rendered_at_ms, playlist_duration_ms, semantics).is_ok() {
            self.last_manifest_commit_identity = Some(super::media_reserve::HlsManifestCommitIdentity::committed(
                self.manifest_generation,
                rendered_at_ms,
            ));
        }
    }

    pub(crate) fn commit_rewritten_manifest_with_semantics(
        &mut self,
        body: String,
        resources: Vec<TransientResourceRef>,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
        semantics: ParsedManifestSemantics,
    ) -> Result<HlsTransientManifestFootprint, HlsTransientManifestCommitError> {
        let manifest_resource_ids = Arc::new(extract_transient_resource_ids(&body));
        let manifest_identity = transient_manifest_identity(&body, &manifest_resource_ids);
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        self.ensure_manifest_generation_available(reusable_finalized_generation)?;
        let resource_count = resources.len().max(manifest_resource_ids.len());
        Self::validate_manifest_shape(body.len(), resource_count)?;
        let template = if let Some(generation) = reusable_finalized_generation {
            self.finalized_manifest_generations.get(&generation).and_then(|retained| retained.template.clone())
        } else {
            parse_hls_transient_manifest_template(&body)?
        }
        .ok_or(HlsTransientManifestCommitError::SnapshotUnavailable)?;
        let footprint = self.validate_manifest_footprint(&TransientManifestFootprintCandidate {
            rewritten_bytes: body.len(),
            resource_count,
            resources: &resources,
            lifecycle: semantics.lifecycle(),
            resource_ids: &manifest_resource_ids,
            reusable_finalized_generation,
            template: Some(&template),
        })?;
        self.upsert_resources(resources);
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template: Some(template),
        })?;
        Ok(footprint)
    }

    pub(crate) fn commit_incremental_rewritten_manifest_with_semantics(
        &mut self,
        body: String,
        appended_resources: Vec<TransientResourceRef>,
        template: Arc<HlsTransientManifestTemplate>,
        rendered_at_ms: u64,
        resource_ttl_ms: u64,
        semantics: ParsedManifestSemantics,
    ) -> Result<HlsTransientManifestFootprint, HlsManifestLimitViolation> {
        self.ensure_manifest_generation_available(None)?;
        let mut manifest_resource_ids = std::mem::take(&mut self.last_manifest_resource_ids);
        let mut inserted_resource_ids = Vec::new();
        for resource in &appended_resources {
            if Arc::make_mut(&mut manifest_resource_ids).insert(resource.id.clone()) {
                inserted_resource_ids.push(resource.id.clone());
            }
        }
        let manifest_identity = if semantics.lifecycle().is_finalized() {
            transient_manifest_identity(&body, &manifest_resource_ids)
        } else {
            TransientManifestIdentity::default()
        };
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        let footprint = Self::validate_manifest_shape(body.len(), manifest_resource_ids.len()).and_then(|()| {
            self.validate_manifest_footprint(&TransientManifestFootprintCandidate {
                rewritten_bytes: body.len(),
                resource_count: manifest_resource_ids.len(),
                resources: &appended_resources,
                lifecycle: semantics.lifecycle(),
                resource_ids: &manifest_resource_ids,
                reusable_finalized_generation,
                template: Some(&template),
            })
        });
        let footprint = match footprint {
            Ok(footprint) => footprint,
            Err(violation) => {
                let resource_ids = Arc::make_mut(&mut manifest_resource_ids);
                for resource_id in inserted_resource_ids {
                    resource_ids.remove(&resource_id);
                }
                self.last_manifest_resource_ids = manifest_resource_ids;
                return Err(violation);
            }
        };
        self.upsert_resources(appended_resources);
        let expires_at_ms = rendered_at_ms.saturating_add(resource_ttl_ms);
        for resource_id in manifest_resource_ids.iter() {
            if let Some(resource) = self.resources.get_mut(resource_id) {
                resource.expires_at_ms = expires_at_ms;
            }
        }
        let playlist_duration_ms = Some(template.playlist_duration_ms());
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template: Some(template),
        })?;
        Ok(footprint)
    }

    pub(crate) fn rolling_event_append_context(
        &self,
        origin_body: &str,
        final_manifest_url: &str,
        rewrite_secret: &[u8],
    ) -> Option<RollingEventAppendContext> {
        let seed = self.rolling_event_rewrite_seed.as_ref()?;
        if seed.final_manifest_url_hash != *blake3::hash(final_manifest_url.as_bytes()).as_bytes()
            || seed.rewrite_secret_hash != *blake3::hash(rewrite_secret).as_bytes()
            || !complete_manifest_append_boundary(&seed.origin_body)
            || origin_body.len() <= seed.origin_body.len()
            || !origin_body.starts_with(seed.origin_body.as_ref())
        {
            return None;
        }
        Some(RollingEventAppendContext {
            suffix_offset: seed.origin_body.len(),
            rewritten_prefix: Arc::clone(&seed.rewritten_body),
            template: Arc::clone(&seed.template),
            checkpoint: seed.checkpoint.clone(),
        })
    }

    pub(crate) fn extend_rolling_event_template(
        previous: &Arc<HlsTransientManifestTemplate>,
        rewritten_suffix: &str,
    ) -> Result<Option<Arc<HlsTransientManifestTemplate>>, HlsManifestLimitViolation> {
        extend_hls_transient_manifest_template(previous, rewritten_suffix)
    }

    pub(crate) fn record_rolling_event_rewrite_seed(
        &mut self,
        origin_body: &str,
        final_manifest_url: &str,
        rewrite_secret: &[u8],
        checkpoint: TransientRewriteCheckpoint,
        retained_metadata_bytes: usize,
        eligible: bool,
    ) {
        let retained_with_origin_body = retained_metadata_bytes
            .saturating_add(origin_body.len())
            .saturating_add(std::mem::size_of::<RollingEventRewriteSeed>())
            .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES);
        if !eligible || retained_with_origin_body > MAX_ESTIMATED_TRANSIENT_METADATA_BYTES {
            self.rolling_event_rewrite_seed = None;
            return;
        }
        let (Some(rewritten_body), Some(template)) = (&self.last_manifest_body, &self.last_manifest_template) else {
            self.rolling_event_rewrite_seed = None;
            return;
        };
        self.rolling_event_rewrite_seed = Some(RollingEventRewriteSeed {
            origin_body: Arc::from(origin_body),
            final_manifest_url_hash: *blake3::hash(final_manifest_url.as_bytes()).as_bytes(),
            rewrite_secret_hash: *blake3::hash(rewrite_secret).as_bytes(),
            rewritten_body: Arc::clone(rewritten_body),
            template: Arc::clone(template),
            checkpoint,
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    fn replace_manifest_common(
        &mut self,
        body: String,
        rendered_at_ms: u64,
        playlist_duration_ms: Option<u64>,
        semantics: ParsedManifestSemantics,
    ) -> Result<(), HlsManifestLimitViolation> {
        let manifest_resource_ids = Arc::new(extract_transient_resource_ids(&body));
        let manifest_identity = transient_manifest_identity(&body, &manifest_resource_ids);
        let reusable_finalized_generation = semantics
            .lifecycle()
            .is_finalized()
            .then(|| self.finalized_generation_for_identity(manifest_identity))
            .flatten();
        let template = parse_hls_transient_manifest_template(&body).ok().flatten();
        self.replace_manifest_common_with_resource_ids(TransientManifestReplacement {
            body: Arc::from(body),
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids: manifest_resource_ids,
            identity: manifest_identity,
            reusable_finalized_generation,
            template,
        })
    }

    fn replace_manifest_common_with_resource_ids(
        &mut self,
        replacement: TransientManifestReplacement,
    ) -> Result<(), HlsManifestLimitViolation> {
        let TransientManifestReplacement {
            body,
            rendered_at_ms,
            playlist_duration_ms,
            semantics,
            resource_ids,
            identity,
            reusable_finalized_generation,
            template,
        } = replacement;
        self.ensure_manifest_generation_available(reusable_finalized_generation)?;
        let manifest_validity =
            CommittedTransientManifestValidity::from_semantics(semantics, rendered_at_ms, playlist_duration_ms);
        let manifest_generation = if let Some(generation) = reusable_finalized_generation {
            generation
        } else {
            self.manifest_generation = self.manifest_generation.saturating_add(1);
            TransientManifestGeneration(self.manifest_generation)
        };
        if reusable_finalized_generation.is_none() {
            self.last_manifest_resource_ids = Arc::clone(&resource_ids);
            self.last_manifest_template.clone_from(&template);
        }
        self.last_manifest_validity = manifest_validity;
        if manifest_validity.is_finalized() {
            self.current_finalized_manifest_generation = Some(manifest_generation);
            if reusable_finalized_generation.is_none() {
                self.insert_finalized_manifest_generation(
                    manifest_generation,
                    RetainedFinalizedManifestGeneration { identity, resource_ids: Arc::clone(&resource_ids), template },
                );
            }
        } else {
            self.current_finalized_manifest_generation = None;
        }
        self.prune_unreferenced_finalized_manifest_generations();
        if reusable_finalized_generation.is_none() {
            self.last_manifest_body = Some(body);
        }
        self.last_manifest_rendered_at_ms = Some(rendered_at_ms);
        self.last_manifest_playlist_duration_ms = playlist_duration_ms;
        Ok(())
    }

    fn validate_manifest_footprint(
        &self,
        candidate: &TransientManifestFootprintCandidate<'_>,
    ) -> Result<HlsTransientManifestFootprint, HlsManifestLimitViolation> {
        let origin_uri_bytes = self.prospective_origin_uri_bytes(candidate.resources);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientOriginUriBytes,
            origin_uri_bytes,
            MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
        )?;
        let resource_entries = self.prospective_resource_entry_count(candidate.resources);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientResourceEntries,
            resource_entries,
            MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION,
        )?;
        let prospective_generation_sets = self.prospective_finalized_generation_sets(
            candidate.lifecycle,
            candidate.resource_ids,
            candidate.reusable_finalized_generation,
        );
        let retained_finalized_generations = prospective_generation_sets.len();
        ensure_manifest_limit(
            HlsManifestLimitKind::FinalizedGenerations,
            retained_finalized_generations,
            MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS,
        )?;
        let finalized_generation_memberships = prospective_generation_sets
            .iter()
            .fold(0_usize, |total, resource_ids| total.saturating_add(resource_ids.len()));
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientGenerationMemberships,
            finalized_generation_memberships,
            MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
        )?;
        let estimated_metadata_bytes =
            self.prospective_estimated_metadata_bytes(candidate, &prospective_generation_sets);
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientEstimatedMetadataBytes,
            estimated_metadata_bytes,
            MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
        )?;
        Ok(HlsTransientManifestFootprint {
            resources: candidate.resource_count,
            resource_entries,
            rewritten_bytes: candidate.rewritten_bytes,
            origin_uri_bytes,
            retained_finalized_generations,
            finalized_generation_memberships,
            estimated_metadata_bytes,
        })
    }

    fn validate_manifest_shape(rewritten_bytes: usize, resource_count: usize) -> Result<(), HlsManifestLimitViolation> {
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientResources,
            resource_count,
            MAX_TRANSIENT_MANIFEST_RESOURCES,
        )?;
        ensure_manifest_limit(
            HlsManifestLimitKind::TransientRewrittenBytes,
            rewritten_bytes,
            MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
        )
    }

    fn ensure_manifest_generation_available(
        &self,
        reusable_generation: Option<TransientManifestGeneration>,
    ) -> Result<(), HlsManifestLimitViolation> {
        if reusable_generation.is_none() && self.manifest_generation == u64::MAX {
            return Err(HlsManifestLimitViolation::new(
                HlsManifestLimitKind::ManifestCommitGeneration,
                usize::MAX,
                usize::MAX - 1,
            ));
        }
        Ok(())
    }

    fn prospective_origin_uri_bytes(&self, resources: &[TransientResourceRef]) -> usize {
        let incoming_uri_bytes = resources
            .iter()
            .map(|resource| (&resource.id, resource.resolved_origin_uri.len()))
            .collect::<HashMap<_, _>>();
        let retained_uri_bytes = self
            .resources
            .iter()
            .filter(|(resource_id, _)| !incoming_uri_bytes.contains_key(resource_id))
            .fold(0_usize, |total, (_, resource)| total.saturating_add(resource.resolved_origin_uri.len()));
        incoming_uri_bytes.values().fold(retained_uri_bytes, |total, uri_bytes| total.saturating_add(*uri_bytes))
    }

    fn prospective_resource_entry_count(&self, resources: &[TransientResourceRef]) -> usize {
        let incoming_ids = resources.iter().map(|resource| &resource.id).collect::<HashSet<_>>();
        incoming_ids.iter().fold(self.resources.len(), |count, resource_id| {
            count.saturating_add(usize::from(!self.resources.contains_key(*resource_id)))
        })
    }

    fn prospective_finalized_generation_sets<'a>(
        &'a self,
        lifecycle: HlsManifestLifecycle,
        candidate_resource_ids: &'a HashSet<TransientResourceId>,
        reusable_generation: Option<TransientManifestGeneration>,
    ) -> Vec<&'a HashSet<TransientResourceId>> {
        let mut sets = self
            .finalized_manifest_generations
            .iter()
            .filter(|(generation, _)| {
                Some(**generation) == reusable_generation || self.generation_is_lease_referenced(**generation)
            })
            .map(|(_, generation)| generation.resource_ids.as_ref())
            .collect::<Vec<_>>();
        if lifecycle.is_finalized() && reusable_generation.is_none() {
            sets.push(candidate_resource_ids);
        }
        sets
    }

    fn prospective_estimated_metadata_bytes(
        &self,
        candidate: &TransientManifestFootprintCandidate<'_>,
        generation_sets: &[&HashSet<TransientResourceId>],
    ) -> usize {
        let incoming = candidate.resources.iter().map(|resource| (&resource.id, resource)).collect::<HashMap<_, _>>();
        let retained_resource_bytes = self
            .resources
            .iter()
            .filter(|(resource_id, _)| !incoming.contains_key(resource_id))
            .fold(0_usize, |total, (resource_id, resource)| {
                total.saturating_add(estimated_transient_resource_entry_bytes(resource_id, resource))
            });
        let resource_bytes = incoming.values().fold(retained_resource_bytes, |total, resource| {
            total.saturating_add(estimated_transient_resource_entry_bytes(&resource.id, resource))
        });
        let generation_bytes = generation_sets.iter().fold(0_usize, |total, resource_ids| {
            total
                .saturating_add(std::mem::size_of::<RetainedFinalizedManifestGeneration>())
                .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
                .saturating_add(estimated_transient_resource_id_set_bytes(resource_ids))
        });
        let rolling_manifest_resource_bytes = if candidate.lifecycle.is_finalized() {
            0
        } else {
            estimated_transient_resource_id_set_bytes(candidate.resource_ids)
        };
        let retained_template_bytes = self
            .finalized_manifest_generations
            .iter()
            .filter(|(generation, _)| {
                Some(**generation) == candidate.reusable_finalized_generation
                    || self.generation_is_lease_referenced(**generation)
            })
            .filter_map(|(_, generation)| generation.template.as_ref())
            .fold(0_usize, |total, template| total.saturating_add(template.estimated_metadata_bytes()));
        let candidate_template_bytes =
            if candidate.lifecycle.is_finalized() && candidate.reusable_finalized_generation.is_some() {
                0
            } else {
                candidate.template.map_or(0, HlsTransientManifestTemplate::estimated_metadata_bytes)
            };
        let mut protected_ids = HashSet::new();
        for resource_ids in generation_sets {
            protected_ids.extend(resource_ids.iter());
        }
        let protected_index_bytes = protected_ids.iter().fold(0_usize, |total, resource_id| {
            total.saturating_add(
                std::mem::size_of::<(TransientResourceId, u16)>()
                    .saturating_add(resource_id.0.len())
                    .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
            )
        });
        let binding_bytes = self.finalized_manifest_lease_bindings.iter().fold(0_usize, |total, binding| {
            total.saturating_add(
                std::mem::size_of::<TransientManifestLeaseBinding>()
                    .saturating_add(binding.lease_id.0.len())
                    .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
            )
        });
        let object_bytes = self
            .object_cache
            .values()
            .fold(0_usize, |total, entry| total.saturating_add(estimated_transient_object_entry_bytes(entry)));
        candidate
            .rewritten_bytes
            .saturating_add(resource_bytes)
            .saturating_add(generation_bytes)
            .saturating_add(rolling_manifest_resource_bytes)
            .saturating_add(retained_template_bytes)
            .saturating_add(candidate_template_bytes)
            .saturating_add(protected_index_bytes)
            .saturating_add(binding_bytes)
            .saturating_add(object_bytes)
            .saturating_add(
                self.object_fetch_notifiers.len().saturating_mul(
                    std::mem::size_of::<(TransientObjectCacheKey, Arc<Notify>)>()
                        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
                ),
            )
    }

    #[cfg(test)]
    pub(crate) const fn manifest_generation(&self) -> u64 { self.manifest_generation }

    pub const fn current_finalized_manifest_generation(&self) -> Option<TransientManifestGeneration> {
        self.current_finalized_manifest_generation
    }

    pub const fn last_manifest_commit_identity(&self) -> Option<super::media_reserve::HlsManifestCommitIdentity> {
        self.last_manifest_commit_identity
    }

    pub(crate) fn record_manifest_commit_identity(
        &mut self,
        identity: super::media_reserve::HlsManifestCommitIdentity,
    ) {
        self.last_manifest_commit_identity = Some(identity);
    }

    pub(crate) const fn last_manifest_finalized(&self) -> bool { self.last_manifest_validity.is_finalized() }

    pub fn last_manifest_template(&self) -> Option<Arc<HlsTransientManifestTemplate>> {
        self.last_manifest_template.clone()
    }

    pub fn last_manifest_published_resource_ids(&self) -> HlsPublishedTransientResourceIds {
        HlsPublishedTransientResourceIds::from_shared(Arc::clone(&self.last_manifest_resource_ids))
    }

    pub(crate) fn merge_current_published_resource_ids(
        &self,
        previous: &HlsPublishedTransientResourceIds,
        next: HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> HlsPublishedTransientResourceIds {
        if Arc::ptr_eq(&previous.0, &next.0)
            || previous.0.iter().all(|resource_id| {
                next.contains(resource_id)
                    || !self
                        .resources
                        .get(resource_id)
                        .is_some_and(|resource| self.resource_is_valid_at(resource, now_ms))
            })
        {
            return next;
        }
        let mut merged = next.0.as_ref().clone();
        merged.extend(
            previous
                .0
                .iter()
                .filter(|resource_id| {
                    self.resources.get(*resource_id).is_some_and(|resource| self.resource_is_valid_at(resource, now_ms))
                })
                .cloned(),
        );
        HlsPublishedTransientResourceIds(Arc::new(merged))
    }

    pub const fn last_manifest_window_policy(&self) -> HlsManifestWindowPolicy {
        self.last_manifest_validity.window_policy()
    }

    pub(crate) const fn last_manifest_valid_until_ms(&self) -> Option<u64> {
        self.last_manifest_validity.valid_until_ms()
    }

    #[cfg(test)]
    pub(crate) fn current_manifest_resource_ids(&self) -> &HashSet<TransientResourceId> {
        self.last_manifest_resource_ids.as_ref()
    }

    pub(crate) fn has_finalized_manifest_generation(&self, generation: TransientManifestGeneration) -> bool {
        self.finalized_manifest_generations.contains_key(&generation)
    }

    #[cfg(test)]
    pub(crate) fn finalized_manifest_generation_count(&self) -> usize { self.finalized_manifest_generations.len() }

    #[cfg(test)]
    pub(crate) fn finalized_manifest_lease_binding_count(&self) -> usize {
        self.finalized_manifest_lease_bindings.len()
    }

    pub(crate) fn bind_finalized_manifest_generation(&mut self, binding: TransientManifestLeaseBinding) -> bool {
        if !self.finalized_manifest_generations.contains_key(&binding.manifest_generation) {
            return false;
        }
        self.finalized_manifest_lease_bindings.insert(binding);
        self.prune_unreferenced_finalized_manifest_generations();
        true
    }

    pub(crate) fn release_finalized_manifest_generations(
        &mut self,
        lease_id: &HlsAccessLeaseId,
        lease_issued_at_ms: u64,
    ) -> bool {
        let previous_len = self.finalized_manifest_lease_bindings.len();
        self.finalized_manifest_lease_bindings
            .retain(|binding| binding.lease_id != *lease_id || binding.lease_issued_at_ms != lease_issued_at_ms);
        let released = self.finalized_manifest_lease_bindings.len() != previous_len;
        if !released {
            return false;
        }
        self.prune_unreferenced_finalized_manifest_generations();
        true
    }

    pub(crate) fn reconcile_finalized_manifest_lease_bindings(&mut self, bindings: &[TransientManifestLeaseBinding]) {
        self.finalized_manifest_lease_bindings = bindings
            .iter()
            .filter(|binding| self.finalized_manifest_generations.contains_key(&binding.manifest_generation))
            .cloned()
            .collect();
        self.prune_unreferenced_finalized_manifest_generations();
    }

    fn prune_unreferenced_finalized_manifest_generations(&mut self) {
        let current_generation = self.current_finalized_manifest_generation();
        let referenced_generations = self
            .finalized_manifest_lease_bindings
            .iter()
            .map(|binding| binding.manifest_generation)
            .collect::<HashSet<_>>();
        let removed = self
            .finalized_manifest_generations
            .extract_if(|generation, _| {
                Some(*generation) != current_generation && !referenced_generations.contains(generation)
            })
            .map(|(_, generation)| generation)
            .collect::<Vec<_>>();
        for generation in removed {
            self.decrement_protected_resource_refcounts(&generation.resource_ids);
        }
    }

    fn finalized_generation_for_identity(
        &self,
        identity: TransientManifestIdentity,
    ) -> Option<TransientManifestGeneration> {
        let generation = self.current_finalized_manifest_generation?;
        self.finalized_manifest_generations
            .get(&generation)
            .is_some_and(|retained| retained.identity == identity)
            .then_some(generation)
    }

    fn generation_is_lease_referenced(&self, generation: TransientManifestGeneration) -> bool {
        self.finalized_manifest_lease_bindings.iter().any(|binding| binding.manifest_generation == generation)
    }

    fn insert_finalized_manifest_generation(
        &mut self,
        generation: TransientManifestGeneration,
        retained: RetainedFinalizedManifestGeneration,
    ) {
        let resource_ids = Arc::clone(&retained.resource_ids);
        if let Some(previous) = self.finalized_manifest_generations.insert(generation, retained) {
            self.decrement_protected_resource_refcounts(&previous.resource_ids);
        }
        for resource_id in resource_ids.iter() {
            let refcount = self.protected_finalized_resource_refcounts.entry(resource_id.clone()).or_default();
            *refcount = refcount.saturating_add(1);
        }
    }

    fn decrement_protected_resource_refcounts(&mut self, resource_ids: &HashSet<TransientResourceId>) {
        for resource_id in resource_ids {
            let remove = self.protected_finalized_resource_refcounts.get_mut(resource_id).is_some_and(|refcount| {
                *refcount = refcount.saturating_sub(1);
                *refcount == 0
            });
            if remove {
                self.protected_finalized_resource_refcounts.remove(resource_id);
            }
        }
    }

    pub fn upsert_resources<I>(&mut self, resources: I)
    where
        I: IntoIterator<Item = TransientResourceRef>,
    {
        for mut resource in resources {
            let preserve_revision = self
                .resources
                .get(&resource.id)
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
        }
    }

    fn resource_is_valid_at_for(
        &self,
        resource: &TransientResourceRef,
        now_ms: u64,
        scope: TransientResourceValidityScope<'_>,
    ) -> bool {
        match scope {
            TransientResourceValidityScope::PublishedSession => {
                resource.is_valid_at(now_ms) || self.protected_finalized_resource_refcounts.contains_key(&resource.id)
            }
            TransientResourceValidityScope::AccessLease { lease_id, lease_issued_at_ms, published_resource_ids } => {
                (published_resource_ids.contains(&resource.id) && resource.is_valid_at(now_ms))
                    || self.finalized_manifest_lease_bindings.iter().any(|binding| {
                        binding.lease_id == *lease_id
                            && binding.lease_issued_at_ms == lease_issued_at_ms
                            && self
                                .finalized_manifest_generations
                                .get(&binding.manifest_generation)
                                .is_some_and(|generation| generation.resource_ids.contains(&resource.id))
                    })
            }
        }
    }

    pub(crate) fn resource_is_valid_at(&self, resource: &TransientResourceRef, now_ms: u64) -> bool {
        self.resource_is_valid_at_for(resource, now_ms, TransientResourceValidityScope::PublishedSession)
    }

    pub(crate) fn resource_matches_current(&self, resource: &TransientResourceRef, now_ms: u64) -> bool {
        self.resources.get(&resource.id).is_some_and(|current| {
            Arc::ptr_eq(&current.access, &resource.access)
                && current.has_same_cache_identity(resource)
                && self.resource_is_valid_at(current, now_ms)
        })
    }

    pub fn resolve_current_resource(
        &self,
        resource_id: &TransientResourceId,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.resources.get(resource_id).filter(|resource| self.resource_is_valid_at(resource, now_ms)).cloned()
    }

    pub(crate) fn resolve_resource_for_lease(
        &self,
        resource_id: &TransientResourceId,
        lease_id: &HlsAccessLeaseId,
        lease_issued_at_ms: u64,
        published_resource_ids: &HlsPublishedTransientResourceIds,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.resources
            .get(resource_id)
            .filter(|resource| {
                self.resource_is_valid_at_for(
                    resource,
                    now_ms,
                    TransientResourceValidityScope::AccessLease {
                        lease_id,
                        lease_issued_at_ms,
                        published_resource_ids,
                    },
                )
            })
            .cloned()
    }

    pub fn get_valid_resource(
        &mut self,
        resource_id: &TransientResourceId,
        now_ms: u64,
    ) -> Option<TransientResourceRef> {
        self.prune_expired(now_ms);
        self.resolve_current_resource(resource_id, now_ms)
    }

    pub fn prune_expired(&mut self, now_ms: u64) { self.prune_expired_except(now_ms, &HashSet::new()); }

    pub fn prune_expired_except(&mut self, now_ms: u64, protected: &HashSet<TransientResourceId>) {
        let expired_resource_ids = self
            .resources
            .iter()
            .filter(|(id, resource)| {
                !protected.contains(*id)
                    && !self.resource_is_valid_at(resource, now_ms)
                    && resource.active_readers() == 0
            })
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();
        self.resources.retain(|id, _| !expired_resource_ids.contains(id));
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
    ) -> Option<TransientObjectCacheEntry> {
        let binding = {
            let entry = self.object_cache.get(key)?;
            let resource = self.resources.get(key.transient_resource_id())?;
            if resource.kind != resource_kind {
                return None;
            }
            let binding = TransientObjectResourceBinding::from_resource(resource, &entry.binding.file_extension);
            (binding.matches_resource_identity(resource) && self.resource_is_valid_at(resource, now_ms))
                .then_some(binding)?
        };
        let entry = self.object_cache.get_mut(key)?;
        if !matches!(entry.status, TransientObjectCacheStatus::Ready { .. })
            || entry.binding != binding
            || (resource_kind == TransientResourceKind::Key && entry.ready_content_length() != Some(16))
            || entry.expires_at_ms < now_ms
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
        (current.has_same_cache_identity(resource)
            && binding.matches_resource_identity(current)
            && self.resource_is_valid_at(current, now_ms))
        .then_some(binding)
    }

    fn binding_is_current(&self, binding: &TransientObjectResourceBinding, now_ms: u64) -> bool {
        self.resources.get(&binding.resource_id).is_some_and(|resource| {
            binding.matches_resource_identity(resource) && self.resource_is_valid_at(resource, now_ms)
        })
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
        let expires_at_ms = transient_object_expires_at(now_ms, cache_duration_ms);
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

    pub fn object_fetch_token_matches(&self, token: &TransientObjectFetchToken) -> bool {
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

    pub fn mark_object_ready_if_current(
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

    pub fn mark_object_failed_retryable_if_current(
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

    pub fn mark_object_failed_permanent_if_current(
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

    /// Returns the finite READY lifetime of an AES key dependency without touching access accounting.
    pub fn ready_key_object_valid_until_ms(
        &self,
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
        file_ext: &str,
        now_ms: u64,
    ) -> Option<u64> {
        let resource = self.resources.get(resource_id)?;
        if resource.kind != TransientResourceKind::Key
            || resource.file_ext_hint.as_deref() != Some(file_ext)
            || !self.resource_is_valid_at(resource, now_ms)
        {
            return None;
        }
        let key = Self::transient_object_key(proxy_session_id, resource_id, file_ext.to_string());
        let entry = self.object_cache.get(&key)?;
        let binding = TransientObjectResourceBinding::from_resource(resource, file_ext);
        let valid_aes128_key = matches!(entry.status, TransientObjectCacheStatus::Ready { content_length: 16, .. });
        let valid_until_ms = if self.protected_finalized_resource_refcounts.contains_key(resource_id) {
            entry.expires_at_ms
        } else {
            entry.expires_at_ms.min(resource.expires_at_ms)
        };
        (valid_aes128_key && entry.binding == binding && entry.is_ready_at(now_ms)).then_some(valid_until_ms)
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

    pub fn take_expired_object_removals_except(
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
    pub fn commit_transient_object_ready_if_current(
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

    pub fn fail_transient_object_retryable_if_current(
        &mut self,
        token: &TransientObjectFetchToken,
        now_ms: u64,
        retry_after_ms: u64,
    ) -> bool {
        self.transient.mark_object_failed_retryable_if_current(token, now_ms, retry_after_ms)
    }

    pub fn fail_transient_object_permanent_if_current(
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
            .field("manifest_generation", &self.manifest_generation)
            .field("last_manifest_commit_identity", &self.last_manifest_commit_identity)
            .field("current_finalized_manifest_generation", &self.current_finalized_manifest_generation)
            .field("last_manifest_validity", &self.last_manifest_validity)
            .field("last_manifest_resource_ids_len", &self.last_manifest_resource_ids.len())
            .field(
                "rolling_event_origin_body_len",
                &self.rolling_event_rewrite_seed.as_ref().map(|seed| seed.origin_body.len()),
            )
            .field("finalized_manifest_generations_len", &self.finalized_manifest_generations.len())
            .field("finalized_manifest_lease_bindings_len", &self.finalized_manifest_lease_bindings.len())
            .field("protected_finalized_resource_refcounts_len", &self.protected_finalized_resource_refcounts.len())
            .field("last_manifest_body_len", &self.last_manifest_body.as_ref().map(|body| body.len()))
            .field(
                "last_manifest_template",
                &self.last_manifest_template.as_ref().map(|template| template.estimated_metadata_bytes()),
            )
            .field("last_manifest_rendered_at_ms", &self.last_manifest_rendered_at_ms)
            .field("last_manifest_playlist_duration_ms", &self.last_manifest_playlist_duration_ms)
            .field("resource_ttl_ms", &self.resource_ttl_ms)
            .finish()
    }
}

fn transient_manifest_identity(body: &str, resource_ids: &HashSet<TransientResourceId>) -> TransientManifestIdentity {
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

fn complete_manifest_append_boundary(body: &str) -> bool {
    body.ends_with('\n')
        && body.lines().rev().map(str::trim).find(|line| !line.is_empty()).is_some_and(|line| !line.starts_with('#'))
}

fn estimated_transient_resource_entry_bytes(
    resource_id: &TransientResourceId,
    resource: &TransientResourceRef,
) -> usize {
    std::mem::size_of::<(TransientResourceId, TransientResourceRef)>()
        .saturating_add(resource_id.0.len())
        .saturating_add(resource.id.0.len())
        .saturating_add(resource.resolved_origin_uri.len())
        .saturating_add(resource.content_type_hint.as_ref().map_or(0, String::len))
        .saturating_add(resource.file_ext_hint.as_ref().map_or(0, String::len))
        .saturating_add(std::mem::size_of::<CacheAccessState>())
        .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES)
}

fn estimated_transient_resource_id_set_bytes(resource_ids: &HashSet<TransientResourceId>) -> usize {
    resource_ids.iter().fold(0_usize, |total, resource_id| {
        total.saturating_add(
            std::mem::size_of::<TransientResourceId>()
                .saturating_add(resource_id.0.len())
                .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES),
        )
    })
}

fn estimated_transient_object_entry_bytes(entry: &TransientObjectCacheEntry) -> usize {
    std::mem::size_of::<(TransientObjectCacheKey, TransientObjectCacheEntry)>()
        .saturating_add(entry.key.stable_value().len())
        .saturating_add(entry.content_type.len())
        .saturating_add(entry.binding.resource_id.0.len())
        .saturating_add(entry.binding.resolved_origin_uri.len())
        .saturating_add(entry.binding.file_extension.len())
        .saturating_add(std::mem::size_of::<CacheAccessState>())
        .saturating_add(ESTIMATED_ARC_ALLOCATION_OVERHEAD_BYTES)
        .saturating_add(ESTIMATED_HASH_ENTRY_OVERHEAD_BYTES)
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
        build_transient_resource_id, extract_transient_resource_ids, transient_object_expires_at,
        HlsPublishedTransientResourceIds, HlsTransientManifestCommitError, TransientManifestLeaseBinding,
        TransientObjectCacheStatus, TransientPassthroughState, TransientResourceId, TransientResourceKind,
        TransientResourceRef, MAX_FAILED_TRANSIENT_OBJECT_ENTRIES,
    };
    use crate::{
        manifest_limits::{
            HlsManifestLimitKind, HlsManifestLimitViolation, MAX_ESTIMATED_TRANSIENT_METADATA_BYTES,
            MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS, MAX_TRANSIENT_GENERATION_MEMBERSHIPS,
            MAX_TRANSIENT_MANIFEST_RESOURCES, MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION,
            MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION, MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES,
        },
        transient_manifest::{TransientManifestRewriter, TransientRewriteResult},
        HlsAccessLeaseId, ProxySessionId,
    };
    use std::{collections::HashSet, fmt::Write as _, sync::Arc, task::Poll};
    use tuliprox_parser::hls::origin_manifest::{
        parse_manifest_semantics, HlsManifestLifecycle, HlsManifestWindowPolicy,
    };

    fn local_representation_limit(error: HlsTransientManifestCommitError) -> HlsManifestLimitViolation {
        let HlsTransientManifestCommitError::LocalRepresentationLimit(violation) = error else {
            panic!("expected local representation limit, got {error:?}");
        };
        violation
    }

    #[test]
    fn extract_transient_resource_ids_ignores_comment_and_foreign_uri_surfaces() {
        let body = concat!(
            "# /r/fake.ts\n",
            "https://origin.example.com/r/fake.ts\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://origin.example.com/r/fake.key\"\n",
        );

        assert!(extract_transient_resource_ids(body).is_empty());
    }

    #[test]
    fn extract_transient_resource_ids_ignores_query_values() {
        let body = concat!(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.key?token=/r/fake.ts\"\n",
            "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.ts?redirect=/r/foreign.ts\n",
        );

        let resource_ids = extract_transient_resource_ids(body);

        assert_eq!(resource_ids.len(), 1);
        assert!(resource_ids.contains(&TransientResourceId("AbCdEfGhIjKlMnOp".to_string())));
    }

    #[test]
    fn extract_transient_resource_ids_rejects_noncanonical_resource_files() {
        let body = concat!(
            "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.exe\n",
            "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.ts/extra\n",
            "/hls/shared/live/proxy/lease/r/AbCdEfGhIjKlMnOp.tar.gz\n",
        );

        assert!(extract_transient_resource_ids(body).is_empty());
    }

    #[test]
    fn extract_transient_resource_ids_keeps_segment_key_and_map_routes() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy/__hls_access_lease_id__/r/Key0000000000Id.key\"\n",
            "#EXT-X-MAP:URI=\"/hls/shared/live/proxy/__hls_access_lease_id__/r/Map0000000000Id.mp4\"\n",
            "#EXTINF:1.0,\n",
            "/hls/shared/live/proxy/lease/r/Seg0000000000Id.ts\n",
        );

        let resource_ids = extract_transient_resource_ids(body);

        assert_eq!(resource_ids.len(), 3);
        assert!(resource_ids.contains(&TransientResourceId("Key0000000000Id".to_string())));
        assert!(resource_ids.contains(&TransientResourceId("Map0000000000Id".to_string())));
        assert!(resource_ids.contains(&TransientResourceId("Seg0000000000Id".to_string())));
    }

    fn finalized_manifest_body(resource_id: &TransientResourceId, extension: &str) -> String {
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.{extension}\n#EXT-X-ENDLIST\n",
            resource_id.0
        )
    }

    fn replace_with_finalized_manifest(
        state: &mut TransientPassthroughState,
        resource_id: &TransientResourceId,
        extension: &str,
        rendered_at_ms: u64,
    ) {
        state.replace_manifest_with_semantics(
            finalized_manifest_body(resource_id, extension),
            rendered_at_ms,
            Some(1_000),
        );
    }

    #[test]
    fn finalized_committed_manifest_has_full_window_and_no_deadline() {
        let mut state = TransientPassthroughState::default();

        state.replace_manifest_with_semantics("#EXTM3U\n#EXT-X-ENDLIST\n".to_string(), 100, Some(1_000));

        assert!(state.last_manifest_finalized());
        assert_eq!(state.last_manifest_window_policy(), HlsManifestWindowPolicy::PreserveFullManifest);
        assert_eq!(state.last_manifest_valid_until_ms(), None);
    }

    #[test]
    fn rolling_typed_committed_manifest_preserves_full_window_with_deadline() {
        let mut state = TransientPassthroughState::default();

        state.replace_manifest_with_semantics(
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:1,\n/r/resource.ts\n".to_string(),
            100,
            Some(1_000),
        );

        assert!(!state.last_manifest_finalized());
        assert_eq!(state.last_manifest_window_policy(), HlsManifestWindowPolicy::PreserveFullManifest);
        assert_eq!(state.last_manifest_valid_until_ms(), Some(1_100));
    }

    fn rewritten_finalized_manifest(segment_count: usize, identity: &str) -> TransientRewriteResult {
        let mut body =
            String::from("#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n");
        for index in 0..segment_count {
            writeln!(body, "#EXTINF:6,\n{identity}-{index}.ts").expect("synthetic manifest renders");
        }
        body.push_str("#EXT-X-ENDLIST\n");
        TransientManifestRewriter::rewrite(
            &body,
            "https://origin.example/archive/index.m3u8",
            &ProxySessionId("proxy-session".to_string()),
            b"secret",
            0,
            300_000,
        )
    }

    #[test]
    fn transient_manifest_resource_limit_rejects_without_mutating_current_manifest() {
        let mut state = TransientPassthroughState::default();
        let baseline = rewritten_finalized_manifest(1, "baseline");
        let baseline_semantics = parse_manifest_semantics(&baseline.body);
        state
            .commit_rewritten_manifest_with_semantics(
                baseline.body,
                baseline.resources,
                1,
                Some(6_000),
                baseline_semantics,
            )
            .expect("baseline manifest commits");
        let baseline_body = state.last_manifest_body.clone().expect("baseline body");
        let baseline_generation = state.manifest_generation();
        let baseline_resource_count = state.resources.len();
        let oversized = rewritten_finalized_manifest(MAX_TRANSIENT_MANIFEST_RESOURCES + 1, "oversized");
        let oversized_semantics = parse_manifest_semantics(&oversized.body);

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    oversized.body,
                    oversized.resources,
                    2,
                    None,
                    oversized_semantics,
                )
                .expect_err("resource overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientResources);
        assert_eq!(violation.actual, MAX_TRANSIENT_MANIFEST_RESOURCES + 1);
        assert_eq!(state.manifest_generation(), baseline_generation);
        assert_eq!(state.resources.len(), baseline_resource_count);
        assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("current body remains"), &baseline_body));
    }

    #[test]
    fn transient_rewritten_body_limit_rejects_before_commit() {
        let mut state = TransientPassthroughState::default();
        let oversized_body = "x".repeat(MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES + 1);

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    oversized_body,
                    Vec::new(),
                    1,
                    None,
                    parse_manifest_semantics("#EXTM3U\n"),
                )
                .expect_err("rewritten body overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientRewrittenBytes);
        assert!(state.last_manifest_body.is_none());
        assert!(state.resources.is_empty());
    }

    #[test]
    fn transient_session_origin_uri_byte_limit_is_enforced() {
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "x".repeat(MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION + 1),
            b"secret",
            0,
            300_000,
            Some("ts".to_string()),
        );

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    finalized_manifest_body(&resource.id, "ts"),
                    vec![resource],
                    1,
                    Some(1_000),
                    parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
                )
                .expect_err("origin URI byte overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientOriginUriBytes);
        assert!(state.resources.is_empty());
    }

    #[test]
    fn prospective_origin_uri_bytes_counts_duplicate_resource_ids_once() {
        let state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://origin.example/archive/segment.ts",
            b"secret",
            0,
            300_000,
            Some("ts".to_string()),
        );

        assert_eq!(
            state.prospective_origin_uri_bytes(&[resource.clone(), resource.clone()]),
            resource.resolved_origin_uri.len()
        );
    }

    #[test]
    fn retained_finalized_generation_limit_rejects_the_next_generation() {
        let mut state = TransientPassthroughState::default();
        for index in 0..MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS {
            let rewritten = rewritten_finalized_manifest(1, &format!("generation-{index}"));
            let semantics = parse_manifest_semantics(&rewritten.body);
            state
                .commit_rewritten_manifest_with_semantics(
                    rewritten.body,
                    rewritten.resources,
                    u64::try_from(index).expect("test index fits u64"),
                    Some(6_000),
                    semantics,
                )
                .expect("generation within retention limit commits");
            let generation = state.current_finalized_manifest_generation().expect("finalized generation");
            assert!(state.bind_finalized_manifest_generation(TransientManifestLeaseBinding::new(
                HlsAccessLeaseId(format!("lease-{index}")),
                u64::try_from(index).expect("test index fits u64"),
                generation,
            )));
        }
        let retained_body = state.last_manifest_body.clone().expect("retained current body");
        let retained_generation = state.manifest_generation();
        let overflow = rewritten_finalized_manifest(1, "generation-overflow");
        let semantics = parse_manifest_semantics(&overflow.body);

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    overflow.body,
                    overflow.resources,
                    100,
                    Some(6_000),
                    semantics,
                )
                .expect_err("additional retained generation is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::FinalizedGenerations);
        assert_eq!(violation.actual, MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS + 1);
        assert_eq!(state.manifest_generation(), retained_generation);
        assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("current body remains"), &retained_body));
    }

    #[test]
    fn byte_identical_finalized_refresh_reuses_generation_and_body_allocation() {
        let mut state = TransientPassthroughState::default();
        let first = rewritten_finalized_manifest(3, "stable");
        let semantics = parse_manifest_semantics(&first.body);
        state
            .commit_rewritten_manifest_with_semantics(first.body, first.resources, 0, Some(18_000), semantics)
            .expect("first finalized manifest commits");
        let generation = state.current_finalized_manifest_generation().expect("finalized generation");
        let generation_highwater = state.manifest_generation();
        let body = state.last_manifest_body.clone().expect("stored body");

        let refreshed = TransientManifestRewriter::rewrite(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:6,\nstable-0.ts\n#EXTINF:6,\nstable-1.ts\n#EXTINF:6,\nstable-2.ts\n#EXT-X-ENDLIST\n",
            "https://origin.example/archive/index.m3u8",
            &ProxySessionId("proxy-session".to_string()),
            b"secret",
            400_000,
            300_000,
        );
        let refreshed_resource_id = refreshed.resources[0].id.clone();
        state
            .commit_rewritten_manifest_with_semantics(
                refreshed.body,
                refreshed.resources,
                400_000,
                Some(18_000),
                semantics,
            )
            .expect("identical finalized refresh commits");

        assert_eq!(state.current_finalized_manifest_generation(), Some(generation));
        assert_eq!(state.manifest_generation(), generation_highwater);
        assert_eq!(state.finalized_manifest_generation_count(), 1);
        assert!(Arc::ptr_eq(state.last_manifest_body.as_ref().expect("same body allocation"), &body));
        assert_eq!(state.resources[&refreshed_resource_id].expires_at_ms, 700_000);
    }

    #[test]
    fn rolling_event_append_requires_current_rewrite_secret_and_refreshes_existing_mapping_validity() {
        let mut state = TransientPassthroughState::default();
        let first_origin = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:EVENT\n\
                            #EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:6,\n0.ts\n#EXTINF:6,\n1.ts\n";
        let first = TransientManifestRewriter::rewrite(
            first_origin,
            "https://origin.example/event/index.m3u8",
            &ProxySessionId("proxy-session".to_string()),
            b"secret",
            0,
            300_000,
        );
        let checkpoint = first.checkpoint.clone();
        let first_resource_id = first.resources[0].id.clone();
        let semantics = parse_manifest_semantics(first_origin);
        let footprint = state
            .commit_rewritten_manifest_with_semantics(first.body, first.resources, 0, Some(12_000), semantics)
            .expect("initial EVENT commits");
        state.record_rolling_event_rewrite_seed(
            first_origin,
            "https://origin.example/event/index.m3u8",
            b"secret",
            checkpoint,
            footprint.estimated_metadata_bytes,
            true,
        );
        let previous_template = state.last_manifest_template().expect("initial template");
        let second_origin = format!("{first_origin}#EXTINF:6,\n2.ts\n");
        assert!(state
            .rolling_event_append_context(&second_origin, "https://origin.example/event/index.m3u8", b"rotated-secret",)
            .is_none());
        let context = state
            .rolling_event_append_context(&second_origin, "https://origin.example/event/index.m3u8", b"secret")
            .expect("safe append is detected");
        let mut appended = TransientManifestRewriter::rewrite_append(
            &second_origin[context.suffix_offset..],
            "https://origin.example/event/index.m3u8",
            &ProxySessionId("proxy-session".to_string()),
            b"secret",
            400_000,
            300_000,
            context.checkpoint,
        );
        let template = TransientPassthroughState::extend_rolling_event_template(&context.template, &appended.body)
            .expect("append remains within limits")
            .expect("append contains a complete media unit");
        let mut body = String::with_capacity(context.rewritten_prefix.len().saturating_add(appended.body.len()));
        body.push_str(&context.rewritten_prefix);
        body.push_str(&appended.body);
        appended.body = body;

        state
            .commit_incremental_rewritten_manifest_with_semantics(
                appended.body,
                appended.resources,
                Arc::clone(&template),
                400_000,
                300_000,
                parse_manifest_semantics(&second_origin),
            )
            .expect("append commits");

        assert_eq!(template.segments().len(), 3);
        assert!(Arc::ptr_eq(&previous_template.segments()[0].uri, &template.segments()[0].uri));
        assert_eq!(state.resources[&first_resource_id].expires_at_ms, 700_000);
        assert_eq!(state.current_manifest_resource_ids().len(), 3);
    }

    #[test]
    fn rolling_lease_membership_retains_only_still_valid_previously_published_resources() {
        let mut state = TransientPassthroughState::default();
        let previous_resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://origin.example/previous.ts",
            b"secret",
            0,
            20,
            Some("ts".to_string()),
        );
        let next_resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://origin.example/next.ts",
            b"secret",
            0,
            40,
            Some("ts".to_string()),
        );
        let previous_resource_id = previous_resource.id.clone();
        let next_resource_id = next_resource.id.clone();
        state.upsert_resources([previous_resource, next_resource]);
        let previous = HlsPublishedTransientResourceIds::from_manifest_body(&format!(
            "/hls/shared/live/proxy/lease/r/{}.ts",
            previous_resource_id.0
        ));
        let next = HlsPublishedTransientResourceIds::from_manifest_body(&format!(
            "/hls/shared/live/proxy/lease/r/{}.ts",
            next_resource_id.0
        ));

        let retained = state.merge_current_published_resource_ids(&previous, next.clone(), 10);
        assert!(retained.contains(&previous_resource_id));
        assert!(retained.contains(&next_resource_id));

        let pruned = state.merge_current_published_resource_ids(&retained, next, 21);
        assert!(!pruned.contains(&previous_resource_id));
        assert!(pruned.contains(&next_resource_id));
    }

    #[test]
    fn transient_session_resource_entry_limit_is_enforced() {
        let mut state = TransientPassthroughState::default();
        for index in 0..MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION {
            state.upsert_resources([TransientResourceRef::new(
                TransientResourceKind::Segment,
                format!("https://origin.example/{index}.ts"),
                b"secret",
                0,
                300_000,
                Some("ts".to_string()),
            )]);
        }
        let incoming = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://other-origin.example/overflow.ts".to_string(),
            b"secret",
            0,
            300_000,
            Some("ts".to_string()),
        );

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    finalized_manifest_body(&incoming.id, "ts"),
                    vec![incoming],
                    1,
                    Some(1_000),
                    parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
                )
                .expect_err("resource entry overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientResourceEntries);
        assert_eq!(violation.actual, MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION + 1);
    }

    #[test]
    fn finalized_generation_membership_limit_counts_overlapping_sets() {
        let mut state = TransientPassthroughState::default();
        let base = rewritten_finalized_manifest(MAX_TRANSIENT_MANIFEST_RESOURCES, "shared");
        let mut body = base.body;
        let resources = base.resources;
        let generations_within_limit = MAX_TRANSIENT_GENERATION_MEMBERSHIPS / MAX_TRANSIENT_MANIFEST_RESOURCES;
        for index in 0..generations_within_limit {
            let _ = writeln!(body, "# generation-{index}");
            state
                .commit_rewritten_manifest_with_semantics(
                    body.clone(),
                    if index == 0 { resources.clone() } else { Vec::new() },
                    u64::try_from(index).expect("index fits u64"),
                    None,
                    parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
                )
                .expect("membership count remains within limit");
            let generation = state.current_finalized_manifest_generation().expect("finalized generation");
            assert!(state.bind_finalized_manifest_generation(TransientManifestLeaseBinding::new(
                HlsAccessLeaseId(format!("lease-{index}")),
                u64::try_from(index).expect("index fits u64"),
                generation,
            )));
        }
        body.push_str("# generation-overflow\n");

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    body,
                    Vec::new(),
                    100,
                    None,
                    parse_manifest_semantics("#EXTM3U\n#EXT-X-ENDLIST\n"),
                )
                .expect_err("membership overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientGenerationMemberships);
        assert_eq!(violation.actual, MAX_TRANSIENT_GENERATION_MEMBERSHIPS + MAX_TRANSIENT_MANIFEST_RESOURCES);
    }

    #[test]
    fn estimated_transient_metadata_limit_counts_owned_hint_bytes() {
        let mut state = TransientPassthroughState::default();
        let mut retained = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "https://origin.example/retained.ts".to_string(),
            b"secret",
            0,
            300_000,
            Some("ts".to_string()),
        );
        retained.content_type_hint = Some("x".repeat(MAX_ESTIMATED_TRANSIENT_METADATA_BYTES));
        state.upsert_resources([retained]);
        let candidate = rewritten_finalized_manifest(1, "candidate");
        let semantics = parse_manifest_semantics(&candidate.body);

        let violation = local_representation_limit(
            state
                .commit_rewritten_manifest_with_semantics(
                    candidate.body,
                    candidate.resources,
                    1,
                    Some(6_000),
                    semantics,
                )
                .expect_err("estimated metadata overflow is rejected"),
        );

        assert_eq!(violation.kind, HlsManifestLimitKind::TransientEstimatedMetadataBytes);
        assert!(violation.actual > MAX_ESTIMATED_TRANSIENT_METADATA_BYTES);
    }

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
    fn manifest_refresh_does_not_extend_transient_ready_object_ttl() {
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
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
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
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 100);

        let object = state.object_cache.get(&key).expect("object remains");
        assert!(matches!(object.status, TransientObjectCacheStatus::Ready { .. }));
        assert_eq!(object.expires_at_ms, 110);
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

        assert_eq!(state.object_cache.get(&token.lookup_key).map(|entry| entry.expires_at_ms), Some(70));
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
    fn finalized_key_readiness_uses_object_ttl_after_mapping_ttl() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/archive/key.key",
            b"secret",
            0,
            20,
            Some("key".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource.clone()]);
        replace_with_finalized_manifest(&mut state, &resource_id, "key", 0);
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "key", 10, 100) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("new finalized AES key starts a cache fetch")
            }
        };
        assert!(state.mark_object_ready_if_current(&token, "application/octet-stream".to_string(), 16, 15, 110));

        assert_eq!(state.ready_key_object_valid_until_ms(&proxy_session_id, &resource_id, "key", 30), Some(110));
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
            state.object_cache.get(&second.lookup_key).map(|entry| &entry.status),
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
        state.replace_manifest_with_semantics(
            format!("#EXTM3U\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.ts\n", resource_id.0),
            10,
            None,
        );
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
    fn finalized_manifest_resource_accepts_late_fetch_commit() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/archive/short.ts",
            b"secret",
            10,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
        let resource = state.resolve_current_resource(&resource_id, 15).expect("finalized resource resolves");
        let token = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 15, 100) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("valid finalized resource starts a cache fill")
            }
        };

        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 18);
        assert_eq!(state.manifest_generation(), 1);
        assert!(state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 121));
        assert!(state.object_cache.contains_key(&token.lookup_key));
    }

    #[test]
    fn finalized_resource_replacement_still_rejects_stale_fetch_commit() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let original = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/archive/original.ts",
            b"secret",
            10,
            10,
            Some("ts".to_string()),
        );
        let resource_id = original.id.clone();
        state.upsert_resources([original]);
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 10);
        let original = state.resolve_current_resource(&resource_id, 15).expect("original mapping");
        let token = match state.begin_object_fetch(&proxy_session_id, &original, "ts", 15, 100) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("original finalized resource starts a cache fill")
            }
        };
        let mut replacement = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://replacement.example.com/archive/replacement.ts",
            b"secret",
            16,
            100,
            Some("ts".to_string()),
        );
        replacement.id = resource_id.clone();
        state.upsert_resources([replacement]);
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 16);

        assert!(!state.mark_object_ready_if_current(&token, "video/mp2t".to_string(), 7, 21, 121));
        assert!(!state.object_cache.contains_key(&token.lookup_key));
    }

    #[test]
    fn expired_finalized_object_is_refetched_instead_of_served_stale() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut state = TransientPassthroughState::default();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/archive/refetch.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        state.upsert_resources([resource]);
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 0);
        let resource = state.resolve_current_resource(&resource_id, 5).expect("finalized mapping");
        let first = match state.begin_object_fetch(&proxy_session_id, &resource, "ts", 5, 5) {
            super::TransientObjectFetchDecision::Fetch(token) => token,
            super::TransientObjectFetchDecision::Ready | super::TransientObjectFetchDecision::Wait(_) => {
                panic!("first cache fill starts")
            }
        };
        assert!(state.mark_object_ready_if_current(
            &first,
            "video/mp2t".to_string(),
            7,
            6,
            transient_object_expires_at(6, 5),
        ));
        let lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");

        assert!(state.ready_object(&lookup_key, TransientResourceKind::Segment, 11).is_some());
        assert!(state.ready_object(&lookup_key, TransientResourceKind::Segment, 12).is_none());
        let resource = state.resolve_current_resource(&resource_id, 12).expect("mapping outlives cached bytes");
        assert!(matches!(
            state.begin_object_fetch(&proxy_session_id, &resource, "ts", 12, 5),
            super::TransientObjectFetchDecision::Fetch(_)
        ));
    }

    #[test]
    fn finalized_catchup_has_no_resource_boundary_at_cache_duration() {
        let proxy_session_id = ProxySessionId("proxy-session".to_string());
        let mut origin_body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-PLAYLIST-TYPE:EVENT\n");
        let durations_ms = (0..46)
            .map(|index| match index {
                0..44 => 6_700,
                44 => 6_720,
                _ => 6_000,
            })
            .collect::<Vec<_>>();
        for (index, duration_ms) in durations_ms.iter().enumerate() {
            writeln!(origin_body, "#EXTINF:{}.{:03},\n{index}.ts", duration_ms / 1_000, duration_ms % 1_000)
                .expect("synthetic manifest renders");
        }
        origin_body.push_str("#EXT-X-ENDLIST\n");
        assert_eq!(durations_ms.iter().take(45).sum::<u64>(), 301_520);
        let lifecycle = parse_manifest_semantics(&origin_body).lifecycle();
        assert_eq!(lifecycle, HlsManifestLifecycle::Finalized);
        let rewritten = TransientManifestRewriter::rewrite(
            &origin_body,
            "http://origin.example.com/archive/index.m3u8",
            &proxy_session_id,
            b"secret",
            0,
            300_000,
        );
        let before_boundary = rewritten
            .resources
            .iter()
            .find(|resource| resource.resolved_origin_uri.ends_with("/44.ts"))
            .expect("segment before boundary")
            .id
            .clone();
        let after_boundary = rewritten
            .resources
            .iter()
            .find(|resource| resource.resolved_origin_uri.ends_with("/45.ts"))
            .expect("segment after boundary")
            .id
            .clone();
        let mut state = TransientPassthroughState::default();
        state.upsert_resources(rewritten.resources);
        state.replace_manifest_with_semantics(rewritten.body, 0, Some(durations_ms.iter().sum()));

        assert!(state.resolve_current_resource(&before_boundary, 301_520).is_some());
        let after = state
            .resolve_current_resource(&after_boundary, 301_520)
            .expect("segment after cache-duration boundary remains resolvable");
        assert!(matches!(
            state.begin_object_fetch(&proxy_session_id, &after, "ts", 301_520, 300_000),
            super::TransientObjectFetchDecision::Fetch(_)
        ));
    }

    #[test]
    fn finalized_prune_keeps_resources_referenced_by_current_manifest() {
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
        replace_with_finalized_manifest(&mut state, &resource_id, "ts", 0);

        state.prune_expired(20);

        assert!(state.resources.contains_key(&resource_id));
    }
}
