use std::fmt;

pub const MAX_HLS_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
/// Bounds one rewritten provider body before per-session resource metadata is retained.
pub(crate) const MAX_TRANSIENT_MANIFEST_RESOURCES: usize = 8_192;
pub(crate) const MAX_TRANSIENT_REWRITTEN_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_TRANSIENT_ORIGIN_URI_BYTES_PER_SESSION: usize = 8 * 1024 * 1024;
/// Prevents token rotation across retained generations from growing the session resource map without bound.
pub(crate) const MAX_TRANSIENT_RESOURCE_ENTRIES_PER_SESSION: usize = 32_768;
/// Counts every resource-ID membership across retained finalized generations, including overlapping generations.
pub(crate) const MAX_TRANSIENT_GENERATION_MEMBERSHIPS: usize = 65_536;
/// Conservative structural estimate covering maps, sets, IDs, strings, bindings and the current rewritten body.
pub(crate) const MAX_ESTIMATED_TRANSIENT_METADATA_BYTES: usize = 32 * 1024 * 1024;
/// Bounds old immutable locator sets retained for already-published access-lease manifests.
pub(crate) const MAX_RETAINED_FINALIZED_MANIFEST_GENERATIONS: usize = 16;
/// Caps the shared parsed template and every normal lease-local snapshot.
/// A six-second append-only EVENT reaches this controlled rejection boundary after about 13.6 hours;
/// the proxy never truncates the finite/event body to stay below the bound.
pub(crate) const MAX_HLS_LEASE_SNAPSHOT_SEGMENTS: usize = 8_192;
pub(crate) const MAX_HLS_LEASE_SNAPSHOT_URI_BYTES: usize = 2 * 1024 * 1024;
/// Rejects an individual untrusted key declaration before allocating its attribute strings.
pub(crate) const MAX_HLS_ENCRYPTION_DIRECTIVE_BYTES: usize = 16 * 1024;
/// Bounds aggregate segment, URI and encryption metadata represented by one parsed snapshot template.
pub(crate) const MAX_HLS_LEASE_SNAPSHOT_METADATA_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsManifestLimitKind {
    TransientResources,
    TransientRewrittenBytes,
    TransientOriginUriBytes,
    TransientResourceEntries,
    TransientGenerationMemberships,
    TransientEstimatedMetadataBytes,
    ManifestCommitGeneration,
    FinalizedGenerations,
    LeaseSnapshotSegments,
    LeaseSnapshotUriBytes,
    LeaseSnapshotEncryptionDirectiveBytes,
    LeaseSnapshotMetadataBytes,
}

impl HlsManifestLimitKind {
    pub const fn as_log_value(self) -> &'static str {
        match self {
            Self::TransientResources => "transient-resources",
            Self::TransientRewrittenBytes => "transient-rewritten-bytes",
            Self::TransientOriginUriBytes => "transient-origin-uri-bytes",
            Self::TransientResourceEntries => "transient-resource-entries",
            Self::TransientGenerationMemberships => "transient-generation-memberships",
            Self::TransientEstimatedMetadataBytes => "transient-estimated-metadata-bytes",
            Self::ManifestCommitGeneration => "manifest-commit-generation",
            Self::FinalizedGenerations => "finalized-generations",
            Self::LeaseSnapshotSegments => "lease-snapshot-segments",
            Self::LeaseSnapshotUriBytes => "lease-snapshot-uri-bytes",
            Self::LeaseSnapshotEncryptionDirectiveBytes => "lease-snapshot-encryption-directive-bytes",
            Self::LeaseSnapshotMetadataBytes => "lease-snapshot-metadata-bytes",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HlsManifestLimitViolation {
    pub kind: HlsManifestLimitKind,
    pub actual: usize,
    pub limit: usize,
}

impl HlsManifestLimitViolation {
    pub const fn new(kind: HlsManifestLimitKind, actual: usize, limit: usize) -> Self { Self { kind, actual, limit } }
}

impl fmt::Display for HlsManifestLimitViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} actual={} limit={}", self.kind.as_log_value(), self.actual, self.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_HLS_LEASE_SNAPSHOT_SEGMENTS, MAX_TRANSIENT_MANIFEST_RESOURCES};

    #[test]
    fn finalized_catchup_example_fits_representation_limits() {
        assert!(MAX_TRANSIENT_MANIFEST_RESOURCES >= 1_643);
        assert!(MAX_HLS_LEASE_SNAPSHOT_SEGMENTS >= 1_643);
    }
}
