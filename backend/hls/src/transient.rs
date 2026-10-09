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
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;
use tuliprox_parser::hls::origin_manifest::{HlsManifestLifecycle, ParsedManifestSemantics};

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

/// Monotonic identity of one committed transient manifest body.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TransientManifestGeneration(u64);

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

/// Generation-safe ownership of one transient full-object cache fill.
#[derive(Clone)]
pub struct TransientObjectFetchToken {
    lookup_key: TransientObjectCacheKey,
    cache_key: TransientObjectCacheKey,
    entry_access: Arc<CacheAccessState>,
    binding: TransientObjectResourceBinding,
    superseded_object: Option<TransientObjectRemoval>,
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
    pub fn has_active_resource_readers(&self) -> bool { self.active_resource_readers() > 0 }
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
mod tests;

mod generations;
mod identity;
mod manifests;
mod objects;
mod resources;
use identity::CommittedTransientManifestValidity;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use identity::{
    build_transient_resource_id, TransientObjectCacheStatus, TransientObjectFetchDecision, TransientObjectRemoval,
    TransientObjectUnavailableState, TransientResourceKind,
};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub(crate) use identity::{
    extract_transient_resource_ids, HlsTransientManifestCommitError, HlsTransientManifestFootprint,
    RollingEventAppendContext, TransientManifestLeaseBinding,
};
